//! Independent risk engine — all configured limits are enforced.
//!
//! Market orders use mid/last as notional reference; without a reference
//! price they are rejected (cannot silently bypass max_order_notional).

use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};

use crate::domain::{Instrument, OrderIntent, OrderType, RiskDecision, RiskLimitConfig};
use crate::ids::{now_ms, AccountId, StrategyInstanceId, TimestampMs};
use crate::portfolio::Portfolio;

#[derive(Debug, Default)]
pub struct KillSwitch {
    pub engaged: bool,
    pub reason: Option<String>,
    pub engaged_at: Option<TimestampMs>,
}

impl KillSwitch {
    pub fn engage(&mut self, reason: impl Into<String>) {
        self.engaged = true;
        self.reason = Some(reason.into());
        self.engaged_at = Some(now_ms());
    }

    pub fn release(&mut self) {
        self.engaged = false;
        self.reason = None;
        self.engaged_at = None;
    }
}

pub struct RiskEngine {
    pub limits: RiskLimitConfig,
    pub kill_switch: KillSwitch,
    consecutive_rejects: u32,
    /// Rolling window of order-check timestamps (rate limit).
    order_times: VecDeque<TimestampMs>,
    /// Active (non-terminal) order notionals by strategy / account.
    open_notional_by_strategy: HashMap<String, Decimal>,
    open_notional_by_account: HashMap<String, Decimal>,
    active_orders_by_strategy: HashMap<String, u32>,
    daily_realized_pnl: Decimal,
    high_water_equity: Decimal,
    /// Set false when market/account stream drops and pause_on_disconnect is set.
    feeds_connected: bool,
}

impl RiskEngine {
    pub fn new(limits: RiskLimitConfig) -> Self {
        Self {
            limits,
            kill_switch: KillSwitch::default(),
            consecutive_rejects: 0,
            order_times: VecDeque::new(),
            open_notional_by_strategy: HashMap::new(),
            open_notional_by_account: HashMap::new(),
            active_orders_by_strategy: HashMap::new(),
            daily_realized_pnl: Decimal::ZERO,
            high_water_equity: Decimal::ZERO,
            feeds_connected: true,
        }
    }

    /// Safe default for process startup: no trading until explicitly enabled.
    pub fn new_safe_default() -> Self {
        let mut e = Self::new(RiskLimitConfig::default());
        e.kill_switch.engage("startup safe-default");
        e
    }

    pub fn set_feeds_connected(&mut self, connected: bool) {
        self.feeds_connected = connected;
    }

    /// Feed PnL / equity facts used by daily-loss and drawdown checks.
    pub fn update_pnl_facts(&mut self, equity: Decimal, daily_realized_pnl: Decimal) {
        self.daily_realized_pnl = daily_realized_pnl;
        if equity > self.high_water_equity {
            self.high_water_equity = equity;
        }
    }

    /// Call when an order reaches a terminal state so open-notional caps free up.
    pub fn note_order_terminal(
        &mut self,
        strategy: &StrategyInstanceId,
        account: &AccountId,
        notional: Decimal,
    ) {
        let s = strategy.as_str().to_string();
        let a = account.as_str().to_string();
        *self.open_notional_by_strategy.entry(s.clone()).or_default() -= notional;
        *self.open_notional_by_account.entry(a).or_default() -= notional;
        let c = self.active_orders_by_strategy.entry(s).or_default();
        *c = c.saturating_sub(1);
    }

    /// Re-book an open order after process restart.
    pub fn note_order_restored(
        &mut self,
        strategy: &StrategyInstanceId,
        account: &AccountId,
        notional: Decimal,
    ) {
        let s = strategy.as_str().to_string();
        let a = account.as_str().to_string();
        *self.open_notional_by_strategy.entry(s.clone()).or_default() += notional;
        *self.open_notional_by_account.entry(a).or_default() += notional;
        let c = self.active_orders_by_strategy.entry(s).or_default();
        *c += 1;
    }

