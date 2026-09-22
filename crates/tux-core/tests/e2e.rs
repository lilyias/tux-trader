//! End-to-end: market → strategy intent → risk → OMS → paper → portfolio.

use std::str::FromStr;
use rust_decimal::Decimal;
use tux_core::domain::*;
use tux_core::events::{BookTicker, InMemoryEventBus, MarketEvent};
use tux_core::ids::*;
use tux_core::market::{BookTop, MarketState};
use tux_core::oms::Oms;
use tux_core::portfolio::Portfolio;
use tux_core::risk::RiskEngine;
use tux_core::sim::{FillModel, PaperEngine};

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn sol() -> Instrument {
    Instrument::spot("SOL-USDT", Venue::LocalPaper, "SOL", "USDT", d("0.01"), d("0.001"))
}

#[tokio::test]
async fn e2e_market_to_portfolio_fill() {
    let instrument = sol();
    let inst_id = instrument.id.clone();
    let account_id = AccountId::from("paper_acc");

    let bus = InMemoryEventBus::new();
    let mut sub = bus.subscribe();

    let mut market = MarketState::new();
    let mut portfolio = Portfolio::new();
    portfolio.seed_balance(Balance {
        account_id: account_id.clone(),
        asset: "USDT".into(),
        free: d("10000"),
        locked: Decimal::ZERO,
        updated_at: now_ms(),
    });

    let limits = RiskLimitConfig {
        instrument_whitelist: vec![inst_id.clone()],
        max_order_notional: Some(d("5000")),
        max_market_age_ms: Some(60_000),
        consecutive_reject_limit: Some(3),
        ..Default::default()
    };
    let mut risk = RiskEngine::new(limits);
    risk.set_feeds_connected(true);

    let paper = PaperEngine::new(FillModel::immediate());
    let mut oms = Oms::new();

    // Market
    let top = BookTop {
        bid_price: d("99.50"),
        bid_qty: d("5"),
        ask_price: d("100.00"),
        ask_qty: d("5"),
        at: now_ms(),
    };
    market.on_book_ticker(&BookTicker {
        instrument_id: inst_id.clone(),
        bid_price: top.bid_price,
        bid_qty: top.bid_qty,
        ask_price: top.ask_price,
        ask_qty: top.ask_qty,
        occurred_at: top.at,
    });
    paper.on_book_top(&inst_id, top.clone());
    bus.publish(tux_core::events::BusEvent::Market(MarketEvent::BookTicker(
        BookTicker {
            instrument_id: inst_id.clone(),
            bid_price: top.bid_price,
            bid_qty: top.bid_qty,
            ask_price: top.ask_price,
            ask_qty: top.ask_qty,
            occurred_at: top.at,
        },
    )))
    .unwrap();
    assert!(sub.try_recv().is_ok(), "event bus must deliver, not drop");

    // Strategy intent (limit buy crossing the spread)
    let intent = OrderIntent {
        id: IntentId::new(),
        strategy_instance_id: StrategyInstanceId::from("s1"),
        account_id: account_id.clone(),
        instrument_id: inst_id.clone(),
        side: OrderSide::Buy,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::Gtc,
        price: Some(d("100.00")),
        quantity: d("0.5"),
        reduce_only: ReduceOnly::No,
        position_side: PositionSide::Both,
        leverage: None,
        client_order_id: ClientOrderId::from("e2e-1"),
        reason: Some("e2e".into()),
        created_at: now_ms(),
    };

    // Risk
    let market_ref = market.reference_price(&inst_id);
    let age = market.age_ms(&inst_id);
    let decision = risk.check_intent(&intent, &instrument, market_ref, age, &portfolio);
    assert!(decision.is_approved(), "{decision:?}");

    // OMS → Paper
    let order = oms.create_from_intent(&intent).unwrap();
    let placed = oms.submit(&order.id, &paper).await.unwrap();
    assert_eq!(placed.status, OrderStatus::Filled, "{placed:?}");
    assert_eq!(placed.filled_quantity, d("0.5"));

    // Paper query works
    let q = paper.query_order(&placed.id).await.unwrap();
    assert_eq!(q.id, placed.id);

    // Fills → Portfolio
    let fills = paper.take_fills();
    assert_eq!(fills.len(), 1);
    for fill in &fills {
        portfolio
            .apply_spot_fill(
                &account_id,
                &instrument,
                fill.side,
                fill.price,
                fill.quantity,
                fill.fee.amount,
                "USDT",
            )
            .unwrap();
    }
    let base = portfolio.balance(&account_id, "SOL").unwrap();
    assert_eq!(base.free, d("0.5"));
    let quote = portfolio.balance(&account_id, "USDT").unwrap();
    // 10000 - 100*0.5 - fee
    assert!(quote.free < d("9950") && quote.free > d("9949"), "quote={}", quote.free);
}

