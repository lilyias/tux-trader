//! Local Paper loop using the same runtime as the dashboard and control API.

use std::str::FromStr;

use rust_decimal::Decimal;
use tux_core::domain::StrategyConfig;
use tux_core::events::{BookTicker, MarketEvent};
use tux_core::ids::{now_ms, ConfigVersionId, Environment};
use tux_core::market::BookTop;
use tux_core::sim::FillModel;
use tux_strategy::{OneShotBuyStrategy, StrategyContext, StrategyHost};

use crate::runtime::SharedRuntime;

fn decimal(value: &str) -> Decimal {
    Decimal::from_str(value).expect("decimal")
}

fn synthetic_mid(index: i64) -> Decimal {
    let step = index as f64;
    let mid = 100.0 + 1.6 * (step * 0.45).sin() + 0.5 * (step * 1.1).cos() + step * 0.04;
    decimal(&format!("{mid:.2}"))
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
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(formatter, "intents processed : {}", self.intents)?;
        writeln!(formatter, "orders created    : {}", self.orders)?;
        writeln!(formatter, "fills             : {}", self.fills)?;
        writeln!(formatter, "restored orders   : {}", self.restored_orders)?;
        writeln!(formatter, "quote balance     : {}", self.final_quote)?;
        writeln!(formatter, "base balance      : {}", self.final_base)?;
        writeln!(formatter, "net pnl (fees in) : {}", self.net_pnl)?;
        writeln!(formatter, "equity (mtm)      : {}", self.equity)?;
        writeln!(
            formatter,
            "price feed        : {} ({}/{})",
            if self.live_ticks > 0 {
                "Binance SOLUSDT"
            } else {
                "synthetic"
            },
            self.live_ticks,
            self.total_ticks
        )?;
        writeln!(formatter, "sqlite db         : {}", self.db_path)
    }
}

