//! Local Paper closed loop on a shared RuntimeState (same instance as control API).

use std::str::FromStr;

use rust_decimal::Decimal;
use tux_core::domain::*;
use tux_core::events::{BookTicker, MarketEvent};
use tux_core::ids::*;
use tux_core::market::BookTop;
use tux_core::sim::FillModel;
use tux_strategy::{OneShotBuyStrategy, StrategyContext, StrategyHost};

use crate::runtime::SharedRuntime;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).expect("decimal")
}

fn synth_mid(i: i64) -> Decimal {
    let t = i as f64;
    let mid = 100.0 + 1.6 * (t * 0.45).sin() + 0.5 * (t * 1.1).cos() + t * 0.04;
    d(&format!("{:.2}", mid))
}

pub struct DemoReport {
    pub intents: usize,
    pub orders: usize,
    pub fills: usize,
    pub final_quote: Decimal,
    pub final_base: Decimal,
    pub net_pnl: Decimal,
    pub equity: Decimal,
    pub db_path: String,
    pub live_ticks: usize,
    pub total_ticks: usize,
    pub restored_orders: usize,
}

impl std::fmt::Display for DemoReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "intents processed : {}", self.intents)?;
        writeln!(f, "orders created    : {}", self.orders)?;
        writeln!(f, "fills             : {}", self.fills)?;
        writeln!(f, "restored orders   : {}", self.restored_orders)?;
        writeln!(f, "quote balance     : {}", self.final_quote)?;
        writeln!(f, "base balance      : {}", self.final_base)?;
        writeln!(f, "net pnl (fees in) : {}", self.net_pnl)?;
        writeln!(f, "equity (mtm)      : {}", self.equity)?;
        writeln!(
            f,
            "price feed        : {} ({}/{})",
            if self.live_ticks > 0 {
                "Binance SOLUSDT"
            } else {
                "synthetic"
            },
            self.live_ticks,
            self.total_ticks
        )?;
        writeln!(f, "sqlite db         : {}", self.db_path)
    }
}

