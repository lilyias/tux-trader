//! Exchange adapter ports and optional venue adapters.
//!
//! Default build has **no** venue code (local Paper only).
//! Enable `binance` / `okx` features when wiring real venues.

use tux_core::domain::{Balance, Fill, Instrument, Order, Position};
use tux_core::error::Result;
use tux_core::ids::Venue;

#[cfg(feature = "binance")]
pub mod binance;
#[cfg(feature = "okx")]
pub mod okx;

pub mod ratelimit;
pub mod reconcile;

/// REST/WS base URLs — fully configurable (Testnet ≠ Live; OKX Demo ≠ Live).
#[derive(Debug, Clone)]
pub struct VenueEndpoints {
    pub rest_base: String,
    pub ws_public: String,
    pub ws_private: String,
    /// Extra flag for venues with a simulated-trading mode (OKX Demo header).
    pub simulated: bool,
}

pub struct VenueCredentials {
    pub api_key: String,
    pub api_secret: String,
    pub passphrase: Option<String>,
}

/// Market data feed.
#[async_trait::async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn fetch_recent_trades(
        &self,
        instrument: &tux_core::domain::InstrumentId,
        limit: u32,
    ) -> Result<Vec<tux_core::events::TradeTick>>;
}

/// Private account snapshot.
#[async_trait::async_trait]
pub trait AccountPort: Send + Sync {
    async fn fetch_balances(&self) -> Result<Vec<Balance>>;
    async fn fetch_positions(&self) -> Result<Vec<Position>>;
    async fn fetch_open_orders(&self) -> Result<Vec<Order>>;
}

/// Order placement (paper engine implements the same trait in tux-core).
#[async_trait::async_trait]
pub trait VenueExecutionPort: Send + Sync {
    async fn place_order(&self, order: &Order) -> Result<Order>;
    async fn cancel_order(&self, order_id: &tux_core::ids::OrderId) -> Result<Order>;
    async fn query_order(&self, order_id: &tux_core::ids::OrderId) -> Result<Order>;
}

#[async_trait::async_trait]
pub trait InstrumentMetadataPort: Send + Sync {
    async fn list_instruments(&self) -> Result<Vec<Instrument>>;
}

#[async_trait::async_trait]
pub trait FeeSchedulePort: Send + Sync {
    async fn trading_fee(
        &self,
        instrument: &tux_core::domain::InstrumentId,
        is_maker: bool,
    ) -> Result<tux_core::domain::Fee>;
}

#[async_trait::async_trait]
pub trait VenueAdapter:
    MarketDataPort + AccountPort + VenueExecutionPort + InstrumentMetadataPort + FeeSchedulePort
{
    fn venue(&self) -> Venue;
    async fn server_time_ms(&self) -> Result<i64>;
    /// Startup: buffer private WS → REST snapshot → merge post-snapshot deltas.
    async fn reconcile(&self) -> Result<reconcile::ReconcileReport>;
    #[allow(dead_code)]
    fn sample_fill(&self) -> Option<Fill> {
        None
    }
}
