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

use crate::ai::token_budget::TokenBudget;
use std::ops::Range;

pub struct Truncator;

impl Truncator {
    /// Truncates a diff output if it's too large.
    /// Preserves the header and checks for balanced chunks.
    pub fn truncate_diff(diff: &str, max_tokens: usize) -> String {
        let estimated = TokenBudget::estimate_tokens(diff);
        if estimated <= max_tokens {
            return diff.to_string();
        }

        let max_chars = max_tokens * 4;
        let lines: Vec<&str> = diff.lines().collect();
        let total_lines = lines.len();

        // Heuristic: If total lines is small but content is huge, we have long lines.
        // We calculate 'allowed_lines' based on a conservative average line length (e.g. 50 chars).
        let allowed_lines = max_chars / 50;

        if total_lines <= allowed_lines {
            // Vulnerability Fix: If we are here, estimated > max_tokens.
            // But line count is small. This implies huge lines.
            // We must perform character-based truncation.
            let kept: String = diff.chars().take(max_chars).collect();
            return format!(
                "{}\n... [Output truncated. Content too large ({} tokens). Displaying first {} chars] ...\n",
                kept, estimated, max_chars
            );
        }

        let keep_top = allowed_lines / 2;
        let keep_bottom = allowed_lines / 2;

        if keep_top + keep_bottom >= total_lines {
            // Should be covered by above check, but safety fallback
            let kept: String = diff.chars().take(max_chars).collect();
            return format!(
                "{}\n... [Output truncated. Content too large. Displaying first {} chars] ...\n",
                kept, max_chars
            );
        }

        let mut result = String::new();
        for line in &lines[..keep_top] {
            result.push_str(line);
            result.push('\n');
        }

        result.push_str(&format!(
            "\n... [Diff truncated. Dropped {} lines] ...\n\n",
            total_lines - (keep_top + keep_bottom)
        ));

        for line in &lines[total_lines - keep_bottom..] {
            result.push_str(line);
            result.push('\n');
        }

        // Final Safety Check
        if TokenBudget::estimate_tokens(&result) > max_tokens {
            let kept: String = result.chars().take(max_chars).collect();
            return format!(
                "{}\n... [Output truncated after line filtering. Original size: {} tokens] ...\n",
                kept, estimated
            );
        }

        result
    }

    /// Smart truncation for code files.
    /// tries to preserve context around `focus_lines`.
    ///
    /// `focus_lines` is 1-based inclusive range.
    pub fn truncate_code(
        content: &str,
        focus_lines: Option<Range<usize>>,
        max_tokens: usize,
    ) -> String {
        let estimated = TokenBudget::estimate_tokens(content);
        if estimated <= max_tokens {
            return content.to_string();
        }

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let (start_focus, end_focus) = if let Some(range) = focus_lines {
            (range.start.max(1) - 1, range.end.min(total_lines))
        } else {
            // If no focus, just take the top part?
            // Or maybe searching for "main" functions?
            // For now, let's default to head/tail strategy if no focus.
            return Self::truncate_diff(content, max_tokens);
        };

        // If we have focus, we want to expand around it.
        // Let's try to include 50 lines before and after.
        let context_size = 50;
        let start_keep = start_focus.saturating_sub(context_size);
        let end_keep = (end_focus + context_size).min(total_lines);

        let mut result = String::new();

        // Add header info if we are skipping the start
        if start_keep > 0 {
            result.push_str(&format!("... [{} lines collapsed] ...\n", start_keep));
        }

        for line in &lines[start_keep..end_keep] {
            result.push_str(line);
            result.push('\n');
        }

        if end_keep < total_lines {
            result.push_str(&format!(
                "... [{} lines collapsed] ...\n",
                total_lines - end_keep
            ));
        }

        // Final Safety Check
        if TokenBudget::estimate_tokens(&result) > max_tokens {
            let max_chars = max_tokens * 4;
            let kept: String = result.chars().take(max_chars).collect();
            return format!(
                "{}\n... [Output truncated. Code context too large. Original size: {} tokens] ...\n",
                kept, estimated
            );
        }

        result
    }

    // Future: Add AST-based collapsing here.
    #[allow(dead_code)]
    fn regex_collapse(_content: &str) -> String {
        // Placeholder for regex based function collapsing
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_diff() {
        let diff = "line1\nline2\nline3\nline4\nline5\nline6";
        // budget 5 tokens (~20 chars) < 30 chars input -> should truncate
        let truncated = Truncator::truncate_diff(diff, 5);
        assert!(truncated.contains("Diff truncated"));
    }

    #[test]
    fn test_truncate_code_focus() {
        let code = (0..200)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        // Budget 300 tokens.
        // Full file ~350 tokens.
        // Collapsed (100 lines) ~175 tokens.
        // So 350 > 300 -> Collapses.
        // 175 < 300 -> Returns collapsed content.
        let truncated = Truncator::truncate_code(&code, Some(100..105), 300);
        // It should keep lines around 100-105.
        assert!(truncated.contains("line 100"));
        assert!(truncated.contains("line 105"));
        // It should have collapsed the start
        assert!(truncated.contains("lines collapsed"));
    }

    #[test]
    fn test_truncate_diff_long_line() {
        // 1000 chars "a", but max_tokens = 20 (approx 80 chars)
        // allowed_lines = 80/50 = 1.
        // total_lines = 1.
        // 1 <= 1 -> Triggers long line logic.
        let long_line = "a".repeat(1000);
        let truncated = Truncator::truncate_diff(&long_line, 20);

        // Should strictly be around max_tokens * 4 + overhead of message
        assert!(truncated.len() < 300);
        assert!(truncated.contains("Output truncated"));
        assert!(truncated.starts_with("aaaa"));
    }
}
