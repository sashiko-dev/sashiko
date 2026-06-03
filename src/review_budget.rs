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

pub fn is_token_budget_failure_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("token budget exceeded")
        || lower.contains("output token budget exceeded")
        || lower.contains("max_input_tokens")
        || lower.contains("preflight cap")
        || lower.contains("prompt estimate")
        || lower.contains("budgetexceeded")
}

pub fn local_prompt_preflight_cap(max_input_tokens: usize) -> usize {
    max_input_tokens.saturating_mul(90) / 100
}

pub fn prompt_preflight_cap(max_input_tokens: usize, bounded_local_model: bool) -> usize {
    if bounded_local_model {
        local_prompt_preflight_cap(max_input_tokens)
    } else {
        max_input_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_budget_failure_messages() {
        assert!(is_token_budget_failure_message(
            "Token budget exceeded before stage 9"
        ));
        assert!(is_token_budget_failure_message(
            "prompt estimate 55000 exceeds preflight cap 54000"
        ));
        assert!(is_token_budget_failure_message(
            "request exceeds max_input_tokens"
        ));
        assert!(!is_token_budget_failure_message(
            "Stage 9 failed to produce JSON"
        ));
    }

    #[test]
    fn applies_local_preflight_margin_only_in_bounded_mode() {
        assert_eq!(prompt_preflight_cap(60_000, true), 54_000);
        assert_eq!(prompt_preflight_cap(60_000, false), 60_000);
    }
}
