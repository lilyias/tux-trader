//! Shared process runtime: market, strategy control, risk, OMS and paper execution.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tux_core::decimal::round_to_step;
use tux_core::domain::{
    Balance, ExecutionPort, Fill, Instrument, Order, OrderIntent, OrderSide, OrderStatus,
    OrderType, PositionSide, ReduceOnly, RiskDecision, TimeInForce,
};
use tux_core::error::{Result, TuxError};
use tux_core::events::InMemoryEventBus;
use tux_core::ids::{now_ms, AccountId, IntentId, StrategyInstanceId, TimestampMs};
use tux_core::market::MarketState;
use tux_core::oms::{generate_client_order_id, Oms, OrderStateMachine};
use tux_core::portfolio::Portfolio;
use tux_core::risk::RiskEngine;
use tux_core::sim::PaperEngine;
use tux_core::store::Store;

pub type SharedRuntime = Arc<RuntimeState>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyRunStatus {
    Running,
    Paused,
    Stopped,
    Error,
}

impl StrategyRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }

    pub fn can_execute(self) -> bool {
        self == Self::Running
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyParameters {
    pub side: OrderSide,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub quantity: Decimal,
    pub limit_offset_bps: Decimal,
}

impl Default for StrategyParameters {
    fn default() -> Self {
        Self {
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            quantity: Decimal::new(5, 1),
            limit_offset_bps: Decimal::ZERO,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategySnapshot {
    pub strategy_id: String,
    pub name: String,
    pub version: String,
    pub status: StrategyRunStatus,
    pub environment: String,
    pub account_id: String,
    pub instrument_id: String,
    pub parameters: StrategyParameters,
    pub revision: u64,
    pub started_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub last_signal_at: Option<TimestampMs>,
    pub last_order_id: Option<String>,
    pub last_error: Option<String>,
    pub signals: u64,
    pub orders: u64,
    pub fills: u64,
}

impl StrategySnapshot {
    fn fresh(
        strategy_id: &StrategyInstanceId,
        account_id: &AccountId,
        instrument: &Instrument,
    ) -> Self {
        let now = now_ms();
        Self {
            strategy_id: strategy_id.to_string(),
            name: "One-shot directional strategy".into(),
            version: "1.0.0".into(),
            status: StrategyRunStatus::Running,
            environment: "local_paper".into(),
            account_id: account_id.to_string(),
            instrument_id: instrument.id.to_string(),
            parameters: StrategyParameters::default(),
            revision: 1,
            started_at: now,
            updated_at: now,
            last_signal_at: None,
            last_order_id: None,
            last_error: None,
            signals: 0,
            orders: 0,
            fills: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MarketSnapshot {
    pub instrument_id: String,
    pub bid: Decimal,
    pub ask: Decimal,
    pub mid: Decimal,
    pub bid_quantity: Decimal,
    pub ask_quantity: Decimal,
    pub occurred_at: TimestampMs,
    pub age_ms: u64,
}

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
    strategy: Mutex<StrategySnapshot>,
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
        let strategy = store
            .load_strategy_snapshot(strategy_instance_id.as_str())
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_else(|| {
                StrategySnapshot::fresh(&strategy_instance_id, &account_id, &instrument)
            });
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
            strategy: Mutex::new(strategy),
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

    pub fn strategy_snapshot(&self) -> StrategySnapshot {
        self.strategy.lock().unwrap().clone()
    }

    fn persist_strategy_snapshot(&self, snapshot: &StrategySnapshot) -> Result<()> {
        let json = serde_json::to_string(snapshot)
            .map_err(|e| TuxError::Storage(format!("strategy serialize: {e}")))?;
        self.store.save_strategy_snapshot(
            self.strategy_instance_id.as_str(),
            &json,
            snapshot.updated_at,
        )
    }

    pub fn configure_strategy(&self, parameters: StrategyParameters) -> Result<StrategySnapshot> {
        if parameters.quantity <= Decimal::ZERO
            || !self.instrument.validate_qty(parameters.quantity)
        {
            return Err(TuxError::Config(format!(
                "quantity {} violates instrument lot size",
                parameters.quantity
            )));
        }
        if parameters.limit_offset_bps.abs() > Decimal::new(1000, 0) {
            return Err(TuxError::Config(
                "limit_offset_bps must be between -1000 and 1000".into(),
            ));
        }
        if matches!(parameters.order_type, OrderType::Stop | OrderType::PostOnly) {
            return Err(TuxError::Config(
                "dashboard strategy supports limit or market orders".into(),
            ));
        }
        let snapshot = {
            let mut state = self.strategy.lock().unwrap();
            state.parameters = parameters;
            state.revision += 1;
            state.updated_at = now_ms();
            state.last_error = None;
            state.clone()
        };
        self.persist_strategy_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    pub fn set_strategy_status(&self, status: StrategyRunStatus) -> Result<StrategySnapshot> {
        let snapshot = {
            let mut state = self.strategy.lock().unwrap();
            state.status = status;
            state.updated_at = now_ms();
            if status != StrategyRunStatus::Error {
                state.last_error = None;
            }
            state.clone()
        };
        self.persist_strategy_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    pub fn record_strategy_operation(
        &self,
        action: &str,
        status: &str,
        actor: &str,
        detail: &str,
    ) -> Result<i64> {
        self.store.append_strategy_operation(
            self.strategy_instance_id.as_str(),
            action,
            status,
            actor,
            detail,
            now_ms(),
        )
    }

    pub fn current_market(&self) -> Option<MarketSnapshot> {
        let market = self.market.lock().unwrap();
        let top = market.top(&self.instrument.id)?;
        Some(MarketSnapshot {
            instrument_id: self.instrument.id.to_string(),
            bid: top.bid_price,
            ask: top.ask_price,
            mid: top.mid()?,
            bid_quantity: top.bid_qty,
            ask_quantity: top.ask_qty,
            occurred_at: top.at,
            age_ms: top.age_ms(now_ms()),
        })
    }

    pub fn strategy_intent(&self) -> Result<OrderIntent> {
        let snapshot = self.strategy_snapshot();
        if !snapshot.status.can_execute() {
            return Err(TuxError::InvalidOrder(format!(
                "strategy is {}",
                snapshot.status.as_str()
            )));
        }
        let top = self
            .market
            .lock()
            .unwrap()
            .top(&self.instrument.id)
            .cloned()
            .ok_or_else(|| TuxError::StaleMarketData {
                instrument: self.instrument.id.to_string(),
            })?;
        let parameters = snapshot.parameters;
        let price = match parameters.order_type {
            OrderType::Market => None,
            OrderType::Limit => {
                let fraction = parameters.limit_offset_bps / Decimal::new(10_000, 0);
                let raw = match parameters.side {
                    OrderSide::Buy => top.ask_price * (Decimal::ONE + fraction),
                    OrderSide::Sell => top.bid_price * (Decimal::ONE - fraction),
                };
                Some(
                    round_to_step(raw, self.instrument.tick_size).ok_or_else(|| {
                        TuxError::Config("instrument price tick must be positive".into())
                    })?,
                )
            }
            _ => {
                return Err(TuxError::InvalidOrder(
                    "unsupported strategy order type".into(),
                ))
            }
        };
        let saved = {
            let mut state = self.strategy.lock().unwrap();
            state.signals += 1;
            state.last_signal_at = Some(now_ms());
            state.updated_at = now_ms();
            state.clone()
        };
        self.persist_strategy_snapshot(&saved)?;
        Ok(OrderIntent {
            id: IntentId::new(),
            strategy_instance_id: self.strategy_instance_id.clone(),
            account_id: self.account_id.clone(),
            instrument_id: self.instrument.id.clone(),
            side: parameters.side,
            order_type: parameters.order_type,
            time_in_force: parameters.time_in_force,
            price,
            quantity: parameters.quantity,
            reduce_only: ReduceOnly::No,
            position_side: PositionSide::Both,
            leverage: None,
            client_order_id: generate_client_order_id(&self.strategy_instance_id),
            reason: Some("dashboard strategy trigger".into()),
            created_at: now_ms(),
        })
    }

    pub fn note_strategy_signal(&self) -> Result<()> {
        let snapshot = {
            let mut state = self.strategy.lock().unwrap();
            state.signals += 1;
            state.last_signal_at = Some(now_ms());
            state.updated_at = now_ms();
            state.clone()
        };
        self.persist_strategy_snapshot(&snapshot)
    }

    fn release_risk(&self, intent: &OrderIntent, notional: Decimal) {
        self.risk.lock().unwrap().note_order_terminal(
            &intent.strategy_instance_id,
            &intent.account_id,
            notional,
        );
    }

    pub async fn execute_intent(&self, intent: OrderIntent) -> Result<Order> {
        self.validate_scope(&intent)?;
        let (market_ref, age) = {
            let market = self.market.lock().unwrap();
            (
                market.reference_price(&self.instrument.id),
                market.age_ms(&self.instrument.id),
            )
        };
        let notional = intent.notional(market_ref).unwrap_or_default();
        let decision = {
            let equity = self.equity_mtm();
            let mut risk = self.risk.lock().unwrap();
            let portfolio = self.portfolio.lock().unwrap();
            let daily = portfolio
                .pnl_book(&self.account_id)
                .map(|book| book.net_pnl())
                .unwrap_or_default();
            risk.update_pnl_facts(equity, daily);
            risk.check_intent(&intent, &self.instrument, market_ref, age, &portfolio)
        };
        if let RiskDecision::Rejected { rule, message } = decision {
            return Err(TuxError::RiskRejected { rule, message });
        }

        let order = match self.oms.lock().unwrap().create_from_intent(&intent) {
            Ok(order) => order,
            Err(error) => {
                self.release_risk(&intent, notional);
                return Err(error);
            }
        };
        let mut submitted = order.clone();
        if let Err(error) = OrderStateMachine::transition(&mut submitted, OrderStatus::Submitted) {
            self.release_risk(&intent, notional);
            return Err(error);
        }
        let placed = match self.paper.place_order(&submitted).await {
            Ok(placed) => placed,
            Err(error) => {
                self.release_risk(&intent, notional);
                return Err(error);
            }
        };
        self.oms.lock().unwrap().upsert(placed.clone());
        self.store.save_order(&placed)?;
        self.drain_fills()?;
        if placed.status.is_terminal() && placed.filled_quantity.is_zero() {
            self.release_risk(&intent, notional);
        }
        Ok(placed)
    }

    pub fn note_strategy_order(&self, order: &Order) -> Result<()> {
        let snapshot = {
            let mut state = self.strategy.lock().unwrap();
            state.orders += 1;
            state.last_order_id = Some(order.id.to_string());
            state.last_error = None;
            state.updated_at = now_ms();
            state.clone()
        };
        self.persist_strategy_snapshot(&snapshot)
    }

    pub fn apply_fill(&self, fill: &Fill) -> Result<()> {
        if fill.account_id != self.account_id {
            return Err(TuxError::InvalidOrder("fill account mismatch".into()));
        }
        {
            let mut portfolio = self.portfolio.lock().unwrap();
            let start = portfolio.ledger().len();
            portfolio.apply_spot_fill(
                &fill.account_id,
                &self.instrument,
                fill.side,
                fill.price,
                fill.quantity,
                fill.fee.amount,
                &fill.fee.asset,
            )?;
            for event in &portfolio.ledger()[start..] {
                self.store.save_ledger(event)?;
            }
        }
        self.store.save_fill(fill)?;
        if let Ok(order) = self.paper.query_order_blocking(&fill.order_id) {
            self.oms.lock().unwrap().upsert(order.clone());
            if order.status.is_terminal() {
                let notional = order
                    .notional(Some(fill.price))
                    .unwrap_or_else(|| fill.price * fill.quantity);
                if let Some(strategy_id) = &order.strategy_instance_id {
                    self.risk.lock().unwrap().note_order_terminal(
                        strategy_id,
                        &order.account_id,
                        notional,
                    );
                }
            }
            self.store.save_order(&order)?;
        }
        let snapshot = {
            let mut state = self.strategy.lock().unwrap();
            state.fills += 1;
            state.updated_at = now_ms();
            state.clone()
        };
        self.persist_strategy_snapshot(&snapshot)?;
        self.bus
            .publish_ignore_lag(tux_core::events::BusEvent::Fill(fill.clone()));
        Ok(())
    }

    pub fn drain_fills(&self) -> Result<usize> {
        let fills = self.paper.take_fills();
        let count = fills.len();
        for fill in fills {
            self.apply_fill(&fill)?;
        }
        Ok(count)
    }

    pub fn equity_mtm(&self) -> Decimal {
        let mut marks = HashMap::new();
        marks.insert(self.quote_asset.clone(), Decimal::ONE);
        if let Some(price) = self
            .market
            .lock()
            .unwrap()
            .reference_price(&self.instrument.id)
        {
            marks.insert(self.base_asset.clone(), price);
        }
        self.portfolio
            .lock()
            .unwrap()
            .equity_mtm(&self.account_id, &self.quote_asset, &marks)
    }

    pub fn persist_equity(&self) -> Result<()> {
        let equity = self.equity_mtm();
        let (book, balances) = {
            let portfolio = self.portfolio.lock().unwrap();
            let book = portfolio
                .pnl_book(&self.account_id)
                .cloned()
                .unwrap_or_default();
            let balances = portfolio
                .balances_for(&self.account_id)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            (book, balances)
        };
        self.risk
            .lock()
            .unwrap()
            .update_pnl_facts(equity, book.net_pnl());
        self.store.save_equity(&tux_core::store::EquityPoint {
            ts: now_ms(),
            account_id: self.account_id.to_string(),
            equity,
            net_pnl: book.net_pnl(),
            fees: book.components.trading_fees,
        })?;
        self.store
            .save_balances(self.account_id.as_str(), &balances)?;
        Ok(())
    }

    pub fn restore_from_store(&self) -> Result<usize> {
        let balances = self.store.load_balances(self.account_id.as_str())?;
        {
            let mut portfolio = self.portfolio.lock().unwrap();
            if balances.is_empty() {
                portfolio.seed_balance(Balance {
                    account_id: self.account_id.clone(),
                    asset: self.quote_asset.clone(),
                    free: Decimal::new(10_000, 0),
                    locked: Decimal::ZERO,
                    updated_at: now_ms(),
                });
            } else {
                for balance in balances {
                    portfolio.seed_balance(balance);
                }
            }
        }
        let open = self.store.load_open_orders_for(self.account_id.as_str())?;
        let mut count = 0usize;
        let mut oms = self.oms.lock().unwrap();
        let mut risk = self.risk.lock().unwrap();
        let market_ref = self
            .market
            .lock()
            .unwrap()
            .reference_price(&self.instrument.id);
        for order in open {
            if order.instrument_id != self.instrument.id {
                continue;
            }
            if order.status.is_active() {
                let notional = order.notional(market_ref).unwrap_or_default();
                if let Some(strategy_id) = &order.strategy_instance_id {
                    risk.note_order_restored(strategy_id, &order.account_id, notional);
                }
                self.paper.restore_order(order.clone());
                count += 1;
            }
            oms.upsert(order);
        }
        Ok(count)
    }

    pub fn kill_switch_engaged(&self) -> bool {
        self.risk.lock().unwrap().kill_switch.engaged
    }
}
