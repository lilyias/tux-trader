//! Strategy SDK. Isolated from API keys; emits `OrderIntent` only.

use async_trait::async_trait;
use serde_json::Value;
use tux_core::domain::{OrderIntent, OrderSide, OrderType, PositionSide, ReduceOnly, StrategyConfig, TimeInForce};
use tux_core::error::Result;
use tux_core::events::MarketEvent;
use tux_core::ids::{now_ms, AccountId, ClientOrderId, Environment, IntentId, StrategyInstanceId, TimestampMs};

pub type StrategySnapshot = Value;

#[derive(Debug, Clone)]
pub struct StrategyContext {
    pub instance_id: StrategyInstanceId,
    pub account_id: AccountId,
    pub environment: Environment,
    pub config: StrategyConfig,
}

#[derive(Debug, Clone, Default)]
pub struct StrategyState {
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Operator,
    RiskHalt,
    Crash,
    Redeploy,
    Shutdown,
}

#[async_trait]
pub trait Strategy: Send + Sync {
    async fn on_start(&mut self, context: &StrategyContext) -> Result<()>;

    async fn on_market(&mut self, event: &MarketEvent, state: &StrategyState) -> Result<Vec<OrderIntent>> {
        let _ = (event, state);
        Ok(Vec::new())
    }

    async fn on_config_update(&mut self, _old: &StrategyConfig, _new: &StrategyConfig) -> Result<()> {
        Ok(())
    }

    async fn on_timer(&mut self, _time: TimestampMs) -> Result<Vec<OrderIntent>> {
        Ok(Vec::new())
    }

    fn snapshot(&self) -> StrategySnapshot {
        Value::Null
    }

    fn restore(&mut self, _snap: &StrategySnapshot) -> Result<()> {
        Ok(())
    }

    async fn on_stop(&mut self, _reason: StopReason) -> Result<()> {
        Ok(())
    }
}

pub struct StrategyHost {
    pub context: StrategyContext,
    strategy: Box<dyn Strategy>,
    state: StrategyState,
}

impl StrategyHost {
    pub fn new(context: StrategyContext, strategy: Box<dyn Strategy>) -> Self {
        Self {
            context,
            strategy,
            state: StrategyState::default(),
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        self.strategy.on_start(&self.context).await
    }

    pub async fn on_market(&mut self, event: &MarketEvent) -> Result<Vec<OrderIntent>> {
        self.strategy.on_market(event, &self.state).await
    }

    pub async fn on_timer(&mut self, t: TimestampMs) -> Result<Vec<OrderIntent>> {
        self.strategy.on_timer(t).await
    }

    pub async fn update_config(&mut self, old: &StrategyConfig, new: &StrategyConfig) -> Result<()> {
        self.strategy.on_config_update(old, new).await?;
        self.context.config = new.clone();
        Ok(())
    }

    pub async fn stop(&mut self, reason: StopReason) -> Result<()> {
        self.strategy.on_stop(reason).await
    }
}

/// Demo strategy: buy once on first book tick (proves the full pipeline).
pub struct OneShotBuyStrategy {
    sent: bool,
    qty: rust_decimal::Decimal,
}

impl OneShotBuyStrategy {
    pub fn new(qty: rust_decimal::Decimal) -> Self {
        Self { sent: false, qty }
    }
}

#[async_trait]
impl Strategy for OneShotBuyStrategy {
    async fn on_start(&mut self, _context: &StrategyContext) -> Result<()> {
        Ok(())
    }

    async fn on_market(&mut self, event: &MarketEvent, _state: &StrategyState) -> Result<Vec<OrderIntent>> {
        if self.sent {
            return Ok(Vec::new());
        }
        let tick = match event {
            MarketEvent::BookTicker(t) => t,
            MarketEvent::Trade(_) => return Ok(Vec::new()),
        };
        self.sent = true;
        Ok(vec![OrderIntent {
            id: IntentId::new(),
            strategy_instance_id: StrategyInstanceId::from("oneshot"),
            account_id: AccountId::from("paper_acc"),
            instrument_id: tick.instrument_id.clone(),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            price: Some(tick.ask_price),
            quantity: self.qty,
            reduce_only: ReduceOnly::No,
            position_side: PositionSide::Both,
            leverage: None,
            client_order_id: ClientOrderId::new(),
            reason: Some("oneshot buy".into()),
            created_at: now_ms(),
        }])
    }
}
