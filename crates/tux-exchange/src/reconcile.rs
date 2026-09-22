//! Account reconciliation after start / reconnect / timeout / crash.

use tux_core::domain::{Balance, Order, Position};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReconcileReport {
    pub open_orders_local: usize,
    pub open_orders_venue: usize,
    pub mismatches: Vec<String>,
}

impl ReconcileReport {
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
    }
}

pub struct Reconciler;

impl Reconciler {
    pub fn diff_orders(local: &[Order], venue: &[Order]) -> Vec<String> {
        let mut out = Vec::new();
        let local_ids: std::collections::HashSet<_> =
            local.iter().map(|o| o.client_order_id.as_str().to_string()).collect();
        let venue_ids: std::collections::HashSet<_> =
            venue.iter().map(|o| o.client_order_id.as_str().to_string()).collect();
        for id in venue_ids.difference(&local_ids) {
            out.push(format!("order only on venue: {id}"));
        }
        for id in local_ids.difference(&venue_ids) {
            out.push(format!("order only local: {id}"));
        }
        out
    }

    pub fn diff_balances(local: &[Balance], venue: &[Balance]) -> Vec<String> {
        use std::collections::HashMap;
        let mut map: HashMap<String, (rust_decimal::Decimal, rust_decimal::Decimal)> = HashMap::new();
        for b in local {
            map.entry(b.asset.clone()).or_default().0 = b.total();
        }
        for b in venue {
            map.entry(b.asset.clone()).or_default().1 = b.total();
        }
        map.into_iter()
            .filter(|(_, (l, v))| l != v)
            .map(|(asset, (l, v))| format!("balance mismatch {asset}: local={l} venue={v}"))
            .collect()
    }

    pub fn diff_positions(local: &[Position], venue: &[Position]) -> Vec<String> {
        use std::collections::HashMap;
        let mut map: HashMap<String, (rust_decimal::Decimal, rust_decimal::Decimal)> = HashMap::new();
        for p in local {
            map.entry(p.instrument_id.as_str().to_string()).or_default().0 = p.size;
        }
        for p in venue {
            map.entry(p.instrument_id.as_str().to_string()).or_default().1 = p.size;
        }
        map.into_iter()
            .filter(|(_, (l, v))| l != v)
            .map(|(inst, (l, v))| format!("position mismatch {inst}: local={l} venue={v}"))
            .collect()
    }
}
