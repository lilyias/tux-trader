//! Rate limiting + exponential backoff.

use std::collections::VecDeque;
use std::sync::Mutex;
use tux_core::ids::now_ms;

pub struct RateLimiter {
    limit_per_minute: u32,
    hits: Mutex<VecDeque<i64>>,
}

impl RateLimiter {
    pub fn new(limit_per_minute: u32) -> Self {
        Self {
            limit_per_minute,
            hits: Mutex::new(VecDeque::new()),
        }
    }

    pub fn try_acquire(&self) -> bool {
        let now = now_ms();
        let window_start = now - 60_000;
        let mut hits = self.hits.lock().unwrap();
        while let Some(&t) = hits.front() {
            if t < window_start {
                hits.pop_front();
            } else {
                break;
            }
        }
        if hits.len() as u32 >= self.limit_per_minute {
            return false;
        }
        hits.push_back(now);
        true
    }
}

pub fn backoff_ms(attempt: u32, base_ms: u64, max_ms: u64) -> u64 {
    base_ms.saturating_mul(1u64 << attempt.min(20)).min(max_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_requests() {
        let rl = RateLimiter::new(2);
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        assert!(!rl.try_acquire());
    }

    #[test]
    fn backoff_grows() {
        assert_eq!(backoff_ms(0, 100, 10_000), 100);
        assert_eq!(backoff_ms(1, 100, 10_000), 200);
        assert_eq!(backoff_ms(20, 100, 10_000), 10_000);
    }
}
