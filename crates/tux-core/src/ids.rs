//! Strongly-typed identifiers and environment triple.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Wall-clock milliseconds since Unix epoch (UTC).
pub type TimestampMs = i64;

/// Monotonic event sequence (config changes apply on event boundaries).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct EventSeq(pub u64);

impl fmt::Display for EventSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub fn now_ms() -> TimestampMs {
    chrono::Utc::now().timestamp_millis()
}

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::new_v4().simple()))
            }
            pub fn from_raw(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl From<String> for $name {
            fn from(raw: String) -> Self {
                Self(raw)
            }
        }
        impl From<&str> for $name {
            fn from(raw: &str) -> Self {
                Self(raw.to_string())
            }
        }
    };
}

id_type!(AccountId, "acc");
id_type!(StrategyId, "strat");
id_type!(StrategyInstanceId, "sinst");
id_type!(OrderId, "ord");
id_type!(ClientOrderId, "clid");
id_type!(FillId, "fill");
id_type!(IntentId, "int");
id_type!(ConfigVersionId, "cfg");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Binance,
    Okx,
    LocalPaper,
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Venue::Binance => write!(f, "binance"),
            Venue::Okx => write!(f, "okx"),
            Venue::LocalPaper => write!(f, "local_paper"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    Spot,
    UsdtPerp,
    CoinPerp,
    CoinFutures,
}

impl fmt::Display for Product {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Product::Spot => write!(f, "spot"),
            Product::UsdtPerp => write!(f, "usdt_perp"),
            Product::CoinPerp => write!(f, "coin_perp"),
            Product::CoinFutures => write!(f, "coin_futures"),
        }
    }
}

/// Runtime environment. Never collapse to a single `demo` boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    ReplayBacktest,
    LocalPaper,
    ExchangeDemo,
    Live,
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Environment::ReplayBacktest => write!(f, "replay_backtest"),
            Environment::LocalPaper => write!(f, "local_paper"),
            Environment::ExchangeDemo => write!(f, "exchange_demo"),
            Environment::Live => write!(f, "live"),
        }
    }
}

/// Config identity triple: venue + product + environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarketContext {
    pub venue: Venue,
    pub product: Product,
    pub environment: Environment,
}

impl MarketContext {
    pub const fn new(venue: Venue, product: Product, environment: Environment) -> Self {
        Self {
            venue,
            product,
            environment,
        }
    }

    pub fn is_simulated_matching(&self) -> bool {
        matches!(
            self.environment,
            Environment::ReplayBacktest | Environment::LocalPaper
        )
    }
}

impl fmt::Display for MarketContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.venue, self.product, self.environment)
    }
}
