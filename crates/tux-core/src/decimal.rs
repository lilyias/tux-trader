//! Decimal helpers. Price / qty / fees never use `f64`.

use rust_decimal::Decimal;

pub type Dec = Decimal;
pub type Qty = Decimal;

/// Round toward zero to a multiple of `step` (tick size / qty step).
pub fn round_to_step(value: Decimal, step: Decimal) -> Option<Decimal> {
    if step <= Decimal::ZERO {
        return None;
    }
    Some((value / step).trunc() * step)
}

pub fn is_multiple_of(value: Decimal, step: Decimal) -> bool {
    if step <= Decimal::ZERO {
        return false;
    }
    round_to_step(value, step) == Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn rounds_to_tick() {
        let tick = d("0.01");
        assert_eq!(round_to_step(d("1.239"), tick), Some(d("1.23")));
        assert_eq!(round_to_step(d("-1.239"), tick), Some(d("-1.23")));
        assert!(is_multiple_of(d("1.23"), tick));
        assert!(!is_multiple_of(d("1.235"), tick));
    }
}
