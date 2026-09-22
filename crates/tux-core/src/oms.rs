//! OMS + order state machine + router.

use std::collections::HashMap;

use crate::domain::{ExecutionPort, Order, OrderIntent, OrderStatus};
use crate::error::{Result, TuxError};
use crate::ids::{now_ms, Environment, MarketContext, OrderId, StrategyInstanceId, Venue};

pub struct OrderStateMachine;

impl OrderStateMachine {
    pub fn transition(order: &mut Order, to: OrderStatus) -> Result<()> {
        let from = order.status;
        order
            .apply_status(to, now_ms())
            .map_err(|_| TuxError::IllegalOrderTransition {
                from: from.to_string(),
                to: to.to_string(),
            })
    }

    pub fn mark_unknown(order: &mut Order, reason: impl Into<String>) -> Result<()> {
        let to = OrderStatus::Unknown;
        let from = order.status;
        order.reject_reason = Some(reason.into());
        order
            .apply_status(to, now_ms())
            .map_err(|_| TuxError::IllegalOrderTransition {
                from: from.to_string(),
                to: to.to_string(),
            })
    }
}

/// Globally unique client order id within venue length limits.
pub fn generate_client_order_id(instance_id: &StrategyInstanceId) -> crate::ids::ClientOrderId {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let short = {
        let s = instance_id.as_str();
        &s[s.len().saturating_sub(6)..]
    };
    crate::ids::ClientOrderId::from_raw(format!(
        "t{}{}{:04x}",
        now_ms() % 10_000_000_000,
        short,
        nanos % 0xffff
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    PaperMatching,
    Venue(Venue),
}

/// Routes by venue + product + environment. `split` always returns the whole
/// order (MVP has no TWAP); it never drops the order.
pub struct SmartOrderRouter;

impl SmartOrderRouter {
    pub fn route(ctx: &MarketContext) -> Route {
        match ctx.environment {
            Environment::ReplayBacktest | Environment::LocalPaper => Route::PaperMatching,
            Environment::ExchangeDemo | Environment::Live => Route::Venue(ctx.venue),
        }
    }

    /// MVP: single child equal to the parent. Future: TWAP/iceberg splits.
    pub fn split(order: &Order) -> Vec<Order> {
        vec![order.clone()]
    }
}

pub struct Oms {
    orders: HashMap<String, Order>,
    by_intent: HashMap<String, String>,
    by_clid: HashMap<String, String>,
}

impl Oms {
    pub fn new() -> Self {
        Self {
            orders: HashMap::new(),
            by_intent: HashMap::new(),
            by_clid: HashMap::new(),
        }
    }

    pub fn get(&self, id: &OrderId) -> Option<&Order> {
        self.orders.get(id.as_str())
    }

    pub fn active_orders(&self) -> impl Iterator<Item = &Order> {
        self.orders.values().filter(|o| o.status.is_active())
    }

    pub fn all(&self) -> impl Iterator<Item = &Order> {
        self.orders.values()
    }

    /// Create local order from a risk-approved intent.
    /// Idempotent on intent id / client_order_id; keeps strategy-supplied clOrdId.
    pub fn create_from_intent(&mut self, intent: &OrderIntent) -> Result<Order> {
        if let Some(id) = self.by_intent.get(intent.id.as_str()) {
            if let Some(o) = self.orders.get(id) {
                if o.side != intent.side || o.quantity != intent.quantity || o.price != intent.price
                {
                    return Err(TuxError::InvalidOrder(format!(
                        "intent {} payload conflict",
                        intent.id
                    )));
                }
                return Ok(o.clone());
            }
        }
        if let Some(id) = self.by_clid.get(intent.client_order_id.as_str()) {
            if let Some(o) = self.orders.get(id) {
                if o.side != intent.side || o.quantity != intent.quantity {
                    return Err(TuxError::InvalidOrder(format!(
                        "client_order_id {} payload conflict",
                        intent.client_order_id
                    )));
                }
                return Ok(o.clone());
            }
        }
        let now = now_ms();
        let mut order = Order {
            id: OrderId::new(),
            intent_id: Some(intent.id.clone()),
            client_order_id: intent.client_order_id.clone(),
            venue_order_id: None,
            account_id: intent.account_id.clone(),
            strategy_instance_id: Some(intent.strategy_instance_id.clone()),
            instrument_id: intent.instrument_id.clone(),
            side: intent.side,
            order_type: intent.order_type,
            time_in_force: intent.time_in_force,
            status: OrderStatus::Created,
            price: intent.price,
            quantity: intent.quantity,
            filled_quantity: rust_decimal::Decimal::ZERO,
            avg_fill_price: None,
            reduce_only: intent.reduce_only,
            position_side: intent.position_side,
            reject_reason: None,
            created_at: now,
            updated_at: now,
        };
        OrderStateMachine::transition(&mut order, OrderStatus::RiskApproved)?;
        self.index_order(&order);
        Ok(order)
    }

    fn index_order(&mut self, order: &Order) {
        self.orders
            .insert(order.id.as_str().to_string(), order.clone());
        if let Some(iid) = &order.intent_id {
            self.by_intent
                .insert(iid.as_str().to_string(), order.id.as_str().to_string());
        }
        self.by_clid.insert(
            order.client_order_id.as_str().to_string(),
            order.id.as_str().to_string(),
        );
    }

    pub async fn submit(&mut self, order_id: &OrderId, port: &dyn ExecutionPort) -> Result<Order> {
        let mut order = self
            .orders
            .get(order_id.as_str())
            .cloned()
            .ok_or_else(|| TuxError::InvalidOrder(format!("order {order_id} not found")))?;

        OrderStateMachine::transition(&mut order, OrderStatus::Submitted)?;

        match port.place_order(&order).await {
            Ok(placed) => {
                self.index_order(&placed);
                Ok(placed)
            }
            Err(e) => {
                OrderStateMachine::mark_unknown(&mut order, e.to_string())?;
                self.index_order(&order);
                Err(e)
            }
        }
    }

    pub async fn cancel(&mut self, order_id: &OrderId, port: &dyn ExecutionPort) -> Result<Order> {
        let cancelled = port.cancel_order(order_id).await?;
        self.orders
            .insert(cancelled.id.as_str().to_string(), cancelled.clone());
        Ok(cancelled)
    }

    pub fn apply_status(&mut self, order_id: &OrderId, status: OrderStatus) -> Result<()> {
        let order = self
            .orders
            .get_mut(order_id.as_str())
            .ok_or_else(|| TuxError::InvalidOrder(format!("order {order_id} not found")))?;
        OrderStateMachine::transition(order, status)
    }

    pub fn upsert(&mut self, order: Order) {
        self.index_order(&order);
    }
}

impl Default for Oms {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;
    use crate::ids::*;
    use rust_decimal::Decimal;

    fn sample() -> Order {
        let now = now_ms();
        Order {
            id: OrderId::new(),
            intent_id: None,
            client_order_id: ClientOrderId::from("c"),
            venue_order_id: None,
            account_id: AccountId::from("a"),
            strategy_instance_id: None,
            instrument_id: InstrumentId::new("SOL-USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            status: OrderStatus::Created,
            price: Some(Decimal::ONE),
            quantity: Decimal::ONE,
            filled_quantity: Decimal::ZERO,
            avg_fill_price: None,
            reduce_only: ReduceOnly::No,
            position_side: PositionSide::Both,
            reject_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn state_machine_happy_path() {
        let mut o = sample();
        OrderStateMachine::transition(&mut o, OrderStatus::RiskApproved).unwrap();
        OrderStateMachine::transition(&mut o, OrderStatus::Submitted).unwrap();
        OrderStateMachine::transition(&mut o, OrderStatus::Acknowledged).unwrap();
        OrderStateMachine::transition(&mut o, OrderStatus::Filled).unwrap();
        assert!(o.status.is_terminal());
    }

    #[test]
    fn router_split_keeps_order() {
        let o = sample();
        let kids = SmartOrderRouter::split(&o);
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].id, o.id);
    }
}
