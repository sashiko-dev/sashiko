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

//! Standalone Pre-existing Bug Pipeline.
//!
//! Processes candidate pre-existing bugs individually through:
//! 1. Dedicated single-issue verification and High/Critical severity calibration.
//! 2. Subsystem & file-localized fast vector candidate retrieval (Top N = 20).
//! 3. LLM deduplication confirmation against known pre-existing bugs.
//! 4. Standalone LKML-style inline review generation for newly discovered bugs.
//! 5. Database persistence and review linking.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, info};

use crate::ai::session::{LlmSession, SessionRunner, ValidationError};
use crate::ai::vector_search::{
    DEFAULT_SIMILARITY_THRESHOLD, DEFAULT_TOP_CANDIDATES, extract_bug_vector, find_top_candidates,
};
use crate::ai::{AiProvider, AiResponse, AiResponseFormat, AiTool};
use crate::db::{Database, NewPreexistingBug, PreexistingBug, Severity};
use crate::toolbox::ToolBox;

/// Input payload representing a candidate pre-existing defect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreexistingBugInput {
    pub problem: String,
    pub reasoning: String,
    pub locations: Option<Value>,
    pub subsystem: Option<String>,
    pub source_files: Vec<String>,
    pub commit_sha: Option<String>,
    pub patchset_id: Option<i64>,
    pub patch_id: Option<i64>,
    pub baseline_sha: Option<String>,
}

/// The result of processing a candidate pre-existing bug through the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PreexistingBugOutcome {
    /// The candidate bug was discarded (invalid, false positive, or Low/Medium severity).
    Discarded {
        reason: String,
        logs: Option<String>,
    },
    /// The bug was confirmed as an identical duplicate of a known pre-existing bug.
    Duplicate {
        existing_bug: PreexistingBug,
        reasoning: String,
        logs: Option<String>,
    },
    /// The bug was confirmed as a newly discovered pre-existing bug.
    NewlyDiscovered { bug: PreexistingBug },
}

// ---------------------------------------------------------------------------
// 1. Verification Session
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct VerificationJson {
    valid: bool,
    severity: String,
    severity_explanation: Option<String>,
    verified_locations: Option<Value>,
    discard_reason: Option<String>,
}

