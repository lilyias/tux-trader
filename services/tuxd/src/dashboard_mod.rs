//! Built-in TUX Console (lite): dashboard + JSON API.
//!
//! Self-contained HTML (no CDN). Served by `tuxd` when `control-api` is on.

pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");