#[tokio::test]
async fn e2e_market_order_notional_enforced() {
    let instrument = sol();
    let limits = RiskLimitConfig {
        instrument_whitelist: vec![instrument.id.clone()],
        max_order_notional: Some(d("50")),
        max_market_age_ms: Some(60_000),
        consecutive_reject_limit: None,
        ..Default::default()
    };
    let mut risk = RiskEngine::new(limits);
    let portfolio = Portfolio::new();

    // Market buy 1 @ ref 100 → notional 100 > 50
    let intent = OrderIntent {
        id: IntentId::new(),
        strategy_instance_id: StrategyInstanceId::from("s1"),
        account_id: AccountId::from("a"),
        instrument_id: instrument.id.clone(),
        side: OrderSide::Buy,
        order_type: OrderType::Market,
        time_in_force: TimeInForce::Ioc,
        price: None,
        quantity: d("1"),
        reduce_only: ReduceOnly::No,
        position_side: PositionSide::Both,
        leverage: None,
        client_order_id: ClientOrderId::from("m1"),
        reason: None,
        created_at: now_ms(),
    };
    let dec = risk.check_intent(&intent, &instrument, Some(d("100")), Some(1), &portfolio);
    match dec {
        RiskDecision::Rejected { rule, .. } => assert_eq!(rule, "max_order_notional"),
        other => panic!("expected reject, got {other:?}"),
    }

    // Without market ref → cannot compute notional → reject (no bypass)
    let dec2 = risk.check_intent(&intent, &instrument, None, Some(1), &portfolio);
    match dec2 {
        RiskDecision::Rejected { rule, .. } => assert_eq!(rule, "missing_reference_price"),
        other => panic!("expected reject, got {other:?}"),
    }
}

#[tokio::test]
async fn e2e_cancel_resting_limit_order() {
    let paper = PaperEngine::new(FillModel::immediate());
    let now = now_ms();
    let order = Order {
        id: OrderId::from_raw("ord_cancel"),
        intent_id: None,
        client_order_id: ClientOrderId::from_raw("cxl-1"),
        venue_order_id: None,
        account_id: AccountId::from("a"),
        strategy_instance_id: None,
        instrument_id: InstrumentId::new("SOL-USDT"),
        side: OrderSide::Buy,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::Gtc,
        status: OrderStatus::RiskApproved,
        price: Some(d("10")), // far below market — rests
        quantity: d("1"),
        filled_quantity: Decimal::ZERO,
        avg_fill_price: None,
        reduce_only: ReduceOnly::No,
        position_side: PositionSide::Both,
        reject_reason: None,
        created_at: now,
        updated_at: now,
    };
    paper
        .on_book_top(
            &InstrumentId::new("SOL-USDT"),
            BookTop {
                bid_price: d("99"),
                bid_qty: d("1"),
                ask_price: d("100"),
                ask_qty: d("1"),
                at: now,
            },
        );
    let placed = paper.place_order(&order).await.unwrap();
    assert_eq!(placed.status, OrderStatus::Acknowledged);
    let cancelled = paper.cancel_order(&placed.id).await.unwrap();
    assert_eq!(cancelled.status, OrderStatus::Cancelled);
}

#[test]
fn live_mode_is_refused() {
    assert!(tux_core::ensure_not_live(Environment::Live).is_err());
    assert!(tux_core::ensure_not_live(Environment::LocalPaper).is_ok());
}

#[test]
fn router_split_preserves_order() {
    let now = now_ms();
    let o = Order {
        id: OrderId::from_raw("x"),
        intent_id: None,
        client_order_id: ClientOrderId::from_raw("c"),
        venue_order_id: None,
        account_id: AccountId::from("a"),
        strategy_instance_id: None,
        instrument_id: InstrumentId::new("SOL-USDT"),
        side: OrderSide::Buy,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::Gtc,
        status: OrderStatus::RiskApproved,
        price: Some(d("1")),
        quantity: d("1"),
        filled_quantity: Decimal::ZERO,
        avg_fill_price: None,
        reduce_only: ReduceOnly::No,
        position_side: PositionSide::Both,
        reject_reason: None,
        created_at: now,
        updated_at: now,
    };
    assert_eq!(tux_core::oms::SmartOrderRouter::split(&o).len(), 1);
}