/// Run book ticks through market → strategy → risk → OMS → paper → portfolio
/// **on the shared runtime** so the control API observes the same state.
pub async fn run_paper_demo(rt: &SharedRuntime, fill_model: FillModel, db_path: &str) -> anyhow::Result<DemoReport> {
    let _ = fill_model; // paper engine already built into runtime
    let account_id = rt.account_id.clone();
    let inst_id = rt.instrument.id.clone();
    let instrument = rt.instrument.clone();
    let strategy_id = rt.strategy_instance_id.clone();

    let restored_orders = rt.restore_from_store()?;

    let mut host = StrategyHost::new(
        StrategyContext {
            instance_id: strategy_id.clone(),
            account_id: account_id.clone(),
            environment: Environment::LocalPaper,
            config: StrategyConfig {
                version: ConfigVersionId::from("cfg1"),
                params: serde_json::json!({}),
                effective_event_seq: None,
                applied_at: None,
            },
        },
        Box::new(OneShotBuyStrategy::new(d("0.5"))),
    );
    host.start().await?;

    let mut intents_seen = 0usize;
    let mut orders_made = 0usize;
    let mut fill_count = 0usize;
    let mut _sub = rt.bus.subscribe();

    rt.store.clear_market_ticks(inst_id.as_str())?;
    let n_ticks = 16i64;
    let mut live_ticks = 0usize;
    let mut warmup: Option<crate::live_feed::LiveBook> = None;
    for _ in 0..6 {
        match crate::live_feed::fetch_book("SOLUSDT").await {
            Ok(b) => {
                warmup = Some(b);
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "live warmup retry");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
    let live_mode = warmup.is_some();

    for i in 0..n_ticks {
        let (bid, ask, bid_qty, ask_qty) = if live_mode {
            match crate::live_feed::fetch_book("SOLUSDT").await {
                Ok(b) => {
                    live_ticks += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    (b.bid, b.ask, b.bid_qty, b.ask_qty)
                }
                Err(_) => continue,
            }
        } else {
            let mid = synth_mid(i);
            let half = d("0.05");
            (mid - half, mid + half, d("2"), d("2"))
        };
        let top = BookTicker {
            instrument_id: inst_id.clone(),
            bid_price: bid,
            bid_qty,
            ask_price: ask,
            ask_qty,
            occurred_at: now_ms(),
        };
        {
            let mut market = rt.market.lock().unwrap();
            market.on_book_ticker(&top);
        }
        let book_top = BookTop {
            bid_price: top.bid_price,
            bid_qty: top.bid_qty,
            ask_price: top.ask_price,
            ask_qty: top.ask_qty,
            at: top.occurred_at,
        };
        rt.paper.on_book_top(&inst_id, book_top.clone());
        // P1: every tick drains paper fills into portfolio/OMS/risk/store.
        fill_count += rt.drain_fills()?;

        if let Some(mid) = book_top.mid() {
            rt.store.save_market_tick(
                inst_id.as_str(),
                book_top.bid_price,
                book_top.ask_price,
                mid,
                book_top.at,
            )?;
        }
        rt.bus
            .publish_ignore_lag(tux_core::events::BusEvent::Market(MarketEvent::BookTicker(
                top.clone(),
            )));

        let intents = host.on_market(&MarketEvent::BookTicker(top)).await?;
        for intent in intents {
            intents_seen += 1;
            if rt.validate_scope(&intent).is_err() {
                tracing::warn!(?intent, "intent out of scope, rejected");
                continue;
            }
            let (market_ref, age) = {
                let market = rt.market.lock().unwrap();
                (
                    market.reference_price(&inst_id),
                    market.age_ms(&inst_id),
                )
            };
            let decision = {
                let mut risk = rt.risk.lock().unwrap();
                let pf = rt.portfolio.lock().unwrap();
                let mut marks = std::collections::HashMap::new();
                marks.insert("USDT".to_string(), Decimal::ONE);
                if let Some(px) = market_ref {
                    marks.insert("SOL".to_string(), px);
                }
                let eq = pf.equity_mtm(&account_id, "USDT", &marks);
                let daily = pf.pnl_book(&account_id).map(|p| p.net_pnl()).unwrap_or_default();
                risk.update_pnl_facts(eq, daily);
                risk.check_intent(&intent, &instrument, market_ref, age, &pf)
            };
            if !decision.is_approved() {
                tracing::warn!(?decision, "intent rejected");
                continue;
            }
            let order = {
                let mut oms = rt.oms.lock().unwrap();
                oms.create_from_intent(&intent)?
            };
            orders_made += 1;
            // Never hold std::Mutex across await.
            let mut local = order.clone();
            tux_core::oms::OrderStateMachine::transition(
                &mut local,
                tux_core::domain::OrderStatus::Submitted,
            )?;
            let placed = rt.paper.place_order(&local).await?;
            {
                let mut oms = rt.oms.lock().unwrap();
                oms.upsert(placed.clone());
            }
            rt.store.save_order(&placed)?;
            fill_count += rt.drain_fills()?;
            // Rejected/cancelled with no fills must free risk budget.
            if placed.status.is_terminal() {
                if let Ok(cur) = rt.paper.query_order_blocking(&placed.id) {
                    if cur.filled_quantity.is_zero() {
                        if let Some(sid) = &cur.strategy_instance_id {
                            let notional = cur.notional(market_ref).unwrap_or_default();
                            rt.risk
                                .lock()
                                .unwrap()
                                .note_order_terminal(sid, &cur.account_id, notional);
                        }
                    }
                }
            }
            // Rejected/cancelled with no fills must free risk budget.
            if placed.status.is_terminal() {
                if let Ok(cur) = rt.paper.query_order_blocking(&placed.id) {
                    if cur.filled_quantity.is_zero() {
                        if let Some(sid) = &cur.strategy_instance_id {
                            let notional = cur.notional(market_ref).unwrap_or_default();
                            rt.risk
                                .lock()
                                .unwrap()
                                .note_order_terminal(sid, &cur.account_id, notional);
                        }
                    }
                }
            }
            // Rejected/cancelled with no fills must free risk budget.
            if placed.status.is_terminal() {
                if let Ok(cur) = rt.paper.query_order_blocking(&placed.id) {
                    if cur.filled_quantity.is_zero() {
                        if let Some(sid) = &cur.strategy_instance_id {
                            let notional = cur.notional(market_ref).unwrap_or_default();
                            rt.risk
                                .lock()
                                .unwrap()
                                .note_order_terminal(sid, &cur.account_id, notional);
                        }
                    }
                }
            }
            // Rejected/cancelled with no fills must free risk budget.
            if placed.status.is_terminal() {
                if let Ok(cur) = rt.paper.query_order_blocking(&placed.id) {
                    if cur.filled_quantity.is_zero() {
                        if let Some(sid) = &cur.strategy_instance_id {
                            let notional = cur.notional(market_ref).unwrap_or_default();
                            rt.risk
                                .lock()
                                .unwrap()
                                .note_order_terminal(sid, &cur.account_id, notional);
                        }
                    }
                }
            }
            // Rejected/cancelled with no fills must free risk budget.
            if placed.status.is_terminal() {
                if let Ok(cur) = rt.paper.query_order_blocking(&placed.id) {
                    if cur.filled_quantity.is_zero() {
                        if let Some(sid) = &cur.strategy_instance_id {
                            let notional = cur.notional(market_ref).unwrap_or_default();
                            rt.risk
                                .lock()
                                .unwrap()
                                .note_order_terminal(sid, &cur.account_id, notional);
                        }
                    }
                }
            }
            // Rejected/cancelled with no fills must free risk budget.
            if placed.status.is_terminal() {
                if let Ok(cur) = rt.paper.query_order_blocking(&placed.id) {
                    if cur.filled_quantity.is_zero() {
                        if let Some(sid) = &cur.strategy_instance_id {
                            let notional = cur.notional(market_ref).unwrap_or_default();
                            rt.risk
                                .lock()
                                .unwrap()
                                .note_order_terminal(sid, &cur.account_id, notional);
                        }
                    }
                }
            }
            // Rejected/cancelled with no fills must free risk budget.
            if placed.status.is_terminal() {
                if let Ok(cur) = rt.paper.query_order_blocking(&placed.id) {
                    if cur.filled_quantity.is_zero() {
                        if let Some(sid) = &cur.strategy_instance_id {
                            let notional = cur.notional(market_ref).unwrap_or_default();
                            rt.risk
                                .lock()
                                .unwrap()
                                .note_order_terminal(sid, &cur.account_id, notional);
                        }
                    }
                }
            }
            tracing::info!(
                order = %placed.id,
                status = %placed.status,
                filled = %placed.filled_quantity,
                "order executed"
            );
        }

        rt.persist_equity()?;
    }

    let quote = {
        let pf = rt.portfolio.lock().unwrap();
        pf.balance(&account_id, "USDT").map(|b| b.free).unwrap_or_default()
    };
    let base = {
        let pf = rt.portfolio.lock().unwrap();
        pf.balance(&account_id, "SOL").map(|b| b.free).unwrap_or_default()
    };
    let net = {
        let pf = rt.portfolio.lock().unwrap();
        pf.pnl_book(&account_id).map(|p| p.net_pnl()).unwrap_or_default()
    };
    let equity = rt.equity_mtm();

    anyhow::ensure!(intents_seen >= 1, "strategy produced no intents");
    anyhow::ensure!(orders_made >= 1, "OMS created no orders");
    anyhow::ensure!(base > Decimal::ZERO, "expected base fill in portfolio");

    Ok(DemoReport {
        intents: intents_seen,
        orders: orders_made,
        fills: fill_count,
        final_quote: quote,
        final_base: base,
        net_pnl: net,
        equity,
        db_path: db_path.to_string(),
        live_ticks,
        total_ticks: n_ticks as usize,
        restored_orders,
    })
}
