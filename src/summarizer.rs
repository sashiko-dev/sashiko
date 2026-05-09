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
use tracing::{info, warn};

const MAX_DIFF_CHARS_PER_PATCH: usize = 4000;

pub async fn generate_series_map(
    provider: &dyn AiProvider,
    cover_letter: Option<&str>,
    diffs: &[&str],
) -> Result<SeriesMap> {
    let mut system_prompt = "You are an expert system mapping cross-patch dependencies in a Linux kernel patch series.\n".to_string();
    system_prompt.push_str("Your task is to analyze the series and output a strict JSON object mapping all introduced symbols.\n");
    system_prompt.push_str("You must return ONLY the raw JSON object. Do NOT wrap it in markdown code blocks like ```json\n");
    system_prompt.push_str("Schema:\n");
    system_prompt.push_str("{\n");
    system_prompt.push_str("  \"introduced_symbols\": [\n");
    system_prompt.push_str("    {\n");
    system_prompt.push_str("      \"name\": \"string\",\n");
    system_prompt.push_str("      \"defined_in_patch_index\": number (1-based index),\n");
    system_prompt.push_str("      \"completed_in_patch_indices\": [number],\n");
    system_prompt.push_str("      \"description\": \"string\"\n");
    system_prompt.push_str("    }\n");
    system_prompt.push_str("  ]\n");
    system_prompt.push_str("}\n");

    let mut user_content = String::with_capacity(8192);
    if let Some(cl) = cover_letter {
        user_content.push_str(&format!("COVER LETTER:\n{}\n\n", cl));
    }

    for (i, diff) in diffs.iter().enumerate() {
        user_content.push_str(&format!("--- PATCH {} ---\n", i + 1));
        if diff.len() > MAX_DIFF_CHARS_PER_PATCH {
            user_content.push_str(&diff[..MAX_DIFF_CHARS_PER_PATCH]);
            user_content.push_str("\n... (truncated)\n");
        } else {
            user_content.push_str(diff);
        }
        user_content.push_str("\n\n");
    }

    let request = AiRequest {
        system: Some(system_prompt),
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
) -> Result<String> {
    let mut system_prompt =
        "You are an expert technical writer summarizing a Linux kernel patch series.\n".to_string();
    system_prompt.push_str("Provide a concise, high-level summary of what the series achieves, why it is needed, and any notable design choices.\n");
    system_prompt.push_str("Format as a single paragraph or bullet points. Do not mention patch numbers explicitly unless necessary.\n");

    let mut user_content = String::with_capacity(8192);
    if let Some(cl) = cover_letter {
        user_content.push_str(&format!("COVER LETTER:\n{}\n\n", cl));
    }

    for (i, diff) in diffs.iter().enumerate() {
        user_content.push_str(&format!("--- PATCH {} ---\n", i + 1));
        if diff.len() > MAX_DIFF_CHARS_PER_PATCH {
            user_content.push_str(&diff[..MAX_DIFF_CHARS_PER_PATCH]);
            user_content.push_str("\n... (truncated)\n");
        } else {
            user_content.push_str(diff);
        }
        user_content.push_str("\n\n");
    }

    let request = AiRequest {
        system: Some(system_prompt),
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

    match provider.generate_content(request).await {
        Ok(response) => {
            if let Some(text) = response.content {
                let summary = text.trim().to_string();
                if !summary.is_empty() {
                    db.set_patchset_summary(patchset_id, &summary).await?;
                    info!(
                        "Generated summary for patchset {} ({} chars)",
                        patchset_id,
                        summary.len()
                    );
                    return Ok(summary);
                }
            }
        }
        Err(e) => {
            warn!(
                "Failed to generate summary for patchset {}: {}",
                patchset_id, e
            );
        }
    }

    Ok("No summary generated.".to_string())
}