    pub fn check_intent(
        &mut self,
        intent: &OrderIntent,
        instrument: &Instrument,
        market_ref: Option<Decimal>,
        market_age_ms: Option<u64>,
        portfolio: &Portfolio,
    ) -> RiskDecision {
        if self.kill_switch.engaged {
            return self.reject(
                "kill_switch",
                self.kill_switch
                    .reason
                    .clone()
                    .unwrap_or_else(|| "engaged".into()),
            );
        }

        if self.limits.pause_on_disconnect && !self.feeds_connected {
            return self.reject("feeds_down", "market/account feed disconnected");
        }

        // Daily loss / drawdown (from portfolio facts).
        if let Some(max_loss) = self.limits.max_daily_loss {
            if self.daily_realized_pnl < Decimal::ZERO && (-self.daily_realized_pnl) > max_loss {
                return self.reject(
                    "max_daily_loss",
                    format!(
                        "daily loss {} exceeds {}",
                        self.daily_realized_pnl, max_loss
                    ),
                );
            }
        }
        if let (Some(max_dd), true) = (self.limits.max_drawdown, !self.high_water_equity.is_zero())
        {
            let mut marks = std::collections::HashMap::new();
            marks.insert(instrument.quote_asset.clone(), Decimal::ONE);
            if let Some(px) = market_ref {
                marks.insert(instrument.base_asset.clone(), px);
            }
            let equity = portfolio.equity_mtm(&intent.account_id, &instrument.quote_asset, &marks);
            let dd = (self.high_water_equity - equity) / self.high_water_equity;
            if dd > max_dd {
                return self.reject("max_drawdown", format!("drawdown {dd} exceeds {max_dd}"));
            }
        }

        // Whitelist.
        if self.limits.instrument_whitelist.is_empty() {
            return self.reject("instrument_whitelist", "whitelist empty = deny all");
        }
        if !self
            .limits
            .instrument_whitelist
            .iter()
            .any(|i| i == &instrument.id)
        {
            return self.reject(
                "instrument_whitelist",
                format!("{} not in whitelist", instrument.id),
            );
        }

        // Instrument filters.
        if let Some(px) = intent.price {
            if !instrument.validate_price(px) {
                return self.reject("price_filter", "price violates tick size");
            }
        }
        if !instrument.validate_qty(intent.quantity) {
            return self.reject("lot_size", "quantity violates step/min/max");
        }

        // Notional — market orders MUST use market_ref (mid/last).
        let notional = match intent.notional(market_ref) {
            Some(n) => n,
            None => {
                return self.reject(
                    "missing_reference_price",
                    "market order without market reference price; notional unknown",
                );
            }
        };

        if intent.order_type != OrderType::Market && !instrument.validate_notional(notional) {
            return self.reject("min_notional", "below min notional");
        }
        if instrument.min_notional.is_some() && !instrument.validate_notional(notional) {
            return self.reject("min_notional", "below min notional");
        }

        if let Some(max) = self.limits.max_order_notional {
            if notional > max {
                return self.reject(
                    "max_order_notional",
                    format!("order notional {notional} exceeds {max}"),
                );
            }
        }

        // Strategy / account open notional caps.
        if let Some(max) = self.limits.max_strategy_notional {
            let open = self
                .open_notional_by_strategy
                .get(intent.strategy_instance_id.as_str())
                .copied()
                .unwrap_or_default();
            if open + notional > max {
                return self.reject(
                    "max_strategy_notional",
                    format!("strategy open+new {open}+{notional} exceeds {max}"),
                );
            }
        }
        if let Some(max) = self.limits.max_account_notional {
            let open = self
                .open_notional_by_account
                .get(intent.account_id.as_str())
                .copied()
                .unwrap_or_default();
            if open + notional > max {
                return self.reject(
                    "max_account_notional",
                    format!("account open+new {open}+{notional} exceeds {max}"),
                );
            }
        }

        // Net exposure cap (existing + this order's signed notional).
        if let Some(max) = self.limits.max_net_exposure {
            let signed = match intent.side {
                crate::domain::OrderSide::Buy => notional,
                crate::domain::OrderSide::Sell => -notional,
            };
            let mark = market_ref.unwrap_or_else(|| {
                if intent.quantity.is_zero() {
                    Decimal::ZERO
                } else {
                    notional / intent.quantity
                }
            });
            let exp = portfolio.exposure(&intent.account_id, instrument, mark);
            let projected = exp.net_notional + signed;
            if projected.abs() > max {
                return self.reject(
                    "max_net_exposure",
                    format!("projected net exposure {projected} exceeds {max}"),
                );
            }
        }

        // Leverage.
        if let (Some(max_lev), Some(lev)) = (self.limits.max_leverage, intent.leverage) {
            if lev > max_lev {
                return self.reject("max_leverage", format!("leverage {lev} exceeds {max_lev}"));
            }
        }

        // Active order count.
        if let Some(max) = self.limits.max_active_orders {
            let n = self
                .active_orders_by_strategy
                .get(intent.strategy_instance_id.as_str())
                .copied()
                .unwrap_or(0);
            if n >= max {
                return self.reject("max_active_orders", format!("{n} active orders >= {max}"));
            }
        }

        // Order rate per minute.
        let now = now_ms();
        if let Some(max) = self.limits.max_order_rate_per_min {
            while let Some(&t) = self.order_times.front() {
                if t < now - 60_000 {
                    self.order_times.pop_front();
                } else {
                    break;
                }
            }
            if self.order_times.len() as u32 >= max {
                return self.reject(
                    "max_order_rate_per_min",
                    format!("{} orders/min >= {max}", self.order_times.len()),
                );
            }
        }

        // Stale market.
        if let Some(max_age) = self.limits.max_market_age_ms {
            match market_age_ms {
                None => return self.reject("stale_market", "no market data"),
                Some(age) if age > max_age => {
                    return self
                        .reject("stale_market", format!("market age {age}ms > {max_age}ms"));
                }
                _ => {}
            }
        }

        // Price deviation vs mid.
        if let (Some(max_dev), Some(mid), Some(px)) =
            (self.limits.max_price_deviation, market_ref, intent.price)
        {
            if !mid.is_zero() {
                let dev = ((px - mid) / mid).abs();
                if dev > max_dev {
                    return self.reject(
                        "price_deviation",
                        format!("deviation {dev} exceeds {max_dev}"),
                    );
                }
            }
        }

        // Approved — book open notional and rate window.
        self.consecutive_rejects = 0;
        self.order_times.push_back(now);
        let s = intent.strategy_instance_id.as_str().to_string();
        let a = intent.account_id.as_str().to_string();
        *self.open_notional_by_strategy.entry(s.clone()).or_default() += notional;
        *self.open_notional_by_account.entry(a).or_default() += notional;
        *self.active_orders_by_strategy.entry(s).or_default() += 1;
        RiskDecision::Approved
    }

