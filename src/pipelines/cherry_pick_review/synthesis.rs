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

//! Verbatim-ported synthesis-stage prompt builders for the cherry-pick pipeline.
//!
//! Synthesis stages 8-11 (+ origin classification) each build a bespoke user
//! prompt that injects the accumulated concerns/findings and carries a per-stage
//! output schema. These templates are copied VERBATIM from the original hack
//! (commit 66fb0a5) so the pipeline reproduces its behaviour exactly.
//!
//! The `clean` prompt is set equal to the user prompt: it only affects logging
//! (synthesis stages use a fixed system context, not accumulated clean history),
//! so this does not change model behaviour.

use crate::pipelines::{PipelineState, StagePromptBuilder};

/// Concerns/dismissed-concerns JSON schema guidance appended to analysis stages
/// 1-7 (copied verbatim from `Worker::execute_stage`).
pub const ANALYSIS_FORMAT_GUIDANCE: &str = include_str!("prompts/analysis_format_guidance.md");

fn concerns_json(state: &PipelineState) -> String {
    serde_json::to_string_pretty(&state.concerns).unwrap_or_default()
}

fn dismissed_json(state: &PipelineState) -> String {
    serde_json::to_string_pretty(&state.dismissed_concerns).unwrap_or_default()
}

fn findings_json(state: &PipelineState) -> String {
    match &state.findings {
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
        None => "[]".to_string(),
    }
}

pub fn stage8_builder() -> StagePromptBuilder {
    Box::new(|stage_prompt: &str, state: &PipelineState| {
        let aggregated_concerns_json = concerns_json(state);
        let aggregated_dismissed_concerns_json = dismissed_json(state);
        let user_prompt = format!(
            r#"{}

Consolidated Concerns:
{}

Consolidated Dismissed Concerns:
{}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Preserve the most precise location details from the input. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ],
  "dismissed_concerns": [
    {{
      "type": "Resource Management",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 125,
          "code_snippet": "safe_code_path();",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }}
      ]
    }}
  ]
}}
```"#,
            stage_prompt, aggregated_concerns_json, aggregated_dismissed_concerns_json
        );
        (user_prompt.clone(), user_prompt)
    })
}

pub fn stage9_builder() -> StagePromptBuilder {
    Box::new(|stage_prompt: &str, state: &PipelineState| {
        let deduplicated_concerns_json = concerns_json(state);
        let deduplicated_dismissed_concerns_json = dismissed_json(state);
        let user_prompt = format!(
            r#"{}

Consolidated Concerns:
{}

Consolidated Dismissed Concerns:
{}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Preserve the most precise locations from the retained concerns. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#,
            stage_prompt, deduplicated_concerns_json, deduplicated_dismissed_concerns_json
        );
        (user_prompt.clone(), user_prompt)
    })
}

pub fn stage10_builder() -> StagePromptBuilder {
    Box::new(|stage_prompt: &str, state: &PipelineState| {
        let full_series_context = "Not applicable (single patch or last patch in series).";
        let conflict_resolved_concerns_json = concerns_json(state);
        let user_prompt = format!(
            "{}\n\nCRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.\n\nFull Series Context:\n{}\n\nConsolidated Concerns:\n{}\n\nReturn ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: \"problem\" (a string containing the vulnerability description), \"severity\" (a string: Low, Medium, High, or Critical), \"severity_explanation\" (a string detailing the reasoning and proof), \"preexisting\" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), \"locations\" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters). Carry forward the locations from the validated concern; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.\n\nExample Output:\n```json\n{{\n  \"findings\": [\n    {{\n      \"problem\": \"Memory leak in function X when condition Y is met.\",\n      \"severity\": \"High\",\n      \"severity_explanation\": \"1. Condition Y is met.\\\n2. The buffer is allocated but not freed before return.\",\n      \"preexisting\": false,\n      \"locations\": [\n        {{\n          \"file\": \"path/to/file.c\",\n          \"function_or_symbol\": \"function_name\",\n          \"line\": 123,\n          \"code_snippet\": \"problematic_code();\",\n          \"why_this_location_matters\": \"This is where the newly allocated resource is dropped on the error path.\"\n        }}\n      ]\n    }}\n  ]\n}}\n```",
            stage_prompt, full_series_context, conflict_resolved_concerns_json
        );
        (user_prompt.clone(), user_prompt)
    })
}

pub fn origin_builder() -> StagePromptBuilder {
    Box::new(|classification_prompt: &str, state: &PipelineState| {
        let findings_str = findings_json(state);
        let user_prompt = format!(
            "{}\n\nFindings to classify:\n{}\n\nReturn ONLY a JSON object with a 'findings' array. Each finding must have all original fields plus an 'origin' field.",
            classification_prompt, findings_str
        );
        (user_prompt.clone(), user_prompt)
    })
}

pub fn stage11_builder() -> StagePromptBuilder {
    Box::new(|stage_prompt: &str, state: &PipelineState| {
        let findings_str = findings_json(state);
        let user_prompt = format!(
            "{}\n\nFindings:\n{}\n\nReturn raw text output, not JSON.",
            stage_prompt, findings_str
        );
        (user_prompt.clone(), user_prompt)
    })
}
