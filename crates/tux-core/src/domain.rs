//! Unified domain model.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::ids::{
    AccountId, ClientOrderId, ConfigVersionId, FillId, IntentId, OrderId, StrategyId,
    StrategyInstanceId, TimestampMs, Venue,
};

// ── Instrument ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstrumentId(pub String);

impl InstrumentId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    pub id: InstrumentId,
    pub venue: Venue,
    pub product: crate::ids::Product,
    pub base_asset: String,
    pub quote_asset: String,
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub min_qty: Decimal,
    pub min_notional: Option<Decimal>,
    /// Spot = 1. Contracts: contracts → underlying.
    pub contract_multiplier: Decimal,
    pub margin_asset: String,
    pub settle_asset: String,
    pub supports_funding: bool,
}

impl Instrument {
    pub fn spot(
        id: impl Into<String>,
        venue: Venue,
        base: impl Into<String>,
        quote: impl Into<String>,
        tick_size: Decimal,
        step_size: Decimal,
    ) -> Self {
        let quote_asset = quote.into();
        Self {
            id: InstrumentId::new(id),
            venue,
            product: crate::ids::Product::Spot,
            base_asset: base.into(),
            quote_asset: quote_asset.clone(),
            tick_size,
            step_size,
            min_qty: step_size,
            min_notional: None,
            contract_multiplier: Decimal::ONE,
            margin_asset: quote_asset.clone(),
            settle_asset: quote_asset,
            supports_funding: false,
        }
    }

    pub fn validate_price(&self, price: Decimal) -> bool {
        price > Decimal::ZERO && crate::decimal::is_multiple_of(price, self.tick_size)
    }

    pub fn validate_qty(&self, qty: Decimal) -> bool {
        qty >= self.min_qty && crate::decimal::is_multiple_of(qty, self.step_size)
    }

    pub fn validate_notional(&self, notional: Decimal) -> bool {
        self.min_notional.map(|m| notional >= m).unwrap_or(true)
    }
}

// ── Account / Position / Exposure ───────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Paper,
    Demo,
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub venue: Venue,
    pub kind: AccountKind,
    pub label: String,
    pub trading_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    pub account_id: AccountId,
    pub asset: String,
    pub free: Decimal,
    pub locked: Decimal,
    pub updated_at: TimestampMs,
}

impl Balance {
    pub fn total(&self) -> Decimal {
        self.free + self.locked
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionMode {
    OneWay,
    Hedge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarginMode {
    Cross,
    Isolated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub account_id: AccountId,
    pub instrument_id: InstrumentId,
    pub position_mode: PositionMode,
    pub margin_mode: MarginMode,
    pub position_side: PositionSide,
    pub size: Decimal,
    pub entry_price: Decimal,
    pub mark_price: Decimal,
    pub liquidation_price: Option<Decimal>,
    pub leverage: Decimal,
    pub unrealized_pnl: Decimal,
    pub margin: Decimal,
    pub updated_at: TimestampMs,
}

/// Unified risk view across spot and derivatives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exposure {
    pub account_id: AccountId,
    pub instrument_id: InstrumentId,
    pub net_notional: Decimal,
    pub gross_notional: Decimal,
    pub updated_at: TimestampMs,
}

// ── Orders ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Fok,
    Gtd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Limit,
    Market,
    PostOnly,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionSide {
    Both,
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReduceOnly {
    No,
    Yes,
}

/// ```text
/// Created → RiskApproved → Submitted → Acknowledged
///               ├→ PartiallyFilled → Filled
///               ├→ CancelPending → Cancelled
///               ├→ Rejected
///               └→ Unknown → Reconcile
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Created,
    RiskApproved,
    Submitted,
    Acknowledged,
    PartiallyFilled,
    Filled,
    CancelPending,
    Cancelled,
    Rejected,
    Unknown,
    Reconciled,
}

impl OrderStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OrderStatus::Filled
                | OrderStatus::Cancelled
                | OrderStatus::Rejected
                | OrderStatus::Reconciled
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            OrderStatus::Submitted
                | OrderStatus::Acknowledged
                | OrderStatus::PartiallyFilled
                | OrderStatus::CancelPending
        )
    }

    pub fn can_transition_to(self, to: OrderStatus) -> bool {
        use OrderStatus::*;
        matches!(
            (self, to),
            (Created, RiskApproved | Rejected)
                | (RiskApproved, Submitted | Rejected)
                | (
                    Submitted,
                    Acknowledged
                        | PartiallyFilled
                        | Filled
                        | CancelPending
                        | Cancelled
                        | Rejected
                        | Unknown
                )
                | (
                    Acknowledged,
                    PartiallyFilled | Filled | CancelPending | Cancelled | Rejected | Unknown
                )
                | (
                    PartiallyFilled,
                    PartiallyFilled | Filled | CancelPending | Cancelled | Unknown
                )
                | (
                    CancelPending,
                    Cancelled | PartiallyFilled | Filled | Unknown
                )
                | (
                    Unknown,
                    Reconciled
                        | Submitted
                        | Acknowledged
                        | PartiallyFilled
                        | Filled
                        | Cancelled
                        | Rejected
                )
        )
    }
}

