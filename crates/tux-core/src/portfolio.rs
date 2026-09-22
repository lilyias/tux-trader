//! Portfolio, exposure and event-sourced PnL.
//!
//! Spot balance ≠ contract Position; both roll up into Exposure.

use std::collections::HashMap;
use rust_decimal::Decimal;

use crate::domain::{Balance, Exposure, Instrument, PnLComponents, Position};
use crate::ids::{AccountId, TimestampMs};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PnLBook {
    pub components: PnLComponents,
}

impl PnLBook {
    pub fn net_pnl(&self) -> Decimal {
        self.components.net_pnl()
    }

    pub fn apply_fee(&mut self, fee: Decimal) {
        self.components.trading_fees += fee;
    }

    pub fn apply_realized(&mut self, realized: Decimal) {
        self.components.gross_realized_pnl += realized;
    }

    pub fn apply_funding(&mut self, payment: Decimal) {
        self.components.funding_fees += payment;
    }

    pub fn set_unrealized(&mut self, unrealized: Decimal) {
        self.components.unrealized_pnl = unrealized;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AccountingEvent {
    Fee { asset: String, amount: Decimal, at: TimestampMs },
    FillApplied { fill_id: String, at: TimestampMs },
    ManualAdjustment {
        asset: String,
        delta: Decimal,
        operator: String,
        note: String,
        at: TimestampMs,
    },
}

use serde::{Deserialize, Serialize};

#[derive(Debug, Default)]
pub struct Portfolio {
    balances: HashMap<String, Balance>,
    positions: HashMap<String, Position>,
    pnl: HashMap<String, PnLBook>,
    ledger: Vec<AccountingEvent>,
}

impl Portfolio {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed_balance(&mut self, balance: Balance) {
        self.balances.insert(
            format!("{}:{}", balance.account_id, balance.asset),
            balance,
        );
    }

    pub fn balance(&self, account_id: &AccountId, asset: &str) -> Option<&Balance> {
        self.balances.get(&format!("{account_id}:{asset}"))
    }

    /// All balances for one account (dashboard / persistence).
    pub fn balances_for(&self, account_id: &AccountId) -> Vec<&Balance> {
        self.balances
            .values()
            .filter(|b| &b.account_id == account_id)
            .collect()
    }

    pub fn upsert_position(&mut self, position: Position) {
        self.positions.insert(
            format!("{}:{}", position.account_id, position.instrument_id),
            position,
        );
    }

    pub fn position(&self, account_id: &AccountId, instrument: &InstrumentIdKey) -> Option<&Position> {
        self.positions.get(&format!("{}:{}", account_id, instrument.0))
    }

    pub fn pnl_book(&self, account_id: &AccountId) -> Option<&PnLBook> {
        self.pnl.get(account_id.as_str())
    }

    pub fn pnl_mut(&mut self, account_id: &AccountId) -> &mut PnLBook {
        self.pnl.entry(account_id.to_string()).or_default()
    }

    pub fn ledger(&self) -> &[AccountingEvent] {
        &self.ledger
    }

    /// Apply a spot fill: adjust free balances and fees.
    /// Buy: spend quote, receive base. Sell: spend base, receive quote.
    /// Fails if free balance would go negative (no naked short / overdraft).
    #[allow(clippy::too_many_arguments)]
    pub fn apply_spot_fill(
        &mut self,
        account_id: &AccountId,
        instrument: &Instrument,
        side: crate::domain::OrderSide,
        price: Decimal,
        qty: Decimal,
        fee_amount: Decimal,
        fee_asset: &str,
    ) -> Result<(), crate::error::TuxError> {
        use crate::error::TuxError;
        let notional = price * qty;
        let now = crate::ids::now_ms();
        // Preflight free-balance checks.
        let quote_free = self
            .balance(account_id, &instrument.quote_asset)
            .map(|b| b.free)
            .unwrap_or(Decimal::ZERO);
        let base_free = self
            .balance(account_id, &instrument.base_asset)
            .map(|b| b.free)
            .unwrap_or(Decimal::ZERO);
        match side {
            crate::domain::OrderSide::Buy => {
                if quote_free < notional + if fee_asset == instrument.quote_asset { fee_amount } else { Decimal::ZERO }
                {
                    return Err(TuxError::InvalidOrder(format!(
                        "insufficient {quote_free} {} for buy notional {notional}",
                        instrument.quote_asset
                    )));
                }
            }
            crate::domain::OrderSide::Sell => {
                if base_free < qty {
                    return Err(TuxError::InvalidOrder(format!(
                        "insufficient {base_free} {} for sell qty {qty}",
                        instrument.base_asset
                    )));
                }
            }
        }

        match side {
            crate::domain::OrderSide::Buy => {
                self.add_free(account_id, &instrument.quote_asset, -notional);
                self.add_free(account_id, &instrument.base_asset, qty);
            }
            crate::domain::OrderSide::Sell => {
                self.add_free(account_id, &instrument.base_asset, -qty);
                self.add_free(account_id, &instrument.quote_asset, notional);
            }
        }
        if fee_amount > Decimal::ZERO {
            self.add_free(account_id, fee_asset, -fee_amount);
            self.pnl_mut(account_id).apply_fee(fee_amount);
            self.ledger.push(AccountingEvent::Fee {
                asset: fee_asset.to_string(),
                amount: fee_amount,
                at: now,
            });
        }
        self.ledger.push(AccountingEvent::FillApplied {
            fill_id: format!("{}:{}:{}", account_id, instrument.id, now),
            at: now,
        });
        Ok(())
    }

    fn add_free(&mut self, account_id: &AccountId, asset: &str, delta: Decimal) {
        let key = format!("{account_id}:{asset}");
        let entry = self.balances.entry(key).or_insert_with(|| Balance {
            account_id: account_id.clone(),
            asset: asset.to_string(),
            free: Decimal::ZERO,
            locked: Decimal::ZERO,
            updated_at: crate::ids::now_ms(),
        });
        entry.free += delta;
        entry.updated_at = crate::ids::now_ms();
    }

    /// Unified exposure.
    /// - `net_notional`: signed sum (spot long + contract size can cancel)
    /// - `gross_notional`: sum of absolute notionals (spot + |contracts|), **not** `abs(net)`
    pub fn exposure(
        &self,
        account_id: &AccountId,
        instrument: &Instrument,
        mark_price: Decimal,
    ) -> Exposure {
        let mut net = Decimal::ZERO;
        let mut gross = Decimal::ZERO;

        if let Some(bal) = self.balance(account_id, &instrument.base_asset) {
            let n = bal.total() * mark_price;
            net += n;
            gross += n.abs();
        }

        if let Some(pos) = self
            .positions
            .get(&format!("{}:{}", account_id, instrument.id))
        {
            let n = pos.size * instrument.contract_multiplier * mark_price;
            net += n;
            gross += n.abs();
        }

        Exposure {
            account_id: account_id.clone(),
            instrument_id: instrument.id.clone(),
            net_notional: net,
            gross_notional: gross,
            updated_at: crate::ids::now_ms(),
        }
    }

    /// Mark-to-market equity in `quote`.
    /// `marks`: asset → price in quote (quote itself is 1).
    pub fn equity_mtm(
        &self,
        account_id: &AccountId,
        quote: &str,
        marks: &HashMap<String, Decimal>,
    ) -> Decimal {
        let mut eq = Decimal::ZERO;
        for b in self.balances.values() {
            if &b.account_id != account_id {
                continue;
            }
            if b.asset == quote {
                eq += b.total();
            } else {
                let px = marks
                    .get(&b.asset)
                    .copied()
                    .or_else(|| marks.get(&b.asset.to_ascii_uppercase()).copied())
                    .unwrap_or(Decimal::ZERO);
                eq += b.total() * px;
            }
        }
        eq += self
            .pnl
            .get(account_id.as_str())
            .map(|p| p.components.unrealized_pnl)
            .unwrap_or(Decimal::ZERO);
        eq
    }

    /// Legacy helper: equity only correct when non-quote balances are zero.
    pub fn equity(&self, account_id: &AccountId) -> Decimal {
        let marks = HashMap::new();
        // Without marks non-quote assets count as 0 — use equity_mtm in real paths.
        self.equity_mtm(account_id, "USDT", &marks)
    }
}

/// Key helper for position lookup.
pub struct InstrumentIdKey<'a>(pub &'a str);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Instrument;
    use crate::ids::Venue;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn gross_exposure_is_not_abs_net() {
        let acc = AccountId::from("a1");
        let inst = Instrument::spot("SOL-USDT", Venue::LocalPaper, "SOL", "USDT", d("0.01"), d("0.001"));
        let mut p = Portfolio::new();
        // 1 SOL spot long @ 100 = +100 notional
        p.seed_balance(Balance {
            account_id: acc.clone(),
            asset: "SOL".into(),
            free: d("1"),
            locked: Decimal::ZERO,
            updated_at: 0,
        });
        // contract-style short -1 size * 1 multiplier * 100 = -100
        p.upsert_position(Position {
            account_id: acc.clone(),
            instrument_id: inst.id.clone(),
            position_mode: crate::domain::PositionMode::OneWay,
            margin_mode: crate::domain::MarginMode::Cross,
            position_side: crate::domain::PositionSide::Both,
            size: d("-1"),
            entry_price: d("100"),
            mark_price: d("100"),
            liquidation_price: None,
            leverage: d("1"),
            unrealized_pnl: Decimal::ZERO,
            margin: Decimal::ZERO,
            updated_at: 0,
        });
        let exp = p.exposure(&acc, &inst, d("100"));
        assert_eq!(exp.net_notional, Decimal::ZERO);
        assert_eq!(exp.gross_notional, d("200"), "gross must not cancel");
    }

