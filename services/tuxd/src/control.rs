//! Dashboard and control API. Mutations stay inside the Local Paper runtime.

use std::str::FromStr;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use tux_core::domain::{OrderIntent, OrderSide, OrderType, PositionSide, ReduceOnly, TimeInForce};
use tux_core::ids::{now_ms, IntentId};
use tux_core::oms::generate_client_order_id;
use tux_core::store::{FillRow, OrderRow, StrategyOperationRow};

use crate::dashboard_mod::DASHBOARD_HTML;
use crate::runtime::{SharedRuntime, StrategyParameters, StrategyRunStatus};

#[derive(Clone)]
pub struct AppState {
    pub rt: SharedRuntime,
}

#[derive(Debug, Deserialize)]
pub struct LimitQ {
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct StrategyConfigRequest {
    quantity: String,
    side: String,
    order_type: String,
    time_in_force: String,
    limit_offset_bps: String,
}

#[derive(Debug, Deserialize)]
struct StrategyControlRequest {
    action: String,
}

#[derive(Debug, Deserialize)]
struct ManualOrderRequest {
    side: String,
    order_type: String,
    time_in_force: Option<String>,
    quantity: String,
    price: Option<String>,
}

type ApiResult = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;

fn limit_or(value: Option<u32>, default: u32) -> u32 {
    value.unwrap_or(default).clamp(1, 500)
}

fn api_error(status: StatusCode, error: impl ToString) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "ok": false, "error": error.to_string() })),
    )
}

fn parse_decimal(value: &str, field: &str) -> std::result::Result<Decimal, String> {
    Decimal::from_str(value).map_err(|_| format!("invalid {field}"))
}

fn parse_side(value: &str) -> std::result::Result<OrderSide, String> {
    match value.to_ascii_lowercase().as_str() {
        "buy" => Ok(OrderSide::Buy),
        "sell" => Ok(OrderSide::Sell),
        _ => Err("side must be buy or sell".into()),
    }
}

fn parse_order_type(value: &str) -> std::result::Result<OrderType, String> {
    match value.to_ascii_lowercase().as_str() {
        "limit" => Ok(OrderType::Limit),
        "market" => Ok(OrderType::Market),
        "post_only" => Ok(OrderType::PostOnly),
        _ => Err("order_type must be limit, market, or post_only".into()),
    }
}

fn parse_tif(value: &str) -> std::result::Result<TimeInForce, String> {
    match value.to_ascii_lowercase().as_str() {
        "gtc" => Ok(TimeInForce::Gtc),
        "ioc" => Ok(TimeInForce::Ioc),
        "fok" => Ok(TimeInForce::Fok),
        _ => Err("time_in_force must be gtc, ioc, or fok".into()),
    }
}

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "service": "tuxd",
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "market": state.rt.current_market(),
    }))
}

async fn overview(State(state): State<AppState>) -> ApiResult {
    let rt = &state.rt;
    let book = {
        let portfolio = rt.portfolio.lock().unwrap();
        portfolio
            .pnl_book(&rt.account_id)
            .cloned()
            .unwrap_or_default()
    };
    let kill_switch = rt.kill_switch_engaged();
    let overview = rt
        .store
        .overview(rt.account_id.as_str(), Some(&book), kill_switch)
        .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let mut value = serde_json::to_value(overview)
        .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    if let Some(object) = value.as_object_mut() {
        object.insert("equity".into(), json!(rt.equity_mtm().to_string()));
        object.insert("kill_switch".into(), json!(kill_switch));
    }
    Ok(Json(value))
}

async fn balances(State(state): State<AppState>) -> Json<Value> {
    let rt = &state.rt;
    let portfolio = rt.portfolio.lock().unwrap();
    let rows = portfolio
        .balances_for(&rt.account_id)
        .into_iter()
        .map(|balance| {
            json!({
                "asset": balance.asset,
                "free": balance.free.to_string(),
                "locked": balance.locked.to_string(),
                "total": balance.total().to_string(),
            })
        })
        .collect::<Vec<_>>();
    Json(json!(rows))
}

async fn fills(
    State(state): State<AppState>,
    Query(query): Query<LimitQ>,
) -> std::result::Result<Json<Vec<FillRow>>, StatusCode> {
    state
        .rt
        .store
        .list_fills(limit_or(query.limit, 20))
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn orders(
    State(state): State<AppState>,
    Query(query): Query<LimitQ>,
) -> std::result::Result<Json<Vec<OrderRow>>, StatusCode> {
    state
        .rt
        .store
        .list_orders(limit_or(query.limit, 20))
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn equity(State(state): State<AppState>, Query(query): Query<LimitQ>) -> ApiResult {
    let points = state
        .rt
        .store
        .equity_series(state.rt.account_id.as_str(), limit_or(query.limit, 200))
        .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(Json(json!(points)))
}

async fn prices(State(state): State<AppState>, Query(query): Query<LimitQ>) -> ApiResult {
    let points = state
        .rt
        .store
        .price_series(state.rt.instrument.id.as_str(), limit_or(query.limit, 180))
        .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(Json(json!(points)))
}

async fn market(State(state): State<AppState>) -> ApiResult {
    state
        .rt
        .current_market()
        .map(|market| Json(json!(market)))
        .ok_or_else(|| api_error(StatusCode::SERVICE_UNAVAILABLE, "market data unavailable"))
}

async fn pnl(State(state): State<AppState>) -> Json<Value> {
    let rt = &state.rt;
    let portfolio = rt.portfolio.lock().unwrap();
    let book = portfolio
        .pnl_book(&rt.account_id)
        .cloned()
        .unwrap_or_default();
    Json(json!(book.components))
}

async fn strategy(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.rt.strategy_snapshot()))
}