impl std::fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OrderStatus::Created => "created",
            OrderStatus::RiskApproved => "risk_approved",
            OrderStatus::Submitted => "submitted",
            OrderStatus::Acknowledged => "acknowledged",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Filled => "filled",
            OrderStatus::CancelPending => "cancel_pending",
            OrderStatus::Cancelled => "cancelled",
            OrderStatus::Rejected => "rejected",
            OrderStatus::Unknown => "unknown",
            OrderStatus::Reconciled => "reconciled",
        })
    }
}

/// What a strategy emits. Strategies never place orders directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderIntent {
    pub id: IntentId,
    pub strategy_instance_id: StrategyInstanceId,
    pub account_id: AccountId,
    pub instrument_id: InstrumentId,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    /// Limit price. For market orders this may be None — notional must use a reference price.
    pub price: Option<Decimal>,
    pub quantity: Decimal,
    pub reduce_only: ReduceOnly,
    pub position_side: PositionSide,
    pub leverage: Option<Decimal>,
    pub client_order_id: ClientOrderId,
    pub reason: Option<String>,
    pub created_at: TimestampMs,
}

impl OrderIntent {
    /// Reference price for notional: limit price, else market reference.
    /// Market orders without a reference price cannot be risk-checked and must be rejected.
    pub fn reference_price(&self, market_ref: Option<Decimal>) -> Option<Decimal> {
        self.price.or(market_ref)
    }