    #[test]
    fn equity_is_mark_to_market() {
        let acc = AccountId::from("a1");
        let mut p = Portfolio::new();
        p.seed_balance(Balance {
            account_id: acc.clone(),
            asset: "USDT".into(),
            free: d("9949.975"),
            locked: Decimal::ZERO,
            updated_at: 0,
        });
        p.seed_balance(Balance {
            account_id: acc.clone(),
            asset: "SOL".into(),
            free: d("0.5"),
            locked: Decimal::ZERO,
            updated_at: 0,
        });
        let mut marks = HashMap::new();
        marks.insert("SOL".to_string(), d("100"));
        // 9949.975 + 0.5*100 = 9999.975  (NOT 9950.475)
        assert_eq!(p.equity_mtm(&acc, "USDT", &marks), d("9999.975"));
    }

    #[test]
    fn apply_spot_fill_rejects_overdraft() {
        let acc = AccountId::from("a1");
        let inst = Instrument::spot("SOL-USDT", Venue::LocalPaper, "SOL", "USDT", d("0.01"), d("0.001"));
        let mut p = Portfolio::new();
        p.seed_balance(Balance {
            account_id: acc.clone(),
            asset: "USDT".into(),
            free: d("10"),
            locked: Decimal::ZERO,
            updated_at: 0,
        });
        let err = p
            .apply_spot_fill(&acc, &inst, crate::domain::OrderSide::Buy, d("100"), d("1"), Decimal::ZERO, "USDT");
        assert!(err.is_err());
    }
}