async fn strategy_history(
    State(state): State<AppState>,
    Query(query): Query<LimitQ>,
) -> std::result::Result<Json<Vec<StrategyOperationRow>>, StatusCode> {
    state
        .rt
        .store
        .strategy_operations(
            state.rt.strategy_instance_id.as_str(),
            limit_or(query.limit, 50),
        )
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn update_strategy_config(
    State(state): State<AppState>,
    Json(request): Json<StrategyConfigRequest>,
) -> ApiResult {
    let parameters = StrategyParameters {
        quantity: parse_decimal(&request.quantity, "quantity")
            .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?,
        side: parse_side(&request.side)
            .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?,
        order_type: parse_order_type(&request.order_type)
            .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?,
        time_in_force: parse_tif(&request.time_in_force)
            .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?,
        limit_offset_bps: parse_decimal(&request.limit_offset_bps, "limit_offset_bps")
            .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?,
    };
    let snapshot = state
        .rt
        .configure_strategy(parameters)
        .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?;
    state
        .rt
        .record_strategy_operation(
            "update_config",
            "succeeded",
            "dashboard",
            &format!(
                "strategy configuration updated to revision {}",
                snapshot.revision
            ),
        )
        .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(Json(json!({ "ok": true, "strategy": snapshot })))
}

async fn control_strategy(
    State(state): State<AppState>,
    Json(request): Json<StrategyControlRequest>,
) -> ApiResult {
    let action = request.action.to_ascii_lowercase();
    let result = match action.as_str() {
        "pause" => {
            let snapshot = state
                .rt
                .set_strategy_status(StrategyRunStatus::Paused)
                .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
            json!({ "ok": true, "strategy": snapshot })
        }
        "resume" => {
            let snapshot = state
                .rt
                .set_strategy_status(StrategyRunStatus::Running)
                .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
            json!({ "ok": true, "strategy": snapshot })
        }
        "stop" => {
            let snapshot = state
                .rt
                .set_strategy_status(StrategyRunStatus::Stopped)
                .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
            json!({ "ok": true, "strategy": snapshot })
        }
        "trigger" => {
            let intent = state
                .rt
                .strategy_intent()
                .map_err(|error| api_error(StatusCode::CONFLICT, error))?;
            let order = match state.rt.execute_intent(intent).await {
                Ok(order) => order,
                Err(error) => {
                    let _ = state.rt.record_strategy_operation(
                        "trigger",
                        "failed",
                        "dashboard",
                        &error.to_string(),
                    );
                    return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, error));
                }
            };
            state
                .rt
                .note_strategy_order(&order)
                .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
            let _ = state.rt.persist_equity();
            json!({ "ok": true, "order": order })
        }
        _ => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "unsupported strategy action",
            ))
        }
    };
    state
        .rt
        .record_strategy_operation(
            &action,
            "succeeded",
            "dashboard",
            &format!("dashboard action {action}"),
        )
        .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(Json(result))
}

async fn manual_order(
    State(state): State<AppState>,
    Json(request): Json<ManualOrderRequest>,
) -> ApiResult {
    let side =
        parse_side(&request.side).map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?;
    let order_type = parse_order_type(&request.order_type)
        .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?;
    let time_in_force = parse_tif(request.time_in_force.as_deref().unwrap_or("gtc"))
        .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?;
    let quantity = parse_decimal(&request.quantity, "quantity")
        .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))?;
    let price = match order_type {
        OrderType::Market => None,
        _ => Some(
            request
                .price
                .as_deref()
                .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "price is required"))
                .and_then(|value| {
                    parse_decimal(value, "price")
                        .map_err(|error| api_error(StatusCode::BAD_REQUEST, error))
                })?,
        ),
    };
    let intent = OrderIntent {
        id: IntentId::new(),
        strategy_instance_id: state.rt.strategy_instance_id.clone(),
        account_id: state.rt.account_id.clone(),
        instrument_id: state.rt.instrument.id.clone(),
        side,
        order_type,
        time_in_force,
        price,
        quantity,
        reduce_only: ReduceOnly::No,
        position_side: PositionSide::Both,
        leverage: None,
        client_order_id: generate_client_order_id(&state.rt.strategy_instance_id),
        reason: Some("dashboard manual intervention".into()),
        created_at: now_ms(),
    };
    let order = match state.rt.execute_intent(intent).await {
        Ok(order) => order,
        Err(error) => {
            let _ = state.rt.record_strategy_operation(
                "manual_order",
                "failed",
                "dashboard",
                &error.to_string(),
            );
            return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, error));
        }
    };
    state
        .rt
        .note_strategy_order(&order)
        .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    state
        .rt
        .record_strategy_operation(
            "manual_order",
            "succeeded",
            "dashboard",
            &format!(
                "{} {} {} @ {}",
                request.side,
                request.quantity,
                request.order_type,
                request.price.as_deref().unwrap_or("market")
            ),
        )
        .map_err(|error| api_error(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let _ = state.rt.persist_equity();
    Ok(Json(json!({ "ok": true, "order": order })))
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
        .route("/api/market", get(market))
        .route("/api/pnl", get(pnl))
        .route("/api/strategy", get(strategy))
        .route("/api/strategy/history", get(strategy_history))
        .route("/api/strategy/config", put(update_strategy_config))
        .route("/api/strategy/control", post(control_strategy))
        .route("/api/manual/order", post(manual_order))
        .with_state(state)
}