    pub fn notional(&self, market_ref: Option<Decimal>) -> Option<Decimal> {
        self.reference_price(market_ref).map(|p| p * self.quantity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub intent_id: Option<IntentId>,
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<String>,
    pub account_id: AccountId,
    pub strategy_instance_id: Option<StrategyInstanceId>,
    pub instrument_id: InstrumentId,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub status: OrderStatus,
    pub price: Option<Decimal>,
    pub quantity: Decimal,
    pub filled_quantity: Decimal,
    pub avg_fill_price: Option<Decimal>,
    pub reduce_only: ReduceOnly,
    pub position_side: PositionSide,
    pub reject_reason: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

impl Order {
    pub fn remaining(&self) -> Decimal {
        self.quantity - self.filled_quantity
    }

    pub fn apply_status(
        &mut self,
        to: OrderStatus,
        now: TimestampMs,
    ) -> std::result::Result<(), String> {
        if self.status == to {
            self.updated_at = now;
            return Ok(());
        }
        if !self.status.can_transition_to(to) {
            return Err(format!("illegal transition {} -> {}", self.status, to));
        }
        self.status = to;
        self.updated_at = now;
        Ok(())
    }

    /// Notional at limit price or provided reference (for market orders).
    pub fn notional(&self, market_ref: Option<Decimal>) -> Option<Decimal> {
        self.price.or(market_ref).map(|p| p * self.quantity)
    }
}

// ── Fills / PnL ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeSide {
    Base,
    Quote,
    Settle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fee {
    pub asset: String,
    pub amount: Decimal,
    pub side: FeeSide,
    pub is_maker: bool,
    pub rate: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub id: FillId,
    pub account_id: AccountId,
    pub instrument_id: InstrumentId,
    pub order_id: OrderId,
    pub venue_order_id: Option<String>,
    pub trade_id: String,
    pub side: OrderSide,
    pub position_side: PositionSide,
    pub price: Decimal,
    pub quantity: Decimal,
    pub fee: Fee,
    pub is_maker: bool,
    pub occurred_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Funding {
    pub account_id: AccountId,
    pub instrument_id: InstrumentId,
    pub rate: Decimal,
    pub payment: Decimal,
    pub is_payer: bool,
    pub occurred_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PnLComponents {
    pub gross_realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub trading_fees: Decimal,
    pub funding_fees: Decimal,
    pub borrow_interest: Decimal,
}

impl PnLComponents {
    pub fn net_pnl(&self) -> Decimal {
        self.gross_realized_pnl - self.trading_fees + self.funding_fees - self.borrow_interest
    }
}

// ── Risk config ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskDecision {
    Approved,
    Rejected { rule: String, message: String },
}

impl RiskDecision {
    pub fn reject(rule: impl Into<String>, message: impl Into<String>) -> Self {
        RiskDecision::Rejected {
            rule: rule.into(),
            message: message.into(),
        }
    }
    pub fn is_approved(&self) -> bool {
        matches!(self, RiskDecision::Approved)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskLimitConfig {
    /// Empty whitelist = deny all.
    pub instrument_whitelist: Vec<InstrumentId>,
    pub max_order_notional: Option<Decimal>,
    pub max_strategy_notional: Option<Decimal>,
    pub max_account_notional: Option<Decimal>,
    pub max_net_exposure: Option<Decimal>,
    pub max_leverage: Option<Decimal>,
    pub max_active_orders: Option<u32>,
    pub max_order_rate_per_min: Option<u32>,
    pub max_price_deviation: Option<Decimal>,
    pub max_market_age_ms: Option<u64>,
    pub max_daily_loss: Option<Decimal>,
    pub max_drawdown: Option<Decimal>,
    pub consecutive_reject_limit: Option<u32>,
    pub pause_on_disconnect: bool,
}

impl Default for RiskLimitConfig {
    fn default() -> Self {
        Self {
            instrument_whitelist: Vec::new(),
            max_order_notional: None,
            max_strategy_notional: None,
            max_account_notional: None,
            max_net_exposure: None,
            max_leverage: None,
            max_active_orders: None,
            max_order_rate_per_min: None,
            max_price_deviation: None,
            max_market_age_ms: Some(5_000),
            max_daily_loss: None,
            max_drawdown: None,
            consecutive_reject_limit: Some(5),
            pause_on_disconnect: true,
        }
    }
}

// ── Strategy metadata ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyLifecycle {
    Draft,
    Backtest,
    Paper,
    ExchangeDemo,
    Shadow,
    Canary,
    Live,
    Paused,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub version: ConfigVersionId,
    pub params: serde_json::Value,
    pub effective_event_seq: Option<u64>,
    pub applied_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyInstance {
    pub id: StrategyInstanceId,
    pub definition_id: StrategyId,
    pub lifecycle: StrategyLifecycle,
    pub config_version: ConfigVersionId,
    pub environment: crate::ids::Environment,
    pub account_id: AccountId,
    pub enabled: bool,
    pub created_at: TimestampMs,
}

// ── Execution ports (used by OMS / paper) ───────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FillModelKind {
    Immediate,
    L1,
    Queue,
}

#[async_trait::async_trait]
pub trait ExecutionPort: Send + Sync {
    async fn place_order(&self, order: &Order) -> crate::error::Result<Order>;
    async fn cancel_order(&self, order_id: &OrderId) -> crate::error::Result<Order>;
    async fn query_order(&self, order_id: &OrderId) -> crate::error::Result<Order>;
    fn validate_client_order_id(&self, id: &ClientOrderId) -> bool;
}
