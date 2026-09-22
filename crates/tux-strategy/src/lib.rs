//! Strategy SDK. Isolated from API keys; emits `OrderIntent` only.

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::Value;
use tux_core::domain::{
    OrderIntent, OrderSide, OrderType, PositionSide, ReduceOnly, StrategyConfig, TimeInForce,
};
use tux_core::error::{Result, TuxError};
use tux_core::events::MarketEvent;
use tux_core::ids::{now_ms, AccountId, Environment, IntentId, StrategyInstanceId, TimestampMs};
use tux_core::oms::generate_client_order_id;

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

    async fn on_market(
        &mut self,
        event: &MarketEvent,
        state: &StrategyState,
    ) -> Result<Vec<OrderIntent>> {
        let _ = (event, state);
        Ok(Vec::new())
    }

    async fn on_config_update(
        &mut self,
        _old: &StrategyConfig,
        _new: &StrategyConfig,
    ) -> Result<()> {
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

    pub async fn update_config(
        &mut self,
        old: &StrategyConfig,
        new: &StrategyConfig,
    ) -> Result<()> {
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
    qty: Decimal,
    side: OrderSide,
    order_type: OrderType,
    time_in_force: TimeInForce,
    limit_offset_bps: Decimal,
    instance_id: Option<StrategyInstanceId>,
    account_id: Option<AccountId>,
}

impl OneShotBuyStrategy {
    pub fn new(qty: Decimal) -> Self {
        Self {
            sent: false,
            qty,
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            limit_offset_bps: Decimal::ZERO,
            instance_id: None,
            account_id: None,
        }
    }
}

#[derive(serde::Deserialize)]
struct OneShotParameters {
    side: OrderSide,
    order_type: OrderType,
    time_in_force: TimeInForce,
    quantity: Decimal,
    limit_offset_bps: Decimal,
}

#[async_trait]
impl Strategy for OneShotBuyStrategy {
    async fn on_start(&mut self, context: &StrategyContext) -> Result<()> {
        self.instance_id = Some(context.instance_id.clone());
        self.account_id = Some(context.account_id.clone());
        if let Ok(parameters) =
            serde_json::from_value::<OneShotParameters>(context.config.params.clone())
        {
            self.qty = parameters.quantity;
            self.side = parameters.side;
            self.order_type = parameters.order_type;
            self.time_in_force = parameters.time_in_force;
            self.limit_offset_bps = parameters.limit_offset_bps;
        }
        Ok(())
    }

    async fn on_config_update(
        &mut self,
        _old: &StrategyConfig,
        new: &StrategyConfig,
    ) -> Result<()> {
        let parameters: OneShotParameters = serde_json::from_value(new.params.clone())
            .map_err(|error| TuxError::Config(format!("strategy parameters: {error}")))?;
        self.qty = parameters.quantity;
        self.side = parameters.side;
        self.order_type = parameters.order_type;
        self.time_in_force = parameters.time_in_force;
        self.limit_offset_bps = parameters.limit_offset_bps;
        Ok(())
    }

    async fn on_market(
        &mut self,
        event: &MarketEvent,
        _state: &StrategyState,
    ) -> Result<Vec<OrderIntent>> {
        if self.sent {
            return Ok(Vec::new());
        }
        let tick = match event {
            MarketEvent::BookTicker(t) => t,
            MarketEvent::Trade(_) => return Ok(Vec::new()),
        };
        self.sent = true;
        let instance_id = self
            .instance_id
            .clone()
            .ok_or_else(|| TuxError::Config("strategy not started".into()))?;
        let account_id = self
            .account_id
            .clone()
            .ok_or_else(|| TuxError::Config("strategy not started".into()))?;
        let reference = match self.side {
            OrderSide::Buy => tick.ask_price,
            OrderSide::Sell => tick.bid_price,
        };
        let fraction = self.limit_offset_bps / Decimal::new(10_000, 0);
        let price = match self.order_type {
            OrderType::Market => None,
            _ => Some(match self.side {
                OrderSide::Buy => reference * (Decimal::ONE + fraction),
                OrderSide::Sell => reference * (Decimal::ONE - fraction),
            }),
        };
        let client_order_id = generate_client_order_id(&instance_id);
        Ok(vec![OrderIntent {
            id: IntentId::new(),
            strategy_instance_id: instance_id,
            account_id,
            instrument_id: tick.instrument_id.clone(),
            side: self.side,
            order_type: self.order_type,
            time_in_force: self.time_in_force,
            price,
            quantity: self.qty,
            reduce_only: ReduceOnly::No,
            position_side: PositionSide::Both,
            leverage: None,
            client_order_id,
            reason: Some("oneshot buy".into()),
            created_at: now_ms(),
        }])
    }
}