/// Run a short bootstrap loop. When the dashboard stays open, the background
/// sampler continues updating market state and matching resting paper orders.
pub async fn run_paper_demo(
    runtime: &SharedRuntime,
    _fill_model: FillModel,
    db_path: &str,
) -> anyhow::Result<DemoReport> {
    let account_id = runtime.account_id.clone();
    let instrument_id = runtime.instrument.id.clone();
    let strategy_id = runtime.strategy_instance_id.clone();
    let restored_orders = runtime.restore_from_store()?;
    let initial_strategy = runtime.strategy_snapshot();
    let mut applied_revision = initial_strategy.revision;
    let mut applied_config = StrategyConfig {
        version: ConfigVersionId::from(format!("cfg-{}", applied_revision)),
        params: serde_json::to_value(&initial_strategy.parameters)?,
        effective_event_seq: None,
        applied_at: Some(now_ms()),
    };
    let mut host = StrategyHost::new(
        StrategyContext {
            instance_id: strategy_id,
            account_id: account_id.clone(),
            environment: Environment::LocalPaper,
            config: applied_config.clone(),
        },
        Box::new(OneShotBuyStrategy::new(
            initial_strategy.parameters.quantity,
        )),
    );
    host.start().await?;
    let _ = runtime.record_strategy_operation(
        "runtime_start",
        "succeeded",
        "system",
        "strategy runtime started",
    );

    let mut intents_seen = 0usize;
    let mut orders_made = 0usize;
    let mut fill_count = 0usize;
    let mut _subscriber = runtime.bus.subscribe();
    let total_ticks = 16i64;
    let mut live_ticks = 0usize;
    let mut warmup = None;
    for _ in 0..3 {
        match crate::live_feed::fetch_book("SOLUSDT").await {
            Ok(book) => {
                warmup = Some(book);
                break;
            }
            Err(error) => {
                tracing::warn!(error = %error, "live warmup retry");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
    let serve_mode = std::env::var("TUXD_SERVE").as_deref() == Ok("1");
    let synthetic_mode = warmup.is_none() && !serve_mode;

    for index in 0..total_ticks {
        let live_book = if synthetic_mode {
            None
        } else if warmup.is_some() {
            warmup.take()
        } else {
            match crate::live_feed::fetch_book("SOLUSDT").await {
                Ok(book) => Some(book),
                Err(error) => {
                    tracing::warn!(error = %error, "live tick unavailable");
                    None
                }
            }
        };
        let (bid, ask, bid_quantity, ask_quantity) = if let Some(book) = live_book {
            live_ticks += 1;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            (book.bid, book.ask, book.bid_qty, book.ask_qty)
        } else if synthetic_mode {
            let mid = synthetic_mid(index);
            let half = decimal("0.05");
            (mid - half, mid + half, decimal("2"), decimal("2"))
        } else {
            runtime.persist_equity()?;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            continue;
        };
        let tick = BookTicker {
            instrument_id: instrument_id.clone(),
            bid_price: bid,
            bid_qty: bid_quantity,
            ask_price: ask,
            ask_qty: ask_quantity,
            occurred_at: now_ms(),
        };
        runtime.market.lock().unwrap().on_book_ticker(&tick);
        let book_top = BookTop {
            bid_price: tick.bid_price,
            bid_qty: tick.bid_qty,
            ask_price: tick.ask_price,
            ask_qty: tick.ask_qty,
            at: tick.occurred_at,
        };
        runtime.paper.on_book_top(&instrument_id, book_top.clone());
        fill_count += runtime.drain_fills()?;
        if let Some(mid) = book_top.mid() {
            runtime.store.save_market_tick(
                instrument_id.as_str(),
                book_top.bid_price,
                book_top.ask_price,
                mid,
                book_top.at,
            )?;
        }
        runtime
            .bus
            .publish_ignore_lag(tux_core::events::BusEvent::Market(MarketEvent::BookTicker(
                tick.clone(),
            )));

        let strategy = runtime.strategy_snapshot();
        if strategy.revision != applied_revision {
            let next = StrategyConfig {
                version: ConfigVersionId::from(format!("cfg-{}", strategy.revision)),
                params: serde_json::to_value(&strategy.parameters)?,
                effective_event_seq: None,
                applied_at: Some(now_ms()),
            };
            host.update_config(&applied_config, &next).await?;
            applied_config = next;
            applied_revision = strategy.revision;
        }
        if !strategy.status.can_execute() {
            runtime.persist_equity()?;
            continue;
        }

        let intents = host.on_market(&MarketEvent::BookTicker(tick)).await?;
        for intent in intents {
            intents_seen += 1;
            runtime.note_strategy_signal()?;
            match runtime.execute_intent(intent).await {
                Ok(order) => {
                    orders_made += 1;
                    runtime.note_strategy_order(&order)?;
                    let _ = runtime.record_strategy_operation(
                        "automatic_signal",
                        "succeeded",
                        "strategy",
                        &format!("created order {} ({})", order.id, order.status),
                    );
                    tracing::info!(
                        order = %order.id,
                        status = %order.status,
                        filled = %order.filled_quantity,
                        "strategy order executed"
                    );
                }
                Err(error) => {
                    let _ = runtime.record_strategy_operation(
                        "automatic_signal",
                        "failed",
                        "strategy",
                        &error.to_string(),
                    );
                    tracing::warn!(error = %error, "strategy intent rejected");
                }
            }
        }
        runtime.persist_equity()?;
    }

    let (quote, base, net_pnl) = {
        let portfolio = runtime.portfolio.lock().unwrap();
        let quote = portfolio
            .balance(&account_id, &runtime.quote_asset)
            .map(|balance| balance.free)
            .unwrap_or_default();
        let base = portfolio
            .balance(&account_id, &runtime.base_asset)
            .map(|balance| balance.free)
            .unwrap_or_default();
        let net_pnl = portfolio
            .pnl_book(&account_id)
            .map(|book| book.net_pnl())
            .unwrap_or_default();
        (quote, base, net_pnl)
    };
    Ok(DemoReport {
        intents: intents_seen,
        orders: orders_made,
        fills: fill_count,
        final_quote: quote,
        final_base: base,
        net_pnl,
        equity: runtime.equity_mtm(),
        db_path: db_path.to_string(),
        live_ticks,
        total_ticks: total_ticks as usize,
        restored_orders,
    })
}
