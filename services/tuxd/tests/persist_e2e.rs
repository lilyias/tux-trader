//! Persistence e2e: closed loop writes SQLite; queries return the facts.

use std::str::FromStr;
use rust_decimal::Decimal;
use tux_core::domain::{ExecutionPort, Instrument};
use tux_core::ids::{AccountId, Venue};
use tux_core::portfolio::Portfolio;
use tux_core::sim::FillModel;
use tux_core::store::Store;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

#[tokio::test]
async fn paper_demo_persists_orders_fills_equity() {
    let store = Store::open_in_memory().unwrap();
    // Reuse the library-level store unit coverage plus a lightweight in-process loop.
    let instrument = Instrument::spot("SOL-USDT", Venue::LocalPaper, "SOL", "USDT", d("0.01"), d("0.001"));
    let account_id = AccountId::from("paper_acc");
    let mut portfolio = Portfolio::new();
    portfolio.seed_balance(tux_core::domain::Balance {
        account_id: account_id.clone(),
        asset: "USDT".into(),
        free: d("10000"),
        locked: Decimal::ZERO,
        updated_at: tux_core::ids::now_ms(),
    });

    let paper = tux_core::sim::PaperEngine::new(FillModel::immediate());
    paper.on_book_top(
        &instrument.id,
        tux_core::market::BookTop {
            bid_price: d("99.5"),
            bid_qty: d("2"),
            ask_price: d("100"),
            ask_qty: d("2"),
            at: tux_core::ids::now_ms(),
        },
    );

    let now = tux_core::ids::now_ms();
    let order = tux_core::domain::Order {
        id: tux_core::ids::OrderId::from_raw("ord_persist"),
        intent_id: None,
        client_order_id: tux_core::ids::ClientOrderId::from_raw("c_persist"),
        venue_order_id: None,
        account_id: account_id.clone(),
        strategy_instance_id: Some(tux_core::ids::StrategyInstanceId::from("s")),
        instrument_id: instrument.id.clone(),
        side: tux_core::domain::OrderSide::Buy,
        order_type: tux_core::domain::OrderType::Limit,
        time_in_force: tux_core::domain::TimeInForce::Gtc,
        status: tux_core::domain::OrderStatus::RiskApproved,
        price: Some(d("100")),
        quantity: d("0.5"),
        filled_quantity: Decimal::ZERO,
        avg_fill_price: None,
        reduce_only: tux_core::domain::ReduceOnly::No,
        position_side: tux_core::domain::PositionSide::Both,
        reject_reason: None,
        created_at: now,
        updated_at: now,
    };
    let placed = paper.place_order(&order).await.unwrap();
    store.save_order(&placed).unwrap();
    for fill in paper.take_fills() {
        let _ = portfolio.apply_spot_fill(
            &account_id,
            &instrument,
            fill.side,
            fill.price,
            fill.quantity,
            fill.fee.amount,
            "USDT",
        );
        store.save_fill(&fill).unwrap();
        if let Some(ev) = portfolio.ledger().last() {
            store.save_ledger(ev).unwrap();
        }
    }
    let equity = portfolio.equity(&account_id);
    let book = portfolio.pnl_book(&account_id).cloned().unwrap_or_default();
    store
        .save_equity(&tux_core::store::EquityPoint {
            ts: now,
            account_id: account_id.to_string(),
            equity,
            net_pnl: book.net_pnl(),
            fees: book.components.trading_fees,
        })
        .unwrap();

    let orders = store.list_orders(10).unwrap();
    assert_eq!(orders.len(), 1);
    let fills = store.list_fills(10).unwrap();
    assert_eq!(fills.len(), 1);
    assert_eq!(d(&fills[0].quantity), d("0.5"));
    let eq = store.equity_series(account_id.as_str(), 10).unwrap();
    assert_eq!(eq.len(), 1);
    let ov = store
        .overview(account_id.as_str(), Some(&book), false)
        .unwrap();
    assert_eq!(ov.fills, 1);
    assert!(ov.equity > d("0"));
}
