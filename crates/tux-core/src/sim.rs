//! Local paper matching engine (implements `ExecutionPort`).
//!
//! Fill models:
//! - Immediate: fill on touch (marketable orders fill at once)
//! - L1: clip to top-of-book available size (partial fills)
//! - Queue: L1 with deterministic seed-based queue factor
//!
//! Orders are stored so query/cancel work. Tick/step/min-notional enforced.

use std::collections::HashMap;
use std::sync::Mutex;

use rust_decimal::Decimal;

use crate::domain::{
    ExecutionPort, Fee, FeeSide, Fill, FillModelKind, Order, OrderSide, OrderStatus, OrderType,
    TimeInForce,
};
use crate::error::{Result, TuxError};
use crate::ids::{now_ms, ClientOrderId, FillId, OrderId};
use crate::market::BookTop;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillModel {
    pub kind: FillModelKind,
    /// Aggressive-fill slippage as fraction of price (0.0001 = 1 bps).
    pub slippage: Decimal,
    pub latency_ms: u64,
    pub random_seed: u64,
    /// Maker / taker fee rates applied to fills.
    pub maker_fee: Decimal,
    pub taker_fee: Decimal,
}

impl Default for FillModel {
    fn default() -> Self {
        Self {
            kind: FillModelKind::Immediate,
            slippage: Decimal::ZERO,
            latency_ms: 0,
            random_seed: 42,
            maker_fee: Decimal::new(2, 4),
            taker_fee: Decimal::new(5, 4),
        }
    }
}

impl FillModel {
    pub fn immediate() -> Self {
        Self {
            kind: FillModelKind::Immediate,
            ..Default::default()
        }
    }

    pub fn l1() -> Self {
        Self {
            kind: FillModelKind::L1,
            ..Default::default()
        }
    }

    pub fn queue(seed: u64) -> Self {
        Self {
            kind: FillModelKind::Queue,
            random_seed: seed,
            ..Default::default()
        }
    }
}

pub struct PaperEngine {
    pub fill_model: FillModel,
    inner: Mutex<Inner>,
    pending_fills: Mutex<Vec<Fill>>,
}

#[derive(Default)]
struct Inner {
    orders: HashMap<String, Order>,
    book: HashMap<String, BookTop>,
}

impl PaperEngine {
    pub fn new(fill_model: FillModel) -> Self {
        Self {
            fill_model,
            inner: Mutex::new(Inner::default()),
            pending_fills: Mutex::new(Vec::new()),
        }
    }

    /// Drain fills produced since last call (for portfolio / event bus).
    pub fn take_fills(&self) -> Vec<Fill> {
        std::mem::take(&mut *self.pending_fills.lock().unwrap())
    }

    /// Sync lookup used by RuntimeState::apply_fill.
    pub fn query_order_blocking(&self, order_id: &OrderId) -> Result<Order> {
        self.lock_orders()
            .orders
            .get(order_id.as_str())
            .cloned()
            .ok_or_else(|| TuxError::InvalidOrder(format!("paper order {order_id} not found")))
    }

    /// Re-seat an order after restart so it can still match or cancel.
    pub fn restore_order(&self, order: Order) {
        self.lock_orders()
            .orders
            .insert(order.id.as_str().to_string(), order);
    }

