use std::time::{Duration, Instant};

pub struct TokenBudget {
    pub max_tokens: usize,
    pub current: usize,
}

impl TokenBudget {
    pub fn new(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            current: 0,
        }
    }

    pub fn remaining(&self) -> usize {
        self.max_tokens.saturating_sub(self.current)
    }

    pub fn can_afford(&self, estimated_tokens: usize) -> bool {
        self.current + estimated_tokens <= self.max_tokens
    }

    pub fn consume(&mut self, tokens: usize) {
        self.current += tokens;
    }

    pub fn reset(&mut self) {
        self.current = 0;
    }

    /// Estimate token count for a string.
    /// A common rule of thumb is 1 token ~ 4 characters.
    pub fn estimate_tokens(text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        // Basic heuristic: 1 token approx 4 chars
        text.len().div_ceil(4)
    }
}

pub struct TokenRateLimiter {
    capacity: usize,
    tokens: f64,
    last_refill: Instant,
    refill_rate_per_sec: f64,
}

impl TokenRateLimiter {
    pub fn new(tokens_per_minute: usize) -> Self {
        let refill_rate_per_sec = tokens_per_minute as f64 / 60.0;
        Self {
            capacity: tokens_per_minute,
            tokens: tokens_per_minute as f64,
            last_refill: Instant::now(),
            refill_rate_per_sec,
        }
    }

    pub fn check_and_withdraw(&mut self, amount: usize) -> Duration {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        // Refill
        self.tokens = (self.tokens + elapsed * self.refill_rate_per_sec).min(self.capacity as f64);
        self.last_refill = now;

        if self.tokens >= amount as f64 {
            self.tokens -= amount as f64;
            Duration::ZERO
        } else {
            let deficit = amount as f64 - self.tokens;
            let wait_secs = deficit / self.refill_rate_per_sec;
            self.tokens -= amount as f64;
            Duration::from_secs_f64(wait_secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_management() {
        let mut budget = TokenBudget::new(100);
        assert_eq!(budget.remaining(), 100);

        budget.consume(20);
        assert_eq!(budget.remaining(), 80);
        assert_eq!(budget.current, 20);

        assert!(budget.can_afford(10));
        assert!(!budget.can_afford(90));
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(TokenBudget::estimate_tokens(""), 0);
        assert_eq!(TokenBudget::estimate_tokens("1234"), 1);
        assert_eq!(TokenBudget::estimate_tokens("12345"), 2);
    }

    #[test]
    fn test_rate_limiter() {
        // 60 tokens per minute = 1 per second
        let mut limiter = TokenRateLimiter::new(60);

        // Immediate consume
        assert_eq!(limiter.check_and_withdraw(10), Duration::ZERO);

        // Consume more than available (starts full at 60, used 10, left 50)
        // Request 60. Deficit 10. Wait 10s.
        let wait = limiter.check_and_withdraw(60);
        assert!(wait.as_secs_f64() >= 9.9 && wait.as_secs_f64() <= 10.1);
    }
}
