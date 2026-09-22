//! Shared process runtime: one Market/Portfolio/OMS/Risk/Paper for pipeline + control API.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use tux_core::domain::{Balance, Fill, Instrument, OrderIntent};
use tux_core::error::{Result, TuxError};
use tux_core::events::InMemoryEventBus;
use tux_core::ids::{AccountId, StrategyInstanceId};
use tux_core::market::MarketState;
use tux_core::oms::Oms;
use tux_core::portfolio::Portfolio;
use tux_core::risk::RiskEngine;
use tux_core::sim::PaperEngine;
use tux_core::store::Store;

pub type SharedRuntime = Arc<RuntimeState>;

pub struct RuntimeState {
    pub instrument: Instrument,
    pub account_id: AccountId,
    pub strategy_instance_id: StrategyInstanceId,
    pub quote_asset: String,
    pub base_asset: String,
    pub store: Arc<Store>,
    pub bus: InMemoryEventBus,
    pub market: Mutex<MarketState>,
    pub portfolio: Mutex<Portfolio>,
    pub oms: Mutex<Oms>,
    pub risk: Mutex<RiskEngine>,
    pub paper: PaperEngine,
}

impl RuntimeState {
    pub fn new(
        instrument: Instrument,
        account_id: AccountId,
        strategy_instance_id: StrategyInstanceId,
        store: Arc<Store>,
        paper: PaperEngine,
        risk: RiskEngine,
    ) -> Self {
        let quote_asset = instrument.quote_asset.clone();
        let base_asset = instrument.base_asset.clone();
        Self {
            instrument,
            account_id,
            strategy_instance_id,
            quote_asset,
            base_asset,
            store,
            bus: InMemoryEventBus::new(),
            market: Mutex::new(MarketState::new()),
            portfolio: Mutex::new(Portfolio::new()),
            oms: Mutex::new(Oms::new()),
            risk: Mutex::new(risk),
            paper,
        }
    }

    pub fn validate_scope(&self, intent: &OrderIntent) -> Result<()> {
        if intent.account_id != self.account_id
            || intent.strategy_instance_id != self.strategy_instance_id
            || intent.instrument_id != self.instrument.id
        {
            return Err(TuxError::InvalidOrder(
                "intent account/strategy/instrument out of scope".into(),
            ));
        }
        Ok(())
    }

    pub fn apply_fill(&self, fill: &Fill) -> Result<()> {
        if fill.account_id != self.account_id {
            return Err(TuxError::InvalidOrder("fill account mismatch".into()));
        }
        {
            let mut pf = self.portfolio.lock().unwrap();
            let start = pf.ledger().len();
            pf.apply_spot_fill(
                &fill.account_id,
                &self.instrument,
                fill.side,
                fill.price,
                fill.quantity,
                fill.fee.amount,
                &fill.fee.asset,
            )?;
            for ev in &pf.ledger()[start..] {
                self.store.save_ledger(ev)?;
            }
        }
        self.store.save_fill(fill)?;
        if let Ok(o) = self.paper.query_order_blocking(&fill.order_id) {
            self.oms.lock().unwrap().upsert(o.clone());
            if o.status.is_terminal() {
                let notional = o
                    .notional(Some(fill.price))
                    .unwrap_or_else(|| fill.price * fill.quantity);
                if let Some(sid) = &o.strategy_instance_id {
                    self.risk
                        .lock()
                        .unwrap()
                        .note_order_terminal(sid, &o.account_id, notional);
                }
            }
            self.store.save_order(&o)?;
        }
        self.bus
            .publish_ignore_lag(tux_core::events::BusEvent::Fill(fill.clone()));
        Ok(())
    }

    pub fn drain_fills(&self) -> Result<usize> {
        let fills = self.paper.take_fills();
        let n = fills.len();
        for f in fills {
            self.apply_fill(&f)?;
        }
        Ok(n)
    }

    pub fn equity_mtm(&self) -> Decimal {
        let mut marks = HashMap::new();
        marks.insert(self.quote_asset.clone(), Decimal::ONE);
        if let Some(px) = self
            .market
            .lock()
            .unwrap()
            .reference_price(&self.instrument.id)
        {
            marks.insert(self.base_asset.clone(), px);
        }
        self.portfolio
            .lock()
            .unwrap()
            .equity_mtm(&self.account_id, &self.quote_asset, &marks)
    }

    pub fn persist_equity(&self) -> Result<()> {
        let equity = self.equity_mtm();
        let book = {
            let pf = self.portfolio.lock().unwrap();
            pf.pnl_book(&self.account_id).cloned().unwrap_or_default()
        };
        self.store.save_equity(&tux_core::store::EquityPoint {
            ts: tux_core::ids::now_ms(),
            account_id: self.account_id.to_string(),
            equity,
            net_pnl: book.net_pnl(),
            fees: book.components.trading_fees,
        })?;
        let bals: Vec<_> = {
            let pf = self.portfolio.lock().unwrap();
            pf.balances_for(&self.account_id)
                .into_iter()
                .cloned()
                .collect()
        };
        self.store.save_balances(self.account_id.as_str(), &bals)?;
        Ok(())
    }

    pub fn restore_from_store(&self) -> Result<usize> {
        let bals = self.store.load_balances(self.account_id.as_str())?;
        {
            let mut pf = self.portfolio.lock().unwrap();
            if bals.is_empty() {
                pf.seed_balance(Balance {
                    account_id: self.account_id.clone(),
                    asset: self.quote_asset.clone(),
                    free: Decimal::new(10000, 0),
                    locked: Decimal::ZERO,
                    updated_at: tux_core::ids::now_ms(),
                });
            } else {
                for b in bals {
                    pf.seed_balance(b);
                }
            }
        }
        let open = self.store.load_open_orders_for(self.account_id.as_str())?;
        let mut n = 0usize;
        {
            let mut oms = self.oms.lock().unwrap();
            let mut risk = self.risk.lock().unwrap();
            let market_ref = self
                .market
                .lock()
                .unwrap()
                .reference_price(&self.instrument.id);
            for o in open {
                if o.status.is_active() {
                    let notional = o.notional(market_ref).unwrap_or_default();
                    if let Some(sid) = &o.strategy_instance_id {
                        risk.note_order_restored(sid, &o.account_id, notional);
                    }
                    // Put back into paper book so it can still match / cancel.
                    self.paper.restore_order(o.clone());
                    n += 1;
                }
                oms.upsert(o);
            }
        }
        Ok(n)
    }

    pub fn kill_switch_engaged(&self) -> bool {
        self.risk.lock().unwrap().kill_switch.engaged
    }
}
