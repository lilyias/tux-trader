//! Local SQLite persistence (lightweight default).
//!
//! PostgreSQL / MinIO stay optional deploy targets — the Paper MVP must not
//! require external infrastructure. Schema is intentionally simple and
//! append-oriented so it can later be mirrored into PostgreSQL.

use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

use rust_decimal::Decimal;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::domain::{Fill, Order, OrderStatus};
use crate::error::{Result, TuxError};
use crate::ids::TimestampMs;
use crate::portfolio::{AccountingEvent, PnLBook};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts: TimestampMs,
    pub account_id: String,
    pub equity: Decimal,
    pub net_pnl: Decimal,
    pub fees: Decimal,
}

/// One market mid-price sample (for the price trend chart).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricePoint {
    pub ts: TimestampMs,
    pub bid: Decimal,
    pub ask: Decimal,
    pub mid: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overview {
    pub account_id: String,
    pub equity: Decimal,
    pub net_pnl: Decimal,
    pub gross_realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub trading_fees: Decimal,
    pub funding_fees: Decimal,
    pub borrow_interest: Decimal,
    pub open_orders: u64,
    pub fills: u64,
    pub kill_switch: bool,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let conn = Connection::open(path).map_err(db_err)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS orders (
                id              TEXT PRIMARY KEY,
                account_id      TEXT NOT NULL,
                instrument_id   TEXT NOT NULL,
                strategy_id     TEXT,
                client_order_id TEXT NOT NULL,
                side            TEXT NOT NULL,
                order_type      TEXT NOT NULL,
                status          TEXT NOT NULL,
                price           TEXT,
                quantity        TEXT NOT NULL,
                filled_quantity TEXT NOT NULL,
                avg_fill_price  TEXT,
                reject_reason   TEXT,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                full_json       TEXT NOT NULL DEFAULT '{}'
            );

            CREATE TABLE IF NOT EXISTS fills (
                id              TEXT PRIMARY KEY,
                order_id        TEXT NOT NULL,
                account_id      TEXT NOT NULL,
                instrument_id   TEXT NOT NULL,
                side            TEXT NOT NULL,
                price           TEXT NOT NULL,
                quantity        TEXT NOT NULL,
                fee_asset       TEXT NOT NULL,
                fee_amount      TEXT NOT NULL,
                is_maker        INTEGER NOT NULL,
                occurred_at     INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_fills_time ON fills(occurred_at);

            CREATE TABLE IF NOT EXISTS ledger (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                at              INTEGER NOT NULL,
                kind            TEXT NOT NULL,
                payload_json    TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_ledger_time ON ledger(at);

            CREATE TABLE IF NOT EXISTS equity (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                ts              INTEGER NOT NULL,
                account_id      TEXT NOT NULL,
                equity          TEXT NOT NULL,
                net_pnl         TEXT NOT NULL,
                fees            TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_equity_time ON equity(ts);

            CREATE TABLE IF NOT EXISTS market_ticks (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                instrument_id   TEXT NOT NULL,
                bid_price       TEXT NOT NULL,
                ask_price       TEXT NOT NULL,
                mid_price       TEXT NOT NULL,
                occurred_at     INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_ticks_time ON market_ticks(occurred_at);

            CREATE TABLE IF NOT EXISTS balances (
                account_id      TEXT NOT NULL,
                asset           TEXT NOT NULL,
                free            TEXT NOT NULL,
                locked          TEXT NOT NULL,
                updated_at      INTEGER NOT NULL,
                PRIMARY KEY (account_id, asset)
            );
            "#,
        )
        .map_err(db_err)?;
        // Older DBs created before full_json: ALTER is a no-op when the column exists.
        let _ = conn.execute(
            "ALTER TABLE orders ADD COLUMN full_json TEXT NOT NULL DEFAULT '{}'",
            [],
        );
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| TuxError::other("store mutex poisoned"))
    }

    pub fn save_order(&self, o: &Order) -> Result<()> {
        let full = serde_json::to_string(o)
            .map_err(|e| TuxError::Storage(format!("order serialize: {e}")))?;
        self.lock()?
            .execute(
                r#"INSERT OR REPLACE INTO orders
                   (id, account_id, instrument_id, strategy_id, client_order_id,
                    side, order_type, status, price, quantity, filled_quantity,
                    avg_fill_price, reject_reason, created_at, updated_at, full_json)
                   VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)"#,
                params![
                    o.id.as_str(),
                    o.account_id.as_str(),
                    o.instrument_id.as_str(),
                    o.strategy_instance_id.as_ref().map(|s| s.as_str().to_string()),
                    o.client_order_id.as_str(),
                    side_str(&o.side),
                    type_str(&o.order_type),
                    o.status.to_string(),
                    o.price.map(|p| p.to_string()),
                    o.quantity.to_string(),
                    o.filled_quantity.to_string(),
                    o.avg_fill_price.map(|p| p.to_string()),
                    o.reject_reason,
                    o.created_at,
                    o.updated_at,
                    full,
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn save_fill(&self, f: &Fill) -> Result<()> {
        self.lock()?
            .execute(
                r#"INSERT OR REPLACE INTO fills
                   (id, order_id, account_id, instrument_id, side, price, quantity,
                    fee_asset, fee_amount, is_maker, occurred_at)
                   VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"#,
                params![
                    f.id.as_str(),
                    f.order_id.as_str(),
                    f.account_id.as_str(),
                    f.instrument_id.as_str(),
                    side_str(&f.side),
                    f.price.to_string(),
                    f.quantity.to_string(),
                    f.fee.asset,
                    f.fee.amount.to_string(),
                    f.is_maker as i64,
                    f.occurred_at,
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn save_ledger(&self, event: &AccountingEvent) -> Result<()> {
        let (at, kind) = match event {
            AccountingEvent::Fee { at, .. } => (*at, "fee"),
            AccountingEvent::FillApplied { at, .. } => (*at, "fill_applied"),
            AccountingEvent::ManualAdjustment { at, .. } => (*at, "manual_adjustment"),
        };
        let payload = serde_json::to_string(event)
            .map_err(|e| TuxError::other(format!("ledger serialize: {e}")))?;
        self.lock()?
            .execute(
                "INSERT INTO ledger (at, kind, payload_json) VALUES (?1, ?2, ?3)",
                params![at, kind, payload],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn save_equity(&self, p: &EquityPoint) -> Result<()> {
        self.lock()?
            .execute(
                r#"INSERT INTO equity (ts, account_id, equity, net_pnl, fees)
                   VALUES (?1,?2,?3,?4,?5)"#,
                params![
                    p.ts,
                    p.account_id,
                    p.equity.to_string(),
                    p.net_pnl.to_string(),
                    p.fees.to_string(),
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn save_market_tick(
        &self,
        instrument_id: &str,
        bid: Decimal,
        ask: Decimal,
        mid: Decimal,
        at: TimestampMs,
    ) -> Result<()> {
        self.lock()?
            .execute(
                r#"INSERT INTO market_ticks (instrument_id, bid_price, ask_price, mid_price, occurred_at)
                   VALUES (?1,?2,?3,?4,?5)"#,
                params![
                    instrument_id,
                    bid.to_string(),
                    ask.to_string(),
                    mid.to_string(),
                    at
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Drop stored ticks for one instrument (start a clean series for a run).
    pub fn clear_market_ticks(&self, instrument_id: &str) -> Result<()> {
        self.lock()?
            .execute(
                "DELETE FROM market_ticks WHERE instrument_id = ?1",
                params![instrument_id],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Recent mid-price points for the dashboard trend chart (oldest → newest).
    pub fn price_series(
        &self,
        instrument_id: &str,
        limit: u32,
    ) -> Result<Vec<PricePoint>> {
        let conn = self.lock()?;
        // ASC window: take latest N by subquery then reverse to chronological.
        let mut stmt = conn
            .prepare(
                r#"SELECT occurred_at, bid_price, ask_price, mid_price
                   FROM (
                     SELECT occurred_at, bid_price, ask_price, mid_price
                     FROM market_ticks
                     WHERE instrument_id = ?1
                     ORDER BY occurred_at DESC
                     LIMIT ?2
                   )
                   ORDER BY occurred_at ASC"#,
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![instrument_id, limit as i64], |r| {
                let bid: String = r.get(1)?;
                let ask: String = r.get(2)?;
                let mid: String = r.get(3)?;
                Ok(PricePoint {
                    ts: r.get(0)?,
                    bid: Decimal::from_str(&bid).unwrap_or_default(),
                    ask: Decimal::from_str(&ask).unwrap_or_default(),
                    mid: Decimal::from_str(&mid).unwrap_or_default(),
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_err)
    }

    pub fn save_balances(&self, account_id: &str, balances: &[crate::domain::Balance]) -> Result<()> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                r#"INSERT INTO balances (account_id, asset, free, locked, updated_at)
                   VALUES (?1,?2,?3,?4,?5)
                   ON CONFLICT(account_id, asset) DO UPDATE SET
                     free=excluded.free, locked=excluded.locked, updated_at=excluded.updated_at"#,
            )
            .map_err(db_err)?;
        for b in balances {
            stmt.execute(params![
                account_id,
                b.asset,
                b.free.to_string(),
                b.locked.to_string(),
                b.updated_at
            ])
            .map_err(db_err)?;
        }
        Ok(())
    }

    pub fn load_balances(&self, account_id: &str) -> Result<Vec<crate::domain::Balance>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT account_id, asset, free, locked, updated_at FROM balances WHERE account_id = ?1",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![account_id], |r| {
                let free: String = r.get(2)?;
                let locked: String = r.get(3)?;
                Ok(crate::domain::Balance {
                    account_id: crate::ids::AccountId::from(r.get::<_, String>(0)?),
                    asset: r.get(1)?,
                    free: Decimal::from_str(&free).unwrap_or_default(),
                    locked: Decimal::from_str(&locked).unwrap_or_default(),
                    updated_at: r.get(4)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_err)
    }

    /// Active orders for restart recovery (scoped to one account).
    pub fn load_open_orders_for(&self, account_id: &str) -> Result<Vec<crate::domain::Order>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                r#"SELECT full_json FROM orders
                   WHERE account_id = ?1
                     AND status IN ('submitted','acknowledged','partially_filled','cancel_pending','unknown','risk_approved')
                   ORDER BY created_at ASC"#,
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![account_id], |r| r.get::<_, String>(0))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let full = row.map_err(db_err)?;
            match serde_json::from_str::<crate::domain::Order>(&full) {
                Ok(o) => out.push(o),
                Err(e) => {
                    return Err(TuxError::Storage(format!(
                        "corrupt order json, refusing silent skip: {e}"
                    )))
                }
            }
        }
        Ok(out)
    }


    /// Active orders for restart recovery into OMS (full semantics from JSON).
    pub fn load_open_orders(&self) -> Result<Vec<crate::domain::Order>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                r#"SELECT full_json, status FROM orders
                   WHERE status IN ('submitted','acknowledged','partially_filled','cancel_pending','unknown','risk_approved')
                   ORDER BY created_at ASC"#,
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                let full: String = r.get(0)?;
                Ok(full)
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let full = row.map_err(db_err)?;
            if let Ok(o) = serde_json::from_str::<crate::domain::Order>(&full) {
                out.push(o);
            }
        }
        Ok(out)
    }

    pub fn list_orders(&self, limit: u32) -> Result<Vec<OrderRow>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                r#"SELECT id, account_id, instrument_id, side, order_type, status,
                          price, quantity, filled_quantity, avg_fill_price, created_at
                   FROM orders ORDER BY created_at DESC LIMIT ?1"#,
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(OrderRow {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    instrument_id: r.get(2)?,
                    side: r.get(3)?,
                    order_type: r.get(4)?,
                    status: r.get(5)?,
                    price: r.get(6)?,
                    quantity: r.get(7)?,
                    filled_quantity: r.get(8)?,
                    avg_fill_price: r.get(9)?,
                    created_at: r.get(10)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_err)
    }

    pub fn list_fills(&self, limit: u32) -> Result<Vec<FillRow>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                r#"SELECT id, order_id, instrument_id, side, price, quantity,
                          fee_asset, fee_amount, is_maker, occurred_at
                   FROM fills ORDER BY occurred_at DESC LIMIT ?1"#,
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(FillRow {
                    id: r.get(0)?,
                    order_id: r.get(1)?,
                    instrument_id: r.get(2)?,
                    side: r.get(3)?,
                    price: r.get(4)?,
                    quantity: r.get(5)?,
                    fee_asset: r.get(6)?,
                    fee_amount: r.get(7)?,
                    is_maker: r.get::<_, i64>(8)? != 0,
                    occurred_at: r.get(9)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_err)
    }

    pub fn equity_series(&self, account_id: &str, limit: u32) -> Result<Vec<EquityPoint>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                r#"SELECT ts, account_id, equity, net_pnl, fees
                   FROM (
                     SELECT ts, account_id, equity, net_pnl, fees
                     FROM equity WHERE account_id = ?1
                     ORDER BY ts DESC LIMIT ?2
                   ) ORDER BY ts ASC"#,
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![account_id, limit as i64], |r| {
                let equity: String = r.get(2)?;
                let net_pnl: String = r.get(3)?;
                let fees: String = r.get(4)?;
                Ok(EquityPoint {
                    ts: r.get(0)?,
                    account_id: r.get(1)?,
                    equity: Decimal::from_str(&equity).unwrap_or_default(),
                    net_pnl: Decimal::from_str(&net_pnl).unwrap_or_default(),
                    fees: Decimal::from_str(&fees).unwrap_or_default(),
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_err)
    }

    pub fn overview(
        &self,
        account_id: &str,
        book: Option<&PnLBook>,
        kill_switch: bool,
    ) -> Result<Overview> {
        let conn = self.lock()?;
        let open_orders: u64 = conn
            .query_row(
                r#"SELECT COUNT(*) FROM orders
                   WHERE account_id = ?1 AND status IN
                     ('submitted','acknowledged','partially_filled','cancel_pending')"#,
                params![account_id],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        let fills: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fills WHERE account_id = ?1",
                params![account_id],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        let last_equity: Option<String> = conn
            .query_row(
                r#"SELECT equity FROM equity WHERE account_id = ?1
                   ORDER BY ts DESC LIMIT 1"#,
                params![account_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;

        let default_book = PnLBook::default();
        let book = book.unwrap_or(&default_book);
        let c = &book.components;
        // Prefer last persisted equity snapshot (authoritative after demo runs).
        let eq_series = {
            drop(conn);
            self.equity_series(account_id, 1).unwrap_or_default()
        };
        let last = eq_series.last();
        Ok(Overview {
            account_id: account_id.to_string(),
            equity: last.map(|p| p.equity).unwrap_or_else(|| {
                last_equity
                    .as_deref()
                    .and_then(|s| Decimal::from_str(s).ok())
                    .unwrap_or_default()
            }),
            net_pnl: last.map(|p| p.net_pnl).unwrap_or_else(|| book.net_pnl()),
            gross_realized_pnl: c.gross_realized_pnl,
            unrealized_pnl: c.unrealized_pnl,
            trading_fees: last.map(|p| p.fees).unwrap_or(c.trading_fees),
            funding_fees: c.funding_fees,
            borrow_interest: c.borrow_interest,
            open_orders,
            fills,
            kill_switch,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRow {
    pub id: String,
    pub account_id: String,
    pub instrument_id: String,
    pub side: String,
    pub order_type: String,
    pub status: String,
    pub price: Option<String>,
    pub quantity: String,
    pub filled_quantity: String,
    pub avg_fill_price: Option<String>,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillRow {
    pub id: String,
    pub order_id: String,
    pub instrument_id: String,
    pub side: String,
    pub price: String,
    pub quantity: String,
    pub fee_asset: String,
    pub fee_amount: String,
    pub is_maker: bool,
    pub occurred_at: TimestampMs,
}

fn side_str(s: &crate::domain::OrderSide) -> &'static str {
    match s {
        crate::domain::OrderSide::Buy => "buy",
        crate::domain::OrderSide::Sell => "sell",
    }
}

fn type_str(t: &crate::domain::OrderType) -> &'static str {
    match t {
        crate::domain::OrderType::Limit => "limit",
        crate::domain::OrderType::Market => "market",
        crate::domain::OrderType::PostOnly => "post_only",
        crate::domain::OrderType::Stop => "stop",
    }
}

pub fn order_side_from_str(s: &str) -> crate::domain::OrderSide {
    match s {
        "sell" | "\"sell\"" => crate::domain::OrderSide::Sell,
        _ => crate::domain::OrderSide::Buy,
    }
}

fn db_err(e: rusqlite::Error) -> TuxError {
    TuxError::Storage(e.to_string())
}


/// Convenience used by status rendering.
pub fn status_is_active(status: &OrderStatus) -> bool {
    status.is_active()
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

    #[test]
    fn save_and_query_order_fill_equity() {
        let store = Store::open_in_memory().unwrap();
        let now = now_ms();
        let order = Order {
            id: OrderId::from_raw("o1"),
            intent_id: None,
            client_order_id: ClientOrderId::from_raw("c1"),
            venue_order_id: Some("paper_o1".into()),
            account_id: AccountId::from("paper_acc"),
            strategy_instance_id: Some(StrategyInstanceId::from("s1")),
            instrument_id: InstrumentId::new("SOL-USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            status: OrderStatus::Filled,
            price: Some(d("100")),
            quantity: d("0.5"),
            filled_quantity: d("0.5"),
            avg_fill_price: Some(d("100")),
            reduce_only: ReduceOnly::No,
            position_side: PositionSide::Both,
            reject_reason: None,
            created_at: now,
            updated_at: now,
        };
        store.save_order(&order).unwrap();

        let fill = Fill {
            id: FillId::from_raw("f1"),
            account_id: AccountId::from("paper_acc"),
            instrument_id: InstrumentId::new("SOL-USDT"),
            order_id: OrderId::from_raw("o1"),
            venue_order_id: Some("paper_o1".into()),
            trade_id: "t1".into(),
            side: OrderSide::Buy,
            position_side: PositionSide::Both,
            price: d("100"),
            quantity: d("0.5"),
            fee: Fee {
                asset: "USDT".into(),
                amount: d("0.025"),
                side: FeeSide::Quote,
                is_maker: false,
                rate: Some(d("0.0005")),
            },
            is_maker: false,
            occurred_at: now,
        };
        store.save_fill(&fill).unwrap();
        store
            .save_ledger(&AccountingEvent::Fee {
                asset: "USDT".into(),
                amount: d("0.025"),
                at: now,
            })
            .unwrap();
        store
            .save_equity(&EquityPoint {
                ts: now,
                account_id: "paper_acc".into(),
                equity: d("9950"),
                net_pnl: d("-0.025"),
                fees: d("0.025"),
            })
            .unwrap();

        let orders = store.list_orders(10).unwrap();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].status, "filled");
        let fills = store.list_fills(10).unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].fee_amount, "0.025");
        let eq = store.equity_series("paper_acc", 10).unwrap();
        assert_eq!(eq.len(), 1);
        assert_eq!(eq[0].equity, d("9950"));

        store
            .save_market_tick("SOL-USDT", d("99.5"), d("100.5"), d("100"), now)
            .unwrap();
        store
            .save_market_tick("SOL-USDT", d("100.5"), d("101.5"), d("101"), now + 1)
            .unwrap();
        let px = store.price_series("SOL-USDT", 10).unwrap();
        assert_eq!(px.len(), 2);
        assert_eq!(px[0].mid, d("100"));
        assert_eq!(px[1].mid, d("101"));

        let ov = store
            .overview(
                "paper_acc",
                Some(&PnLBook {
                    components: crate::domain::PnLComponents {
                        trading_fees: d("0.025"),
                        ..Default::default()
                    },
                }),
                false,
            )
            .unwrap();
        assert_eq!(ov.fills, 1);
        assert_eq!(ov.trading_fees, d("0.025"));
    }
}
