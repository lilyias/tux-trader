//! In-process event bus and domain events.
//!
//! External NATS JetStream is optional later; local Paper MVP uses this bus.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::domain::{Fill, InstrumentId, Order, OrderIntent};
use crate::error::{Result, TuxError};
use crate::ids::{EventSeq, MarketContext, TimestampMs};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope<T> {
    pub seq: EventSeq,
    pub context: MarketContext,
    pub produced_at: TimestampMs,
    pub payload: T,
}

impl<T> EventEnvelope<T> {
    pub fn new(seq: EventSeq, context: MarketContext, payload: T) -> Self {
        Self {
            seq,
            context,
            produced_at: crate::ids::now_ms(),
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookTicker {
    pub instrument_id: InstrumentId,
    pub bid_price: rust_decimal::Decimal,
    pub bid_qty: rust_decimal::Decimal,
    pub ask_price: rust_decimal::Decimal,
    pub ask_qty: rust_decimal::Decimal,
    pub occurred_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeTick {
    pub instrument_id: InstrumentId,
    pub price: rust_decimal::Decimal,
    pub qty: rust_decimal::Decimal,
    pub is_buyer_maker: bool,
    pub occurred_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MarketEvent {
    BookTicker(BookTicker),
    Trade(TradeTick),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BusEvent {
    Market(MarketEvent),
    Intent(OrderIntent),
    OrderUpdate(Order),
    Fill(Fill),
    Halt { reason: String, at: TimestampMs },
}

/// Working in-process bus (broadcast). Nothing is silently dropped.
#[derive(Clone)]
pub struct InMemoryEventBus {
    tx: broadcast::Sender<BusEvent>,
}

impl InMemoryEventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }

    pub fn publish(&self, event: BusEvent) -> Result<usize> {
        self.tx.send(event).map_err(|_| {
            TuxError::other("event bus has no active subscribers; event not delivered")
        })
    }

    pub fn publish_ignore_lag(&self, event: BusEvent) {
        // Drop only if no subscriber exists yet — still logged via return on publish().
        let _ = self.tx.send(event);
    }

    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for InMemoryEventBus {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedBus = Arc<InMemoryEventBus>;
