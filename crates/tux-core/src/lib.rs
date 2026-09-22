//! TUX trading core (single library for the local MVP).
//!
//! Pipeline: market → strategy intent → risk → OMS → paper/venue → portfolio.

pub mod decimal;
pub mod domain;
pub mod error;
pub mod events;
pub mod ids;
pub mod market;
pub mod oms;
pub mod portfolio;
pub mod risk;
pub mod sim;
pub mod store;

pub use error::{Result, TuxError};
pub use ids::{
    AccountId, ClientOrderId, ConfigVersionId, Environment, EventSeq, FillId, IntentId, MarketContext,
    OrderId, Product, StrategyId, StrategyInstanceId, TimestampMs, Venue,
};

/// Live trading is refused until every risk control is production-validated.
pub fn ensure_not_live(env: Environment) -> Result<()> {
    if env == Environment::Live {
        return Err(TuxError::Config(
            "Live mode is disabled: finish risk validation before enabling real trading".into(),
        ));
    }
    Ok(())
}
