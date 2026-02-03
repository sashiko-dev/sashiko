// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::OnceLock;
use tiktoken_rs::{cl100k_base, CoreBPE};

pub struct TokenBudget {
    pub max_tokens: usize,
    pub current: usize,
}

static TOKENIZER: OnceLock<CoreBPE> = OnceLock::new();

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

    /// Estimate token count for a string using cl100k_base (GPT-4/Gemini approximation).
    pub fn estimate_tokens(text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let bpe = TOKENIZER.get_or_init(|| cl100k_base().expect("Failed to load cl100k_base tokenizer"));
        bpe.encode_with_special_tokens(text).len()
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
        // Use strings that are more stable across tokenizer versions if possible,
        // or just accept what the tokenizer says.
        let t1 = TokenBudget::estimate_tokens("hello");
        assert!(t1 >= 1);
        let t2 = TokenBudget::estimate_tokens("hello world");
        assert!(t2 > t1);
    }

    #[test]
    fn test_estimate_tokens_performance() {
        let text = "Hello world this is a test string to estimate tokens for.";
        let start = std::time::Instant::now();
        // 1,000 iterations is enough to detect regression.
        // Optimized: ~0.15s.
        // Unoptimized: ~30s.
        let iterations = 1_000;

        for _ in 0..iterations {
            let _ = TokenBudget::estimate_tokens(text);
        }

        let duration = start.elapsed();
        println!("Time for {} iterations: {:?}", iterations, duration);

        assert!(duration.as_secs() < 1, "Token estimation is too slow! {:?} for {} iterations", duration, iterations);
    }
}