    /// Feed market data so resting orders can match and market orders can price.
    pub fn on_book_top(&self, instrument: &crate::domain::InstrumentId, top: BookTop) -> Vec<Fill> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.book.insert(instrument.as_str().to_string(), top);
        }
        let fills = self.match_resting(instrument);
        self.pending_fills.lock().unwrap().extend(fills.clone());
        fills
    }

    fn match_resting(&self, instrument: &crate::domain::InstrumentId) -> Vec<Fill> {
        let mut fills = Vec::new();
        let mut inner = self.inner.lock().unwrap();
        let top = match inner.book.get(instrument.as_str()).cloned() {
            Some(t) => t,
            None => return fills,
        };
        // Resting Limit and PostOnly (PostOnly is a resting limit).
        let ids: Vec<String> = inner
            .orders
            .values()
            .filter(|o| {
                o.instrument_id == *instrument
                    && matches!(
                        o.status,
                        OrderStatus::Acknowledged | OrderStatus::PartiallyFilled
                    )
                    && matches!(o.order_type, OrderType::Limit | OrderType::PostOnly)
            })
            .map(|o| o.id.as_str().to_string())
            .collect();

        // Consume displayed L1 size across multiple orders (no double-spend).
        let mut bid_left = top.bid_qty;
        let mut ask_left = top.ask_qty;

        for id in ids {
            let mut order = match inner.orders.get(&id).cloned() {
                Some(o) => o,
                None => continue,
            };
            let px = match order.price {
                Some(p) => p,
                None => continue,
            };
            let marketable = match order.side {
                OrderSide::Buy => top.ask_price > Decimal::ZERO && px >= top.ask_price,
                OrderSide::Sell => top.bid_price > Decimal::ZERO && px <= top.bid_price,
            };
            if !marketable {
                continue;
            }
            let exec_px = match order.side {
                OrderSide::Buy => top.ask_price.min(px),
                OrderSide::Sell => top.bid_price.max(px),
            };
            let avail = match order.side {
                OrderSide::Buy => ask_left,
                OrderSide::Sell => bid_left,
            };
            let fill_qty = clip_qty(
                order.remaining(),
                avail,
                self.fill_model.kind,
                self.fill_model.random_seed,
            );
            if fill_qty <= Decimal::ZERO {
                continue;
            }
            match order.side {
                OrderSide::Buy => ask_left = (ask_left - fill_qty).max(Decimal::ZERO),
                OrderSide::Sell => bid_left = (bid_left - fill_qty).max(Decimal::ZERO),
            }
            // Resting fills are maker.
            let fill = self.apply_fill(&mut order, exec_px, fill_qty, true);
            fills.push(fill);
            inner.orders.insert(id, order);
        }
        fills
    }

    fn apply_fill(&self, order: &mut Order, price: Decimal, qty: Decimal, is_maker: bool) -> Fill {
        let notional = price * qty;
        let rate = if is_maker {
            self.fill_model.maker_fee
        } else {
            self.fill_model.taker_fee
        };
        let fee_amt = notional * rate;
        let now = now_ms();

        let prev_filled = order.filled_quantity;
        let prev_avg = order.avg_fill_price.unwrap_or(Decimal::ZERO);
        order.filled_quantity += qty;
        order.avg_fill_price = Some(match order.filled_quantity.is_zero() {
            true => price,
            false => (prev_avg * prev_filled + price * qty) / order.filled_quantity,
        });
        order.updated_at = now;
        order.venue_order_id = Some(format!("paper_{}", order.id));

        let new_status = if order.remaining() <= Decimal::ZERO {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        let _ = order.apply_status(new_status, now);

        Fill {
            id: FillId::new(),
            account_id: order.account_id.clone(),
            instrument_id: order.instrument_id.clone(),
            order_id: order.id.clone(),
            venue_order_id: order.venue_order_id.clone(),
            trade_id: {
                // unique-ish trade id
                format!("pt_{}", now)
            },
            side: order.side,
            position_side: order.position_side,
            price,
            quantity: qty,
            fee: Fee {
                asset: "USDT".into(),
                amount: fee_amt,
                side: FeeSide::Quote,
                is_maker,
                rate: Some(rate),
            },
            is_maker,
            occurred_at: now,
        }
    }

    fn try_fill_now(
        &self,
        order: &mut Order,
        top: Option<&BookTop>,
        last: Option<Decimal>,
    ) -> Option<Fill> {
        match order.order_type {
            // Stop trigger is not implemented — callers reject Stop.
            OrderType::Stop => None,
            OrderType::Market => {
                let ref_px = top
                    .and_then(|t| match order.side {
                        OrderSide::Buy if t.ask_price > Decimal::ZERO => Some(t.ask_price),
                        OrderSide::Sell if t.bid_price > Decimal::ZERO => Some(t.bid_price),
                        _ => t.mid(),
                    })
                    .or(last)?;
                let slip = ref_px * self.fill_model.slippage;
                let px = match order.side {
                    OrderSide::Buy => ref_px + slip,
                    OrderSide::Sell => ref_px - slip,
                };
                let qty = order.remaining();
                // Market against L1 only fills up to displayed size (except Immediate).
                let avail = match order.side {
                    OrderSide::Buy => top.map(|t| t.ask_qty).unwrap_or(qty),
                    OrderSide::Sell => top.map(|t| t.bid_qty).unwrap_or(qty),
                };
                let qty = clip_qty(
                    qty,
                    avail,
                    self.fill_model.kind,
                    self.fill_model.random_seed,
                );
                if qty <= Decimal::ZERO {
                    return None;
                }
                Some(self.apply_fill(order, px, qty, false))
            }
            // PostOnly never takes liquidity here (reject-if-cross is in place_order).
            OrderType::PostOnly => None,
            OrderType::Limit => {
                let top = top?;
                let px = order.price?;
                let marketable = match order.side {
                    OrderSide::Buy => top.ask_price > Decimal::ZERO && px >= top.ask_price,
                    OrderSide::Sell => top.bid_price > Decimal::ZERO && px <= top.bid_price,
                };
                if !marketable {
                    return None;
                }
                let exec_px = match order.side {
                    OrderSide::Buy => top.ask_price.min(px),
                    OrderSide::Sell => top.bid_price.max(px),
                };
                let avail = match order.side {
                    OrderSide::Buy => top.ask_qty,
                    OrderSide::Sell => top.bid_qty,
                };
                let qty = clip_qty(
                    order.remaining(),
                    avail,
                    self.fill_model.kind,
                    self.fill_model.random_seed,
                );
                if qty <= Decimal::ZERO {
                    return None;
                }
                Some(self.apply_fill(order, exec_px, qty, false))
            }
        }
    }

    /// Max quantity fillable right now against L1 (used by FOK checks).
    fn fillable_now(&self, order: &Order, top: Option<&BookTop>) -> Decimal {
        match order.order_type {
            OrderType::Market | OrderType::Limit | OrderType::PostOnly => {
                let Some(top) = top else {
                    return if order.order_type == OrderType::Market
                        && self.fill_model.kind == FillModelKind::Immediate
                    {
                        order.remaining()
                    } else {
                        Decimal::ZERO
                    };
                };
                let marketable = match order.order_type {
                    OrderType::Market => true,
                    _ => {
                        let Some(px) = order.price else {
                            return Decimal::ZERO;
                        };
                        match order.side {
                            OrderSide::Buy => top.ask_price > Decimal::ZERO && px >= top.ask_price,
                            OrderSide::Sell => top.bid_price > Decimal::ZERO && px <= top.bid_price,
                        }
                    }
                };
                if !marketable {
                    return Decimal::ZERO;
                }
                let avail = match order.side {
                    OrderSide::Buy => top.ask_qty,
                    OrderSide::Sell => top.bid_qty,
                };
                clip_qty(
                    order.remaining(),
                    avail,
                    self.fill_model.kind,
                    self.fill_model.random_seed,
                )
            }
            OrderType::Stop => Decimal::ZERO,
        }
    }
}