struct PreexistingVerifySession<'a> {
    input: &'a PreexistingBugInput,
    tools: Option<Arc<ToolBox>>,
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for PreexistingVerifySession<'_> {
    type Output = VerificationJson;

    fn system_prompt(&self) -> String {
        let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
        format!(
            "Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.\n\n\
            You are an expert Linux kernel maintainer. Your task is to rigorously verify a candidate pre-existing vulnerability reported in the Linux kernel codebase.\n\
            Use available tools to inspect the code, verify call chains, and confirm whether this defect truly exists in the tree.\n\
            CRITICAL SEVERITY FILTER: We ONLY report High and Critical pre-existing issues. If the issue is valid but only Low or Medium severity, or if it is speculative / unverifiable, mark valid=false with discard_reason explaining the low severity or speculativeness."
        )
    }

    fn initial_user_prompt(&self) -> String {
        let loc_str = self
            .input
            .locations
            .as_ref()
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_else(|| "[]".to_string());

        format!(
            "Candidate Pre-existing Vulnerability:\n\
            Problem: {}\n\
            Reasoning: {}\n\
            Locations:\n{}\n\n\
            Task:\n\
            1. Verify the problem and check the actual codebase using tools.\n\
            2. Determine if the issue is a genuine, reproducible High or Critical severity defect in the codebase.\n\
            3. If it is invalid, a false positive, or only Low/Medium severity, set \"valid\": false and provide \"discard_reason\".\n\
            4. If it is a confirmed High or Critical severity bug, set \"valid\": true, assign \"severity\": \"High\" or \"Critical\", and detail your proof in \"severity_explanation\".\n\n\
            Return ONLY a valid JSON object matching this schema:\n\
            {{\n\
              \"valid\": true,\n\
              \"severity\": \"High\",\n\
              \"severity_explanation\": \"1. ... 2. ...\",\n\
              \"verified_locations\": [ {{\"file\": \"...\", \"function_or_symbol\": \"...\", \"line\": 123}} ],\n\
              \"discard_reason\": null\n\
            }}",
            self.input.problem, self.input.reasoning, loc_str
        )
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        self.tools.as_ref().map(|t| t.get_declarations_generic())
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        if let Some(ref tools) = self.tools {
            tools.call(name, args).await
        } else {
            bail!("Tool execution requested but no toolbox available");
        }
    }

    fn response_format(&self) -> Option<AiResponseFormat> {
        Some(AiResponseFormat::Json { schema: None })
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        let parsed: VerificationJson = crate::workflow::output::parse_json_from_text(text)
            .map_err(ValidationError::FormatViolation)?;

        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// 2. Deduplication Confirmation Session
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct DedupJson {
    is_duplicate: bool,
    duplicate_of_id: Option<i64>,
    reasoning: String,
}

struct PreexistingDedupSession<'a> {
    candidate_problem: &'a str,
    candidate_locations: Option<&'a Value>,
    candidate_subsystem: Option<&'a str>,
    known_candidates: &'a [PreexistingBug],
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for PreexistingDedupSession<'_> {
    type Output = DedupJson;

    fn system_prompt(&self) -> String {
        "You are an expert Linux kernel maintainer responsible for defect tracking and deduplication.\n\
        You will compare a newly verified pre-existing bug against a list of known pre-existing bugs in the codebase.\n\
        Determine if the newly verified bug is an identical duplicate (describing the same root cause in the same code path/function) of one of the candidate bugs.\n\
        Output raw JSON only."
            .to_string()
    }

    fn initial_user_prompt(&self) -> String {
        let loc_str = self
            .candidate_locations
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_else(|| "[]".to_string());

        let mut known_list = String::new();
        for bug in self.known_candidates {
            let files_str = bug
                .source_files
                .as_ref()
                .map(|f| f.join(", "))
                .unwrap_or_default();
            known_list.push_str(&format!(
                "- Bug ID {}: [Slug: {}] [Severity: {}] [Subsystem: {}]\n  Problem: {}\n  Affected Files: {}\n\n",
                bug.id,
                bug.slug,
                bug.severity.as_str(),
                bug.subsystem.as_deref().unwrap_or("unknown"),
                bug.problem,
                files_str
            ));
        }

        format!(
            "Newly Verified Pre-existing Bug:\n\
            Problem: {}\n\
            Subsystem: {}\n\
            Locations:\n{}\n\n\
            Candidate Known Pre-existing Bugs in Database:\n\
            {}\n\
            Task:\n\
            Determine if the newly verified bug is an identical duplicate of ANY of the candidate bugs listed above.\n\
            - If it matches a candidate bug, set \"is_duplicate\": true, set \"duplicate_of_id\": <ID of matched bug>, and explain in \"reasoning\".\n\
            - If it is a distinct or newly discovered issue, set \"is_duplicate\": false, \"duplicate_of_id\": null, and explain in \"reasoning\".\n\n\
            Return ONLY a valid JSON object matching:\n\
            {{\n\
              \"is_duplicate\": true,\n\
              \"duplicate_of_id\": 12,\n\
              \"reasoning\": \"Both describe the same missing unlock in foo_cleanup()\"\n\
            }}",
            self.candidate_problem,
            self.candidate_subsystem.unwrap_or("unknown"),
            loc_str,
            known_list
        )
    }

    fn response_format(&self) -> Option<AiResponseFormat> {
        Some(AiResponseFormat::Json { schema: None })
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        let parsed: DedupJson = crate::workflow::output::parse_json_from_text(text)
            .map_err(ValidationError::FormatViolation)?;

        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// 3. Standalone Inline Review Generation Session
// ---------------------------------------------------------------------------

struct PreexistingReportSession<'a> {
    problem: &'a str,
    severity: &'a str,
    severity_explanation: &'a str,
    locations: Option<&'a Value>,
    tools: Option<Arc<ToolBox>>,
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for PreexistingReportSession<'_> {
    type Output = String;

    fn system_prompt(&self) -> String {
        "You are an automated review bot generating a dedicated, standalone defect report for the Linux Kernel Mailing List (LKML).\n\
        Generate a polite, professional, plain-text inline review report for a pre-existing vulnerability discovered in the codebase.\n\
        CRITICAL RULES:\n\
        1. Follow standard LKML plain-text review style. Do NOT use markdown headers or ALL CAPS shouting.\n\
        2. Do NOT use markdown code fences ('```'). Use '> ' to quote code snippets.\n\
        3. Do NOT use backticks to quote code or symbol names.\n\
        4. State clearly at the beginning: 'This is a pre-existing issue in the codebase.'\n\
        5. Explain the execution trigger, cite exact function and line numbers when known, and suggest a concrete fix."
            .to_string()
    }

    fn initial_user_prompt(&self) -> String {
        let loc_str = self
            .locations
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_else(|| "[]".to_string());

        format!(
            "Pre-existing Vulnerability Details:\n\
            Problem: {}\n\
            Severity: {}\n\
            Severity Explanation: {}\n\
            Locations:\n{}\n\n\
            Task:\n\
            Generate a complete, standalone LKML-style review comment block for this issue.\n\
            Quote the problematic source lines with '> ' and provide interspersed explanations and remediation suggestions.\n\
            Return raw plain text, not JSON or markdown fences.",
            self.problem, self.severity, self.severity_explanation, loc_str
        )
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        self.tools.as_ref().map(|t| t.get_declarations_generic())
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        if let Some(ref tools) = self.tools {
            tools.call(name, args).await
        } else {
            bail!("Tool execution requested but no toolbox available");
        }
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("").trim();
        if text.is_empty() {
            return Err(ValidationError::FormatViolation("Output was empty".into()));
        }
        if text.lines().any(|l| l.trim_start().starts_with("```")) {
            return Err(ValidationError::FormatViolation(
                "Output contains markdown code blocks ('```'). Must be plain text.".into(),
            ));
        }
        if !text.lines().any(|l| l.trim_start().starts_with('>')) {
            return Err(ValidationError::FormatViolation(
                "Output must quote code using '> ' context.".into(),
            ));
        }

        Ok(text.to_string())
    }
}

// ---------------------------------------------------------------------------
// 4. Pipeline Driver
// ---------------------------------------------------------------------------

/// Generates a random unique slug for a newly discovered pre-existing bug (e.g. `pb-a1b2c3d4`).
pub fn generate_preexisting_slug() -> String {
    let mut rng = fastrand::Rng::new();
    let mut hex = String::with_capacity(8);
    for _ in 0..8 {
        let b = rng.u8(..16);
        hex.push_str(&format!("{:x}", b));
    }
    format!("pb-{}", hex)
}

/// Executes the standalone pre-existing bug pipeline for a single candidate concern.
pub async fn process_preexisting_issue(
    provider: &dyn AiProvider,
    tools: Option<Arc<ToolBox>>,
    db: &Database,
    input: PreexistingBugInput,
    context_tag: Option<&str>,
) -> Result<PreexistingBugOutcome> {
    info!(
        "Processing candidate pre-existing issue: '{}' in subsystem '{:?}'",
        input.problem, input.subsystem
    );

    let mut full_history = Vec::new();
    let runner = SessionRunner::new(provider);

    // Step 1: Subsystem & File-Aware Fast Vector Space Candidate Retrieval (Top N = 20)
    let query_vector = extract_bug_vector(
        &input.problem,
        input.subsystem.as_deref(),
        &input.source_files,
        input.locations.as_ref(),
    );

    let known_bugs = db.list_all_preexisting_bugs_for_vector_search().await?;
    let candidate_matches = find_top_candidates(
        &query_vector,
        &known_bugs,
        DEFAULT_TOP_CANDIDATES,
        DEFAULT_SIMILARITY_THRESHOLD,
    );

    debug!(
        "Found {} potential vector candidate matches for pre-existing issue",
        candidate_matches.len()
    );

    // Step 2: LLM Deduplication Confirmation (Short-circuit if duplicate of existing bug)
    if !candidate_matches.is_empty() {
        let candidate_bugs: Vec<PreexistingBug> =
            candidate_matches.iter().map(|m| m.bug.clone()).collect();

        let mut dedup_session = PreexistingDedupSession {
            candidate_problem: &input.problem,
            candidate_locations: input.locations.as_ref(),
            candidate_subsystem: input.subsystem.as_deref(),
            known_candidates: &candidate_bugs,
            context_tag: context_tag.map(|s| s.to_string()),
        };

        let dedup_result = runner.run(&mut dedup_session).await?;
        full_history.extend(dedup_result.history);
        let dedup = dedup_result.output;
        let duplicate_match = if dedup.is_duplicate {
            dedup
                .duplicate_of_id
                .and_then(|dup_id| candidate_bugs.iter().find(|b| b.id == dup_id))
        } else {
            None
        };

        if let Some(existing) = duplicate_match {
            info!(
                "Matched duplicate pre-existing bug #{} ({}) - skipping tool verification",
                existing.id, existing.slug
            );
            let logs = serde_json::to_string(&full_history).ok();
            return Ok(PreexistingBugOutcome::Duplicate {
                existing_bug: existing.clone(),
                reasoning: dedup.reasoning,
                logs,
            });
        }
    }

    // Step 3: Issue Verification & Severity Calibration (only for novel candidates)
    let mut verify_session = PreexistingVerifySession {
        input: &input,
        tools: tools.clone(),
        context_tag: context_tag.map(|s| s.to_string()),
    };

    let verify_result = runner.run(&mut verify_session).await?;
    full_history.extend(verify_result.history);
    let verification = verify_result.output;

    if !verification.valid {
        let reason = verification
            .discard_reason
            .unwrap_or_else(|| "Discarded as invalid or below High severity threshold".to_string());
        info!("Pre-existing candidate discarded: {}", reason);
        let logs = serde_json::to_string(&full_history).ok();
        return Ok(PreexistingBugOutcome::Discarded { reason, logs });
    }

    let severity = Severity::from_str(&verification.severity);
    if severity < Severity::High {
        let reason = format!(
            "Discarded: calibrated severity is {} (minimum threshold is High)",
            severity.as_str()
        );
        info!("{}", reason);
        let logs = serde_json::to_string(&full_history).ok();
        return Ok(PreexistingBugOutcome::Discarded { reason, logs });
    }

    let final_locations = verification
        .verified_locations
        .or_else(|| input.locations.clone());
    let severity_explanation = verification.severity_explanation;

    // Step 4: Standalone Inline Review Generation
    let mut report_session = PreexistingReportSession {
        problem: &input.problem,
        severity: severity.as_str(),
        severity_explanation: severity_explanation.as_deref().unwrap_or(""),
        locations: final_locations.as_ref(),
        tools,
        context_tag: context_tag.map(|s| s.to_string()),
    };

    let report_result = runner.run(&mut report_session).await?;
    full_history.extend(report_result.history);
    let inline_review = report_result.output;

    let logs_json = serde_json::to_string(&full_history).ok();

    // Step 5: Persist in Database
    let slug = generate_preexisting_slug();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    let source_files_opt = if input.source_files.is_empty() {
        None
    } else {
        Some(input.source_files.clone())
    };

    let new_bug = NewPreexistingBug {
        slug: slug.clone(),
        problem: input.problem.clone(),
        severity,
        severity_explanation,
        locations: final_locations,
        subsystem: input.subsystem.clone(),
        source_files: source_files_opt,
        inline_review,
        logs: logs_json,
        vector_json: Some(query_vector.to_json()),
        discovered_in_patchset_id: input.patchset_id,
        discovered_in_patch_id: input.patch_id,
        discovered_in_commit: input.commit_sha.clone(),
        created_at: now,
    };

    let bug_id = db.create_preexisting_bug(&new_bug).await?;
    let saved_bug = db
        .get_preexisting_bug(bug_id)
        .await?
        .expect("Saved bug must exist");

    info!(
        "Successfully registered newly discovered pre-existing bug #{} ({})",
        bug_id, slug
    );

    Ok(PreexistingBugOutcome::NewlyDiscovered { bug: saved_bug })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiRequest, ProviderCapabilities};
    use serde_json::json;

    struct MockAiProvider {
        response_text: String,
    }

    #[async_trait]
    impl AiProvider for MockAiProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            Ok(AiResponse {
                content: Some(self.response_text.clone()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            100
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 8192,
            }
        }
    }

    #[tokio::test]
    async fn test_verify_session_valid() {
        let input = PreexistingBugInput {
            problem: "Memory leak in net/core/dev.c".to_string(),
            reasoning: "Allocated buffer not freed on error path".to_string(),
            locations: Some(json!([{"file": "net/core/dev.c", "line": 100}])),
            subsystem: Some("net".to_string()),
            source_files: vec!["net/core/dev.c".to_string()],
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
        };

        let mock_provider = MockAiProvider {
            response_text: json!({
                "valid": true,
                "severity": "High",
                "severity_explanation": "1. Buffer allocated. 2. Not freed before return.",
                "verified_locations": [{"file": "net/core/dev.c", "line": 100}],
                "discard_reason": null
            })
            .to_string(),
        };

        let mut session = PreexistingVerifySession {
            input: &input,
            tools: None,
            context_tag: None,
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert!(res.output.valid);
        assert_eq!(res.output.severity, "High");
    }

    #[tokio::test]
    async fn test_dedup_session_duplicate() {
        let mock_provider = MockAiProvider {
            response_text: json!({
                "is_duplicate": true,
                "duplicate_of_id": 42,
                "reasoning": "Identical leak in net/core/dev.c"
            })
            .to_string(),
        };

        let known_bugs = vec![PreexistingBug {
            id: 42,
            slug: "pb-42".to_string(),
            problem: "Memory leak in dev.c".to_string(),
            severity: Severity::High,
            severity_explanation: None,
            locations: None,
            subsystem: Some("net".to_string()),
            source_files: None,
            inline_review: "".to_string(),
            logs: None,
            vector_json: None,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            created_at: 100,
        }];

        let mut session = PreexistingDedupSession {
            candidate_problem: "Memory leak in net/core/dev.c",
            candidate_locations: None,
            candidate_subsystem: Some("net"),
            known_candidates: &known_bugs,
            context_tag: None,
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert!(res.output.is_duplicate);
        assert_eq!(res.output.duplicate_of_id, Some(42));
    }

    struct QueuedMockAiProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl QueuedMockAiProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl AiProvider for QueuedMockAiProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let next_resp = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "{}".to_string());
            Ok(AiResponse {
                content: Some(next_resp),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            100
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 8192,
            }
        }
    }

    #[tokio::test]
    async fn test_report_session() {
        let mock_provider = MockAiProvider {
            response_text: "This is a pre-existing issue in the codebase.\n\n> int *ptr = alloc();\n> if (!ptr)\n>     return -ENOMEM;\n\nThe buffer is not freed.\n".to_string(),
        };

        let mut session = PreexistingReportSession {
            problem: "Memory leak in net/core/dev.c",
            severity: "High",
            severity_explanation: "Missing free",
            locations: None,
            tools: None,
            context_tag: None,
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert!(res.output.contains("pre-existing issue"));
        assert!(res.output.contains("> int *ptr"));
    }

    #[tokio::test]
    async fn test_process_preexisting_issue_flow() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let thread_id = db.create_thread("t1", "subj", 100).await.unwrap();
        let ps_id = db
            .create_patchset(
                thread_id, None, "m1", "subj", "auth", 100, 1, 0, "", "", None, 1, None, false,
                None, None,
            )
            .await
            .unwrap()
            .unwrap();

        let verify_json = json!({
            "valid": true,
            "severity": "Critical",
            "severity_explanation": "1. Buffer overflow occurs when size exceeds MTU.",
            "verified_locations": [{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}],
            "discard_reason": null
        }).to_string();

        let report_text = "This is a pre-existing issue in the codebase.\n\n> memcpy(skb->data, buf, size);\n\nPotential buffer overflow when size > MTU.\n".to_string();

        // 1st call: Verification, 2nd call: Standalone Report (no existing candidates in DB so dedup is skipped)
        let provider = QueuedMockAiProvider::new(vec![verify_json, report_text]);

        let input = PreexistingBugInput {
            problem: "Buffer overflow in e1000 rx handler".to_string(),
            reasoning: "Size not checked against MTU".to_string(),
            locations: Some(
                json!([{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]),
            ),
            subsystem: Some("net:intel".to_string()),
            source_files: vec!["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            commit_sha: Some("abcdef123456".to_string()),
            patchset_id: Some(ps_id),
            patch_id: None,
            baseline_sha: None,
        };

        let outcome = process_preexisting_issue(&provider, None, &db, input, None)
            .await
            .unwrap();

        match outcome {
            PreexistingBugOutcome::NewlyDiscovered { bug } => {
                assert_eq!(bug.problem, "Buffer overflow in e1000 rx handler");
                assert_eq!(bug.severity, Severity::Critical);
                assert!(bug.slug.starts_with("pb-"));
                assert_eq!(bug.discovered_in_patchset_id, Some(ps_id));
                assert!(bug.logs.is_some(), "Logs must be populated");
                let logs_str = bug.logs.unwrap();
                assert!(logs_str.contains("pre-existing"));
            }
            _ => panic!("Expected NewlyDiscovered outcome, got {:?}", outcome),
        }
    }

    #[tokio::test]
    async fn test_process_preexisting_issue_dedup_first_short_circuits_verification() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        // Seed an existing bug into the database
        let existing_vector = extract_bug_vector(
            "Buffer overflow in e1000 rx handler",
            Some("net:intel"),
            &["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            None,
        );

        let existing_id = db
            .create_preexisting_bug(&NewPreexistingBug {
                slug: "pb-existing1".to_string(),
                problem: "Buffer overflow in e1000 rx handler".to_string(),
                severity: Severity::High,
                severity_explanation: Some("Known buffer overflow".to_string()),
                locations: None,
                subsystem: Some("net:intel".to_string()),
                source_files: Some(vec![
                    "drivers/net/ethernet/intel/e1000/e1000_main.c".to_string(),
                ]),
                inline_review: "> code\nInline review text".to_string(),
                logs: None,
                vector_json: Some(existing_vector.to_json()),
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                created_at: 1000,
            })
            .await
            .unwrap();

        // 1st call: Dedup returns is_duplicate: true. Verification and report generation are skipped!
        let dedup_json = json!({
            "is_duplicate": true,
            "duplicate_of_id": existing_id,
            "reasoning": "Exact match with known bug #1 in e1000 driver"
        })
        .to_string();

        let provider = QueuedMockAiProvider::new(vec![dedup_json]);

        let input = PreexistingBugInput {
            problem: "Buffer overflow in e1000 rx handler".to_string(),
            reasoning: "Size not checked against MTU".to_string(),
            locations: Some(
                json!([{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]),
            ),
            subsystem: Some("net:intel".to_string()),
            source_files: vec!["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            commit_sha: Some("abcdef123456".to_string()),
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
        };

        let outcome = process_preexisting_issue(&provider, None, &db, input, None)
            .await
            .unwrap();

        match outcome {
            PreexistingBugOutcome::Duplicate {
                existing_bug,
                reasoning,
                logs,
            } => {
                assert_eq!(existing_bug.id, existing_id);
                assert_eq!(existing_bug.slug, "pb-existing1");
                assert_eq!(reasoning, "Exact match with known bug #1 in e1000 driver");
                assert!(logs.is_some());
            }
            _ => panic!("Expected Duplicate outcome, got {:?}", outcome),
        }
    }
}
