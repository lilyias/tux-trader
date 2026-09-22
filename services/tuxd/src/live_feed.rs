//! Binance public market data (no API key).
//!
//! Used for Local Paper book + dashboard price trend so Mid matches live market.
#![allow(dead_code)]

use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;
use tux_core::events::BookTicker as CoreBookTicker;
use tux_core::market::BookTop;

use crate::runtime::SharedRuntime;

#[derive(Debug, Clone, Deserialize)]
pub struct BookTicker {
    pub symbol: String,
    #[serde(rename = "bidPrice")]
    pub bid_price: String,
    #[serde(rename = "bidQty")]
    pub bid_qty: String,
    #[serde(rename = "askPrice")]
    pub ask_price: String,
    #[serde(rename = "askQty")]
    pub ask_qty: String,
}

#[derive(Debug, Clone)]
pub struct LiveBook {
    pub bid: Decimal,
    pub bid_qty: Decimal,
    pub ask: Decimal,
    pub ask_qty: Decimal,
}

/// Keep the dashboard market snapshot, price history and paper matching live.
pub fn spawn_price_sampler(runtime: SharedRuntime, symbol: &'static str) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tick.tick().await;
            match fetch_book(symbol).await {
                Ok(b) => {
                    let mid = b.mid();
                    let at = tux_core::ids::now_ms();
                    let instrument_id = runtime.instrument.id.clone();
                    let event = CoreBookTicker {
                        instrument_id: instrument_id.clone(),
                        bid_price: b.bid,
                        bid_qty: b.bid_qty,
                        ask_price: b.ask,
                        ask_qty: b.ask_qty,
                        occurred_at: at,
                    };
                    runtime.market.lock().unwrap().on_book_ticker(&event);
                    runtime.paper.on_book_top(
                        &instrument_id,
                        BookTop {
                            bid_price: b.bid,
                            bid_qty: b.bid_qty,
                            ask_price: b.ask,
                            ask_qty: b.ask_qty,
                            at,
                        },
                    );
                    if let Err(error) = runtime.drain_fills() {
                        tracing::error!(error = %error, "price sampler: apply fill failed");
                    }
                    if let Err(error) = runtime.store.save_market_tick(
                        instrument_id.as_str(),
                        b.bid,
                        b.ask,
                        mid,
                        at,
                    ) {
                        tracing::error!(error = %error, "price sampler: persist tick failed");
                    }
                    let _ = runtime.persist_equity();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "price sampler: fetch failed");
                }
            }
        }
    });
}

impl LiveBook {
    pub fn mid(&self) -> Decimal {
        (self.bid + self.ask) / Decimal::TWO
    }
}

fn dec(s: &str) -> Option<Decimal> {
    Decimal::from_str(s).ok()
}

/// `GET https://api.binance.com/api/v3/ticker/bookTicker?symbol=SOLUSDT`
pub async fn fetch_book(symbol: &str) -> anyhow::Result<LiveBook> {
    let url = format!("https://api.binance.com/api/v3/ticker/bookTicker?symbol={symbol}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()?;
    let raw: BookTicker = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let bid = dec(&raw.bid_price).ok_or_else(|| anyhow::anyhow!("bad bid"))?;
    let ask = dec(&raw.ask_price).ok_or_else(|| anyhow::anyhow!("bad ask"))?;
    let bid_qty = dec(&raw.bid_qty).unwrap_or(Decimal::ONE);
    let ask_qty = dec(&raw.ask_qty).unwrap_or(Decimal::ONE);
    if bid <= Decimal::ZERO || ask <= Decimal::ZERO {
        anyhow::bail!("empty book from binance");
    }
    // Align to 0.01 tick so Local Paper limit prices validate.
    let bid = round_tick(bid);
    let ask = round_tick(ask);
    Ok(LiveBook {
        bid,
        bid_qty,
        ask: ask.max(bid + rust_decimal::Decimal::new(1, 2)),
        ask_qty,
    })
}

fn round_tick(px: Decimal) -> Decimal {
    use tux_core::decimal::round_to_step;
    let tick = Decimal::new(1, 2);
    round_to_step(px, tick).unwrap_or(px)
}

/// Optional last price (for KPI when book is one-sided).
pub async fn fetch_last_price(symbol: &str) -> anyhow::Result<Decimal> {
    let url = format!("https://api.binance.com/api/v3/ticker/price?symbol={symbol}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let raw: serde_json::Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    raw.get("price")
        .and_then(|v| v.as_str())
        .and_then(dec)
        .ok_or_else(|| anyhow::anyhow!("bad last price"))
}
