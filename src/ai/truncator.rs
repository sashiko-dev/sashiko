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

pub struct Truncator;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequentialTruncationResult {
    pub content: String,
    pub lines_kept: usize,
    pub truncated: bool,
}

impl Truncator {
    /// Truncates a diff output if it's too large.
    /// Preserves the header and checks for balanced chunks.
    pub fn truncate_diff(diff: &str, max_tokens: usize, label: &str) -> TruncationResult {
        let estimated = TokenBudget::estimate_tokens(diff);
        if estimated <= max_tokens {
            return TruncationResult {
                content: diff.to_string(),
                truncated: false,
            };
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
            return TruncationResult {
                content: format!(
                    "{}\n... [Output truncated. Content too large ({} tokens). Displaying first {} chars] ...\n",
                    kept, estimated, max_chars
                ),
                truncated: true,
            };
        }

        let keep_top = allowed_lines / 2;
        let keep_bottom = allowed_lines / 2;

        if keep_top + keep_bottom >= total_lines {
            // Should be covered by above check, but safety fallback
            let kept: String = diff.chars().take(max_chars).collect();
            return TruncationResult {
                content: format!(
                    "{}\n... [Output truncated. Content too large. Displaying first {} chars] ...\n",
                    kept, max_chars
                ),
                truncated: true,
            };
        }

        let mut result = String::new();
        for line in &lines[..keep_top] {
            result.push_str(line);
            result.push('\n');
        }

        result.push_str(&format!(
            "\n... [{} truncated. Dropped {} lines (lines {}-{})] ...\n\n",
            label,
            total_lines - (keep_top + keep_bottom),
            keep_top + 1,
            total_lines - keep_bottom
        ));

        for line in &lines[total_lines - keep_bottom..] {
            result.push_str(line);
            result.push('\n');
        }

        // Final Safety Check
        if TokenBudget::estimate_tokens(&result) > max_tokens {
            let kept: String = result.chars().take(max_chars).collect();
            return TruncationResult {
                content: format!(
                    "{}\n... [Output truncated after line filtering. Original size: {} tokens] ...\n",
                    kept, estimated
                ),
                truncated: true,
            };
        }

        TruncationResult {
            content: result,
            truncated: true,
        }
    }

    /// Sequentially truncates content, keeping only the first N lines/tokens.
    /// Appends a truncation warning.
    pub fn truncate_sequential(content: &str, max_tokens: usize) -> SequentialTruncationResult {
        let estimated = TokenBudget::estimate_tokens(content);
        if estimated <= max_tokens {
            return SequentialTruncationResult {
                content: content.to_string(),
                lines_kept: content.lines().count(),
                truncated: false,
            };
        }

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Binary search to find the maximum number of lines that fit within max_tokens
        let mut low = 0;
        let mut high = total_lines;
        let mut best_keep = 0;

        while low <= high {
            let mid = (low + high) / 2;
            let candidate = lines[..mid].join("\n");
            let cand_tokens = TokenBudget::estimate_tokens(&candidate);

            if cand_tokens <= max_tokens {
                best_keep = mid;
                low = mid + 1;
            } else {
                high = mid - 1;
            }
        }

        if best_keep == 0 {
            // Fallback to character-based truncation
            let max_chars = max_tokens * 4;
            let kept: String = content.chars().take(max_chars).collect();
            return SequentialTruncationResult {
                content: format!(
                    "{}\n... [Output truncated. Content too large ({} tokens). Displaying first {} chars] ...\n",
                    kept, estimated, max_chars
                ),
                lines_kept: 0,
                truncated: true,
            };
        }

        let mut result = lines[..best_keep].join("\n");
        result.push('\n');

        let warning = format!(
            "... [Output truncated. Dropped {} lines. Original size: {} tokens] ...\n",
            total_lines - best_keep,
            estimated
        );

        // Adjust best_keep if adding the warning pushes us over budget
        while best_keep > 0 {
            let candidate = format!("{}{}", result, warning);
            if TokenBudget::estimate_tokens(&candidate) <= max_tokens {
                return SequentialTruncationResult {
                    content: candidate,
                    lines_kept: best_keep,
                    truncated: true,
                };
            }
            best_keep -= 1;
            result = lines[..best_keep].join("\n");
            if best_keep > 0 {
                result.push('\n');
            }
        }

        let max_chars = max_tokens * 4;
        let kept: String = content.chars().take(max_chars).collect();
        SequentialTruncationResult {
            content: format!(
                "{}\n... [Output truncated. Content too large ({} tokens). Displaying first {} chars] ...\n",
                kept, estimated, max_chars
            ),
            lines_kept: 0,
            truncated: true,
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_diff() {
        let diff = "line1\nline2\nline3\nline4\nline5\nline6";
        // budget 5 tokens (~20 chars) < 30 chars input -> should truncate
        let res = Truncator::truncate_diff(diff, 5, "Diff");
        assert!(res.content.contains("Diff truncated"));
        assert!(res.truncated);
    }

    #[test]
    fn test_truncate_diff_long_line() {
        // 1000 chars "a", but max_tokens = 20 (approx 80 chars)
        // allowed_lines = 80/50 = 1.
        // total_lines = 1.
        // 1 <= 1 -> Triggers long line logic.
        let long_line = "a".repeat(1000);
        let res = Truncator::truncate_diff(&long_line, 20, "Diff");

        // Should strictly be around max_tokens * 4 + overhead of message
        assert!(res.content.len() < 300);
        assert!(res.content.contains("Output truncated"));
        assert!(res.content.starts_with("aaaa"));
        assert!(
            res.truncated,
            "Should be marked as truncated despite being a single line"
        );
    }

    #[test]
    fn test_truncate_sequential() {
        let content = (0..100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let res = Truncator::truncate_sequential(&content, 50);
        assert!(res.content.contains("line 0"));
        assert!(res.content.contains("Output truncated. Dropped"));
        assert!(!res.content.contains("line 99"));
        assert!(res.truncated);
        assert!(res.lines_kept > 0);
        assert!(res.lines_kept < 100);
    }

    #[test]
    fn test_truncate_diff_precise_range() {
        let diff = (1..=20)
            .map(|i| format!("diff line {} padding text", i))
            .collect::<Vec<_>>()
            .join("\n");
        // budget 80 tokens -> allowed_lines = (80 * 4) / 50 = 6 lines.
        // keep_top = 3, keep_bottom = 3. Total 20 lines.
        // Dropped lines count: 14. Range: 4 to 17.
        let res = Truncator::truncate_diff(&diff, 80, "Diff");
        assert!(res.truncated);
        assert!(
            res.content
                .contains("Diff truncated. Dropped 14 lines (lines 4-17)")
        );
        assert!(res.content.contains("diff line 1 "));
        assert!(res.content.contains("diff line 3 "));
        assert!(res.content.contains("diff line 18 "));
        assert!(res.content.contains("diff line 20"));
    }

}