    fn reject(&mut self, rule: &str, message: impl Into<String>) -> RiskDecision {
        self.consecutive_rejects += 1;
        if let Some(limit) = self.limits.consecutive_reject_limit {
            if self.consecutive_rejects >= limit && !self.kill_switch.engaged {
                self.kill_switch
                    .engage("consecutive rejects circuit breaker");
            }
        }
        RiskDecision::reject(rule, message)
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

    fn intent(px: Option<Decimal>, qty: Decimal) -> OrderIntent {
        OrderIntent {
            id: IntentId::new(),
            strategy_instance_id: StrategyInstanceId::from("s1"),
            account_id: AccountId::from("a1"),
            instrument_id: InstrumentId::new("SOL-USDT"),
            side: OrderSide::Buy,
            order_type: if px.is_some() {
                OrderType::Limit
            } else {
                OrderType::Market
            },
            time_in_force: TimeInForce::Gtc,
            price: px,
            quantity: qty,
            reduce_only: ReduceOnly::No,
            position_side: PositionSide::Both,
            leverage: None,
            client_order_id: ClientOrderId::from("c1"),
            reason: None,
            created_at: 0,
        }
    }

    fn inst() -> Instrument {
        Instrument::spot(
            "SOL-USDT",
            Venue::LocalPaper,
            "SOL",
            "USDT",
            d("0.01"),
            d("0.001"),
        )
    }

    fn cfg() -> RiskLimitConfig {
        RiskLimitConfig {
            instrument_whitelist: vec![InstrumentId::new("SOL-USDT")],
            max_order_notional: Some(d("1000")),
            max_market_age_ms: Some(60_000),
            consecutive_reject_limit: None,
            ..Default::default()
        }
    }

    #[test]
    fn market_order_uses_market_ref_for_notional() {
        let mut e = RiskEngine::new(cfg());
        let i = intent(None, d("10"));
        // mid=200 → notional 2000 > 1000
        let dec = e.check_intent(&i, &inst(), Some(d("200")), Some(1), &Portfolio::new());
        assert!(matches!(dec, RiskDecision::Rejected { rule, .. } if rule == "max_order_notional"));
    }

    #[test]
    fn market_order_without_price_rejected() {
        let mut e = RiskEngine::new(cfg());
        let i = intent(None, d("1"));
        let dec = e.check_intent(&i, &inst(), None, Some(1), &Portfolio::new());
        assert!(
            matches!(dec, RiskDecision::Rejected { rule, .. } if rule == "missing_reference_price")
        );
    }

    #[test]
    fn deny_all_when_whitelist_empty() {
        let mut e = RiskEngine::new(RiskLimitConfig {
            consecutive_reject_limit: None,
            ..Default::default()
        });
        let dec = e.check_intent(
            &intent(Some(d("100")), d("1")),
            &inst(),
            Some(d("100")),
            Some(1),
            &Portfolio::new(),
        );
        assert!(
            matches!(dec, RiskDecision::Rejected { rule, .. } if rule == "instrument_whitelist")
        );
    }
}
