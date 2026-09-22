//! Binance adapter skeleton (feature = "binance").
//! Endpoints configurable: Testnet ≠ production; spot testnet ≠ futures testnet.

use tux_core::ids::Venue;

use crate::{VenueEndpoints, VenueCredentials};

pub struct BinanceAdapter {
    pub endpoints: VenueEndpoints,
    pub product: tux_core::ids::Product,
    #[allow(dead_code)]
    pub credentials: Option<VenueCredentials>,
}

impl BinanceAdapter {
    pub fn spot_testnet() -> Self {
        Self {
            endpoints: VenueEndpoints {
                rest_base: "https://testnet.binance.vision".into(),
                ws_public: "wss://stream.testnet.binance.vision".into(),
                ws_private: "wss://stream.testnet.binance.vision/ws".into(),
                simulated: false,
            },
            product: tux_core::ids::Product::Spot,
            credentials: None,
        }
    }

    pub fn spot_production() -> Self {
        Self {
            endpoints: VenueEndpoints {
                rest_base: "https://api.binance.com".into(),
                ws_public: "wss://stream.binance.com:9443".into(),
                ws_private: "wss://stream.binance.com:9443/ws".into(),
                simulated: false,
            },
            product: tux_core::ids::Product::Spot,
            credentials: None,
        }
    }

    pub fn venue(&self) -> Venue {
        Venue::Binance
    }
}

// Full port impls land with the Stage-1 venue work. Skeleton keeps compile surface only.