fn clip_qty(remaining: Decimal, available: Decimal, kind: FillModelKind, seed: u64) -> Decimal {
    match kind {
        // Immediate ignores displayed liquidity.
        FillModelKind::Immediate => remaining,
        FillModelKind::L1 => {
            if available.is_zero() {
                Decimal::ZERO
            } else {
                remaining.min(available)
            }
        }
        FillModelKind::Queue => {
            if available.is_zero() {
                return Decimal::ZERO;
            }
            // Old constant ≡ 0 (mod 3) made slot always 1; mix bits instead.
            let slot = (seed.rotate_left(17) ^ seed.wrapping_mul(0x9E37_79B9)) % 3;
            let factor = match slot {
                0 => Decimal::new(25, 2),
                1 => Decimal::new(5, 1),
                _ => Decimal::new(75, 2),
            };
            remaining.min(available * factor)
        }
    }
}

/// L1/Queue respect liquidity; Immediate for unit tests / fast backtest.
fn respects_liquidity(kind: FillModelKind) -> bool {
    matches!(kind, FillModelKind::L1 | FillModelKind::Queue)
}

impl PaperEngine {
    fn lock_orders(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl ExecutionPort for PaperEngine {
    async fn place_order(&self, order: &Order) -> Result<Order> {
        if !self.validate_client_order_id(&order.client_order_id) {
            return Err(TuxError::InvalidOrder("client_order_id rejected".into()));
        }
        // Detect duplicate client_order_id (restart safety).
        {
            let inner = self.lock_orders();
            if inner
                .orders
                .values()
                .any(|o| o.client_order_id == order.client_order_id && !o.status.is_terminal())
            {
                return Err(TuxError::InvalidOrder(format!(
                    "duplicate client_order_id {}",
                    order.client_order_id
                )));
            }
        }

        let mut o = order.clone();
        // Stop trigger is not implemented — reject rather than fake a fill.
        if o.order_type == OrderType::Stop {
            o.status = OrderStatus::Rejected;
            o.reject_reason = Some("stop orders not supported in paper MVP".into());
            self.lock_orders()
                .orders
                .insert(o.id.as_str().to_string(), o.clone());
            return Ok(o);
        }

        o.status = OrderStatus::Submitted;
        o.venue_order_id = Some(format!("paper_{}", o.id));
        o.updated_at = now_ms();

        let top_snapshot = {
            let inner = self.lock_orders();
            inner.book.get(o.instrument_id.as_str()).cloned()
        };

        // Post-only: reject if it would cross; else rest as maker.
        if o.order_type == OrderType::PostOnly {
            if let Some(top) = &top_snapshot {
                let crosses = match o.side {
                    OrderSide::Buy => {
                        top.ask_price > Decimal::ZERO
                            && o.price.map(|p| p >= top.ask_price).unwrap_or(false)
                    }
                    OrderSide::Sell => {
                        top.bid_price > Decimal::ZERO
                            && o.price.map(|p| p <= top.bid_price).unwrap_or(false)
                    }
                };
                if crosses {
                    o.status = OrderStatus::Rejected;
                    o.reject_reason = Some("post_only would cross".into());
                    self.lock_orders()
                        .orders
                        .insert(o.id.as_str().to_string(), o.clone());
                    return Ok(o);
                }
            }
            o.status = OrderStatus::Acknowledged;
            self.lock_orders()
                .orders
                .insert(o.id.as_str().to_string(), o.clone());
            return Ok(o);
        }

        // FOK: all-or-none against current liquidity.
        if o.time_in_force == TimeInForce::Fok {
            let need = o.remaining();
            let fillable = self.fillable_now(&o, top_snapshot.as_ref());
            if respects_liquidity(self.fill_model.kind) && fillable < need {
                o.status = OrderStatus::Cancelled;
                o.reject_reason = Some("fok: insufficient liquidity".into());
                self.lock_orders()
                    .orders
                    .insert(o.id.as_str().to_string(), o.clone());
                return Ok(o);
            }
        }

        let fill: Option<Fill> = {
            let inner = self.lock_orders();
            let top = inner.book.get(o.instrument_id.as_str()).cloned();
            let top_ref = top.as_ref();
            self.try_fill_now(&mut o, top_ref, top_ref.and_then(|t| t.mid()))
        };

        // IOC: fill what we can now, cancel remainder. FOK handled above.
        if o.time_in_force == TimeInForce::Ioc {
            if o.filled_quantity < o.quantity {
                let _ = o.apply_status(OrderStatus::Cancelled, now_ms());
                if o.reject_reason.is_none() {
                    o.reject_reason = Some("ioc remainder cancelled".into());
                }
            }
        } else if o.status == OrderStatus::Submitted {
            if o.filled_quantity > Decimal::ZERO {
                // apply_fill already set PartiallyFilled / Filled
            } else if o.order_type == OrderType::Market {
                o.status = OrderStatus::Rejected;
                o.reject_reason = Some("no market reference price / liquidity".into());
            } else {
                o.status = OrderStatus::Acknowledged;
            }
        }

        self.lock_orders()
            .orders
            .insert(o.id.as_str().to_string(), o.clone());
        if let Some(f) = fill {
            self.pending_fills.lock().unwrap().push(f);
        }
        Ok(o)
    }

    async fn cancel_order(&self, order_id: &OrderId) -> Result<Order> {
        let mut inner = self.lock_orders();
        let o = inner
            .orders
            .get_mut(order_id.as_str())
            .ok_or_else(|| TuxError::InvalidOrder(format!("paper order {order_id} not found")))?;
        match o.status {
            OrderStatus::Acknowledged | OrderStatus::PartiallyFilled | OrderStatus::Submitted => {
                o.apply_status(OrderStatus::Cancelled, now_ms())
                    .map_err(TuxError::other)?;
                Ok(o.clone())
            }
            s if s.is_terminal() => Ok(o.clone()),
            s => Err(TuxError::InvalidOrder(format!(
                "cannot cancel order in {s}"
            ))),
        }
    }

    async fn query_order(&self, order_id: &OrderId) -> Result<Order> {
        self.lock_orders()
            .orders
            .get(order_id.as_str())
            .cloned()
            .ok_or_else(|| TuxError::InvalidOrder(format!("paper order {order_id} not found")))
    }

    fn validate_client_order_id(&self, id: &ClientOrderId) -> bool {
        let s = id.as_str();
        !s.is_empty() && s.len() <= 36
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;
    use crate::ids::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn limit_buy(id: &str, px: &str, qty: &str) -> Order {
        let now = now_ms();
        Order {
            id: OrderId::from_raw(id),
            intent_id: None,
            client_order_id: ClientOrderId::from_raw(id),
            venue_order_id: None,
            account_id: AccountId::from("a"),
            strategy_instance_id: Some(StrategyInstanceId::from("s")),
            instrument_id: InstrumentId::new("SOL-USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            status: OrderStatus::RiskApproved,
            price: Some(d(px)),
            quantity: d(qty),
            filled_quantity: Decimal::ZERO,
            avg_fill_price: None,
            reduce_only: ReduceOnly::No,
            position_side: PositionSide::Both,
            reject_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn place_query_cancel_roundtrip() {
        let eng = PaperEngine::new(FillModel::immediate());
        let o = limit_buy("o1", "100", "1");
        let placed = eng.place_order(&o).await.unwrap();
        assert!(matches!(
            placed.status,
            OrderStatus::Acknowledged | OrderStatus::Filled | OrderStatus::PartiallyFilled
        ));
        let got = eng.query_order(&placed.id).await.unwrap();
        assert_eq!(got.id, placed.id);
        let c = eng.cancel_order(&placed.id).await.unwrap();
        assert_eq!(c.status, OrderStatus::Cancelled);
    }

    #[tokio::test]
    async fn marketable_limit_fills() {
        let eng = PaperEngine::new(FillModel::immediate());
        eng.on_book_top(
            &InstrumentId::new("SOL-USDT"),
            BookTop {
                bid_price: d("99"),
                bid_qty: d("10"),
                ask_price: d("101"),
                ask_qty: d("10"),
                at: now_ms(),
            },
        );
        let o = limit_buy("o2", "102", "1");
        let placed = eng.place_order(&o).await.unwrap();
        assert_eq!(placed.status, OrderStatus::Filled);
        assert_eq!(placed.filled_quantity, d("1"));
        assert_eq!(placed.avg_fill_price, Some(d("101")));
    }

    #[tokio::test]
    async fn l1_partial_fill() {
        let eng = PaperEngine::new(FillModel::l1());
        eng.on_book_top(
            &InstrumentId::new("SOL-USDT"),
            BookTop {
                bid_price: d("99"),
                bid_qty: d("10"),
                ask_price: d("100"),
                ask_qty: d("0.4"),
                at: now_ms(),
            },
        );
        let o = limit_buy("o3", "100", "1");
        let placed = eng.place_order(&o).await.unwrap();
        assert_eq!(placed.status, OrderStatus::PartiallyFilled);
        assert_eq!(placed.filled_quantity, d("0.4"));
    }

    #[tokio::test]
    async fn fok_all_or_none() {
        let eng = PaperEngine::new(FillModel::l1());
        eng.on_book_top(
            &InstrumentId::new("SOL-USDT"),
            BookTop {
                bid_price: d("99"),
                bid_qty: d("10"),
                ask_price: d("100"),
                ask_qty: d("0.3"),
                at: now_ms(),
            },
        );
        let mut o = limit_buy("fok1", "100", "1");
        o.time_in_force = TimeInForce::Fok;
        let placed = eng.place_order(&o).await.unwrap();
        assert_eq!(placed.status, OrderStatus::Cancelled);
        assert_eq!(placed.filled_quantity, Decimal::ZERO);
    }

    #[tokio::test]
    async fn ioc_cancels_remainder() {
        let eng = PaperEngine::new(FillModel::l1());
        eng.on_book_top(
            &InstrumentId::new("SOL-USDT"),
            BookTop {
                bid_price: d("99"),
                bid_qty: d("10"),
                ask_price: d("100"),
                ask_qty: d("0.4"),
                at: now_ms(),
            },
        );
        let mut o = limit_buy("ioc1", "100", "1");
        o.time_in_force = TimeInForce::Ioc;
        let placed = eng.place_order(&o).await.unwrap();
        assert_eq!(placed.status, OrderStatus::Cancelled);
        assert_eq!(placed.filled_quantity, d("0.4"));
    }

    #[tokio::test]
    async fn post_only_rests_then_maker_fill() {
        let eng = PaperEngine::new(FillModel::l1());
        let t0 = now_ms();
        eng.on_book_top(
            &InstrumentId::new("SOL-USDT"),
            BookTop {
                bid_price: d("99"),
                bid_qty: d("10"),
                ask_price: d("100"),
                ask_qty: d("10"),
                at: t0,
            },
        );
        let mut o = limit_buy("po1", "90", "1");
        o.order_type = OrderType::PostOnly;
        let placed = eng.place_order(&o).await.unwrap();
        assert_eq!(placed.status, OrderStatus::Acknowledged);
        eng.on_book_top(
            &InstrumentId::new("SOL-USDT"),
            BookTop {
                bid_price: d("89"),
                bid_qty: d("10"),
                ask_price: d("90"),
                ask_qty: d("1"),
                at: t0 + 1,
            },
        );
        let fills = eng.take_fills();
        assert_eq!(fills.len(), 1);
        assert!(fills[0].is_maker);
    }

    #[tokio::test]
    async fn post_only_rejects_cross() {
        let eng = PaperEngine::new(FillModel::immediate());
        eng.on_book_top(
            &InstrumentId::new("SOL-USDT"),
            BookTop {
                bid_price: d("99"),
                bid_qty: d("10"),
                ask_price: d("100"),
                ask_qty: d("10"),
                at: now_ms(),
            },
        );
        let mut o = limit_buy("po2", "100", "1");
        o.order_type = OrderType::PostOnly;
        let placed = eng.place_order(&o).await.unwrap();
        assert_eq!(placed.status, OrderStatus::Rejected);
    }

    #[tokio::test]
    async fn stop_rejected_unsupported() {
        let eng = PaperEngine::new(FillModel::immediate());
        let mut o = limit_buy("st1", "100", "1");
        o.order_type = OrderType::Stop;
        let placed = eng.place_order(&o).await.unwrap();
        assert_eq!(placed.status, OrderStatus::Rejected);
    }
}
