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

use crate::ai::{AiMessage, AiProvider, AiRequest, AiResponseFormat, AiRole};
use crate::db::Database;
use crate::worker::prompts::SeriesMap;
use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::info;

const MAX_DIFF_CHARS_PER_PATCH: usize = 4000;
const MAX_TOTAL_CHARS: usize = 50_000;

const SERIES_MAP_SYSTEM_PROMPT: &str = "\
You are an expert system mapping cross-patch dependencies in a Linux kernel patch series.
Your task is to analyze the series and output a strict JSON object mapping:
1. All introduced symbols (functions, structs, macros) and which patches complete them.
2. All cross-patch fixes: where one patch introduces a bug, side-effect, or errant change \
that a later patch in the same series corrects.
   This includes accidental deletions restored by later patches, intermediate breakage \
fixed by subsequent patches, and fixup commits.
You must return ONLY the raw JSON object. Do NOT wrap it in markdown code blocks like ```json
Schema:
{
  \"introduced_symbols\": [
    {
      \"name\": \"string\",
      \"defined_in_patch_index\": number (1-based index),
      \"completed_in_patch_indices\": [number],
      \"description\": \"string\"
    }
  ],
  \"cross_patch_fixes\": [
    {
      \"introduced_in_patch_index\": number (1-based, the patch that introduces the issue),
      \"fixed_in_patch_index\": number (1-based, the patch that fixes it),
      \"description\": \"string (what was broken and how it is fixed)\"
    }
  ]
}
";

const SUMMARY_SYSTEM_PROMPT: &str = "\
You are an expert technical writer summarizing a Linux kernel patch series.
Provide a concise, high-level summary of what the series achieves, why it is needed, \
and any notable design choices.
Format as a single paragraph or bullet points. Do not mention patch numbers explicitly \
unless necessary.
";

/// Truncate a string to at most `max_bytes` bytes without splitting a
/// multi-byte UTF-8 character. Returns the largest valid prefix.
fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if max_bytes >= s.len() {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Build the user-content string from a cover letter and truncated diffs.
/// Shared by both `generate_series_map` and `generate_patchset_summary`.
fn build_diff_context(cover_letter: Option<&str>, diffs: &[&str]) -> String {
    let mut content = String::with_capacity(8192);
    if let Some(cl) = cover_letter {
        content.push_str(&format!("COVER LETTER:\n{}\n\n", cl));
    }
    for (i, diff) in diffs.iter().enumerate() {
        let header = format!("--- PATCH {} ---\n", i + 1);
        if content.len() + header.len() > MAX_TOTAL_CHARS {
            content.push_str("\n... (remaining patches omitted: token budget exhausted)\n");
            break;
        }
        content.push_str(&header);
        let remaining = MAX_TOTAL_CHARS.saturating_sub(content.len());
        if diff.len() > remaining.min(MAX_DIFF_CHARS_PER_PATCH) {
            content.push_str(safe_truncate(diff, remaining.min(MAX_DIFF_CHARS_PER_PATCH)));
            content.push_str("\n... (truncated)\n");
        } else {
            content.push_str(diff);
        }
        content.push_str("\n\n");
    }
    content
}

pub async fn generate_series_map(
    provider: &dyn AiProvider,
    cover_letter: Option<&str>,
    diffs: &[&str],
) -> Result<SeriesMap> {
    let user_content = build_diff_context(cover_letter, diffs);

    let request = AiRequest {
        system: Some(SERIES_MAP_SYSTEM_PROMPT.to_string()),
        messages: vec![AiMessage {
            role: AiRole::User,
            content: Some(user_content),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        temperature: Some(0.1),
        response_format: Some(AiResponseFormat::Json { schema: None }),
        context_tag: None,
    };

    let response = provider.generate_content(request).await?;
    let content = response.content.unwrap_or_default();

    let content = crate::utils::clean_json_string(&content);

    let map: SeriesMap =
        serde_json::from_str(&content).context("Failed to parse SeriesMap JSON")?;
    Ok(map)
}

pub async fn generate_patchset_summary(
    db: &Arc<Database>,
    provider: &dyn AiProvider,
    patchset_id: i64,
    cover_letter: Option<&str>,
    diffs: &[&str],
) -> Result<()> {
    let user_content = build_diff_context(cover_letter, diffs);

    let request = AiRequest {
        system: Some(SUMMARY_SYSTEM_PROMPT.to_string()),
        messages: vec![AiMessage {
            role: AiRole::User,
            content: Some(user_content),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        temperature: Some(0.3),
        response_format: Some(AiResponseFormat::Text),
        context_tag: Some(format!("[ps:{} summary]", patchset_id)),
    };

    let response = provider.generate_content(request).await.context(format!(
        "Failed to generate summary for patchset {}",
        patchset_id
    ))?;

    if let Some(text) = response.content {
        let summary = text.trim().to_string();
        if !summary.is_empty() {
            db.set_patchset_summary(patchset_id, &summary).await?;
            info!(
                "Generated summary for patchset {} ({} chars)",
                patchset_id,
                summary.len()
            );
            return Ok(());
        }
    }

    anyhow::bail!("Patchset summary generation returned empty content")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_truncate_ascii() {
        assert_eq!(safe_truncate("hello", 3), "hel");
        assert_eq!(safe_truncate("hello", 10), "hello");
        assert_eq!(safe_truncate("hello", 5), "hello");
    }

    #[test]
    fn test_safe_truncate_multibyte() {
        // 'é' is 2 bytes (0xC3 0xA9)
        let s = "café";
        assert_eq!(s.len(), 5); // c(1) a(1) f(1) é(2)
        assert_eq!(safe_truncate(s, 5), "café");
        assert_eq!(safe_truncate(s, 4), "caf");
        assert_eq!(safe_truncate(s, 3), "caf");

        // CJK characters are 3 bytes each
        let s = "你好世界";
        assert_eq!(s.len(), 12);
        assert_eq!(safe_truncate(s, 6), "你好");
        assert_eq!(safe_truncate(s, 5), "你");
        assert_eq!(safe_truncate(s, 4), "你");
        assert_eq!(safe_truncate(s, 3), "你");
        assert_eq!(safe_truncate(s, 2), "");
        assert_eq!(safe_truncate(s, 0), "");
    }

    #[test]
    fn test_build_diff_context_with_cover_letter() {
        let ctx = build_diff_context(Some("My cover letter"), &["diff1", "diff2"]);
        assert!(ctx.contains("COVER LETTER:\nMy cover letter"));
        assert!(ctx.contains("--- PATCH 1 ---"));
        assert!(ctx.contains("--- PATCH 2 ---"));
        assert!(ctx.contains("diff1"));
        assert!(ctx.contains("diff2"));
    }

    #[test]
    fn test_build_diff_context_without_cover_letter() {
        let ctx = build_diff_context(None, &["only-diff"]);
        assert!(!ctx.contains("COVER LETTER"));
        assert!(ctx.contains("--- PATCH 1 ---"));
        assert!(ctx.contains("only-diff"));
    }

    #[test]
    fn test_build_diff_context_truncation() {
        let long_diff = "x".repeat(5000);
        let ctx = build_diff_context(None, &[&long_diff]);
        assert!(ctx.contains("... (truncated)"));
        assert!(!ctx.contains(&"x".repeat(5000)));
    }
}
