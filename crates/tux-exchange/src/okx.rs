//! OKX adapter skeleton (feature = "okx").
//! Demo REST needs the simulated-trading header and Demo WebSocket hosts.

use tux_core::ids::Venue;

use crate::{VenueCredentials, VenueEndpoints};

pub struct OkxAdapter {
    pub endpoints: VenueEndpoints,
    pub product: tux_core::ids::Product,
    #[allow(dead_code)]
    pub credentials: Option<VenueCredentials>,
}

impl OkxAdapter {
    pub fn demo() -> Self {
        Self {
            endpoints: VenueEndpoints {
                rest_base: "https://www.okx.com".into(),
                ws_public: "wss://wspap.okx.com:8443/ws/v5/public".into(),
                ws_private: "wss://wspap.okx.com:8443/ws/v5/private".into(),
                simulated: true,
            },
            product: tux_core::ids::Product::Spot,
            credentials: None,
        }
    }

    pub fn production() -> Self {
        Self {
            endpoints: VenueEndpoints {
                rest_base: "https://www.okx.com".into(),
                ws_public: "wss://ws.okx.com:8443/ws/v5/public".into(),
                ws_private: "wss://ws.okx.com:8443/ws/v5/private".into(),
                simulated: false,
            },
            product: tux_core::ids::Product::Spot,
            credentials: None,
        }
    }

    /// Value for `x-simulated-trading` header when Demo.
    pub fn simulated_header(&self) -> Option<&'static str> {
        if self.endpoints.simulated {
            Some("1")
        } else {
            None
        }
    }

    pub fn venue(&self) -> Venue {
        Venue::Okx
    }
}
