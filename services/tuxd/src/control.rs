//! Control API + dashboard routes (feature = "control-api").
//! Reads the SAME RuntimeState as the trading pipeline.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use tux_core::store::{FillRow, OrderRow};

use crate::dashboard_mod::DASHBOARD_HTML;
use crate::runtime::SharedRuntime;

#[derive(Clone)]
pub struct AppState {
    pub rt: SharedRuntime,
}

#[derive(Debug, Deserialize)]
pub struct LimitQ {
    pub limit: Option<u32>,
}

fn limit_or(q: Option<u32>, default: u32) -> u32 {
    q.unwrap_or(default).clamp(1, 500)
}

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "service": "tuxd",
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn overview(State(st): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let rt = &st.rt;
    let book = {
        let p = rt.portfolio.lock().unwrap();
        p.pnl_book(&rt.account_id).cloned().unwrap_or_default()
    };
    let kill = rt.kill_switch_engaged();
    let ov = rt
        .store
        .overview(rt.account_id.as_str(), Some(&book), kill)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut v = serde_json::to_value(ov).unwrap_or_default();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("equity".into(), serde_json::json!(rt.equity_mtm().to_string()));
        obj.insert("kill_switch".into(), serde_json::json!(kill));
    }
    Ok(Json(v))
}

async fn balances(State(st): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let rt = &st.rt;
    let p = rt.portfolio.lock().unwrap();
    let rows: Vec<_> = p
        .balances_for(&rt.account_id)
        .into_iter()
        .map(|b| {
            serde_json::json!({
                "asset": b.asset,
                "free": b.free.to_string(),
                "locked": b.locked.to_string(),
                "total": b.total().to_string(),
            })
        })
        .collect();
    Ok(Json(serde_json::json!(rows)))
}

async fn fills(
    State(st): State<AppState>,
    Query(q): Query<LimitQ>,
) -> Result<Json<Vec<FillRow>>, StatusCode> {
    st.rt
        .store
        .list_fills(limit_or(q.limit, 50))
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn orders(
    State(st): State<AppState>,
    Query(q): Query<LimitQ>,
) -> Result<Json<Vec<OrderRow>>, StatusCode> {
    st.rt
        .store
        .list_orders(limit_or(q.limit, 50))
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn equity(
    State(st): State<AppState>,
    Query(q): Query<LimitQ>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pts = st
        .rt
        .store
        .equity_series(st.rt.account_id.as_str(), limit_or(q.limit, 200))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::to_value(pts).unwrap_or_default()))
}

async fn prices(
    State(st): State<AppState>,
    Query(q): Query<LimitQ>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pts = st
        .rt
        .store
        .price_series("SOL-USDT", limit_or(q.limit, 120))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::to_value(pts).unwrap_or_default()))
}

async fn pnl(State(st): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let rt = &st.rt;
    let p = rt.portfolio.lock().unwrap();
    let book = p.pnl_book(&rt.account_id).cloned().unwrap_or_default();
    Ok(Json(serde_json::to_value(book.components).unwrap_or_default()))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/api/overview", get(overview))
        .route("/api/balances", get(balances))
        .route("/api/fills", get(fills))
        .route("/api/orders", get(orders))
        .route("/api/equity", get(equity))
        .route("/api/prices", get(prices))
        .route("/api/pnl", get(pnl))
        .with_state(state)
}
