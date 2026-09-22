//! Latest top-of-book / trade state for risk and paper matching.

use std::collections::HashMap;
use rust_decimal::Decimal;

use crate::domain::InstrumentId;
use crate::events::{BookTicker, TradeTick};
use crate::ids::{now_ms, TimestampMs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookTop {
    pub bid_price: Decimal,
    pub bid_qty: Decimal,
    pub ask_price: Decimal,
    pub ask_qty: Decimal,
    pub at: TimestampMs,
}

impl BookTop {
    pub fn mid(&self) -> Option<Decimal> {
        if self.bid_price <= Decimal::ZERO || self.ask_price <= Decimal::ZERO {
            return None;
        }
        Some((self.bid_price + self.ask_price) / Decimal::TWO)
    }

    pub fn age_ms(&self, now: TimestampMs) -> u64 {
        (now - self.at).max(0) as u64
    }
}

#[derive(Debug, Default)]
pub struct MarketState {
    tops: HashMap<String, BookTop>,
    last_trade: HashMap<String, TradeTick>,
}

impl MarketState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_book_ticker(&mut self, t: &BookTicker) {
        self.tops.insert(
            t.instrument_id.as_str().to_string(),
            BookTop {
                bid_price: t.bid_price,
                bid_qty: t.bid_qty,
                ask_price: t.ask_price,
                ask_qty: t.ask_qty,
                at: t.occurred_at,
            },
        );
    }

    pub fn on_trade(&mut self, t: &TradeTick) {
        self.last_trade
            .insert(t.instrument_id.as_str().to_string(), t.clone());
    }

    pub fn top(&self, id: &InstrumentId) -> Option<&BookTop> {
        self.tops.get(id.as_str())
    }

    pub fn mid(&self, id: &InstrumentId) -> Option<Decimal> {
        self.top(id).and_then(|t| t.mid())
    }

    pub fn last_price(&self, id: &InstrumentId) -> Option<Decimal> {
        self.last_trade.get(id.as_str()).map(|t| t.price)
    }

    /// Mid, else last trade — used for market-order notional estimation.
    pub fn reference_price(&self, id: &InstrumentId) -> Option<Decimal> {
        self.mid(id).or_else(|| self.last_price(id))
    }

    pub fn age_ms(&self, id: &InstrumentId) -> Option<u64> {
        self.top(id).map(|t| t.age_ms(now_ms()))
    }

    pub fn is_stale(&self, id: &InstrumentId, max_age_ms: u64) -> bool {
        match self.age_ms(id) {
            Some(age) => age > max_age_ms,
            None => true,
        }
    }
}
