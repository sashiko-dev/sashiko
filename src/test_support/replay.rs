// Copyright 2026 The Sashiko Authors
// Licensed under the Apache License, Version 2.0

//! A mock [`AiProvider`] that replays pre-recorded responses keyed by stage
//! identifier.  Used by the cherry-pick pipeline equivalence tests.

use crate::ai::{AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ── recorded / canned types ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub stage_id: String,
    pub user_prompt: String,
}

#[derive(Debug, Clone, Default)]
pub struct CannedResponse {
    pub stage_id: String,
    pub content: String,
    /// If set, generate_content returns Err instead of Ok.
    pub error: Option<String>,
    /// If true, AiResponse.truncated = true.
    pub truncated: bool,
    /// If set, response includes tool calls (content is ignored).
    pub tool_calls: Option<Vec<crate::ai::ToolCall>>,
}

impl CannedResponse {
    /// Simple text response (backward-compatible).
    pub fn ok(stage_id: &str, content: &str) -> Self {
        Self {
            stage_id: stage_id.into(),
            content: content.into(),
            error: None,
            truncated: false,
            tool_calls: None,
        }
    }

    /// Error response: generate_content returns Err.
    pub fn err(stage_id: &str, error: &str) -> Self {
        Self {
            stage_id: stage_id.into(),
            content: String::new(),
            error: Some(error.into()),
            truncated: false,
            tool_calls: None,
        }
    }

    /// Truncated response: AiResponse.truncated = true.
    pub fn truncated_resp(stage_id: &str, content: &str) -> Self {
        Self {
            stage_id: stage_id.into(),
            content: content.into(),
            error: None,
            truncated: true,
            tool_calls: None,
        }
    }

    /// Response with tool calls instead of text content.
    pub fn with_tools(stage_id: &str, calls: Vec<crate::ai::ToolCall>) -> Self {
        Self {
            stage_id: stage_id.into(),
            content: String::new(),
            error: None,
            truncated: false,
            tool_calls: Some(calls),
        }
    }
}

// ── ReplayProvider ──────────────────────────────────────────────────────

pub struct ReplayProvider {
    pub responses: Mutex<VecDeque<CannedResponse>>,
    pub calls: Mutex<Vec<RecordedCall>>,
    call_count: AtomicUsize,
}

impl ReplayProvider {
    pub fn new(responses: Vec<CannedResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            calls: Mutex::new(Vec::new()),
            call_count: AtomicUsize::new(0),
        }
    }

    pub fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
}

/// Detect which stage is being called by inspecting the user prompt.
pub fn detect_stage(request: &AiRequest) -> String {
    let combined: String = request
        .messages
        .iter()
        .filter(|m| m.role == crate::ai::AiRole::User)
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n");

    // Most specific first
    if combined.contains("Origin Classification") {
        return "origin_classification".into();
    }
    if combined.contains("Conflict review report") {
        return "conflict_report".into();
    }
    if combined.contains("selected_prompts") && combined.contains("subsystem") {
        return "phase0_prescreen".into();
    }
    if combined.contains("relevant_stages") {
        return "planning".into();
    }
    // "# Stage N." pattern
    for line in combined.lines() {
        let t = line.trim_start_matches('#').trim();
        if let Some(rest) = t.strip_prefix("Stage ")
            && let Some(dot) = rest.find('.')
            && let Ok(n) = rest[..dot].trim().parse::<u8>()
        {
            return format!("stage_{n}");
        }
    }
    // Keyword fallback
    if combined.contains("Deduplication") && combined.contains("Consolidation") {
        return "stage_8".into();
    }
    if combined.contains("conflict resolution") && combined.contains("dismissed") {
        return "stage_9".into();
    }
    if combined.contains("Verification") && combined.contains("severity") {
        return "stage_10".into();
    }
    "unknown".into()
}

fn extract_user_prompt(request: &AiRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| m.role == crate::ai::AiRole::User)
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl AiProvider for ReplayProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let call_num = self.call_count.fetch_add(1, Ordering::SeqCst);
        let stage_id = format!("call_{}", call_num);
        let user_prompt = extract_user_prompt(&request);

        self.calls.lock().unwrap().push(RecordedCall {
            stage_id: stage_id.clone(),
            user_prompt,
        });

        let response = self.responses.lock().unwrap().pop_front();

        match response {
            Some(canned) => {
                // Error simulation
                if let Some(err_msg) = canned.error {
                    return Err(anyhow!(err_msg));
                }
                Ok(AiResponse {
                    content: if canned.tool_calls.is_some() {
                        None
                    } else {
                        Some(canned.content)
                    },
                    thought: None,
                    thought_signature: None,
                    tool_calls: canned.tool_calls,
                    usage: Some(AiUsage {
                        prompt_tokens: 100,
                        completion_tokens: 50,
                        total_tokens: 150,
                        cached_tokens: Some(0),
                    }),
                    truncated: canned.truncated,
                })
            }
            None => Err(anyhow!(
                "ReplayProvider: no canned response left (stage: {stage_id})"
            )),
        }
    }

    fn estimate_tokens(&self, _request: &AiRequest) -> usize {
        100
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: "replay-mock".into(),
            context_window_size: 1_000_000,
        }
    }
}

// ── Fixture builders ────────────────────────────────────────────────────

pub fn concern(concern_type: &str, description: &str, severity: &str) -> Value {
    serde_json::json!({
        "type": concern_type,
        "description": description,
        "reasoning": format!("Reasoning for: {description}"),
        "preexisting": false,
        "severity": severity,
        "locations": [{
            "file": "test.c",
            "function_or_symbol": "test_func",
            "line_range": "10-20",
            "why_this_location_matters": "test location"
        }]
    })
}

pub fn dismissed_concern(concern_type: &str, description: &str) -> Value {
    serde_json::json!({
        "type": concern_type,
        "description": description,
        "reasoning": format!("Dismissed because: {description}"),
        "locations": [{
            "file": "test.c",
            "function_or_symbol": "test_func",
            "line_range": "10-20",
            "why_this_location_matters": "test location"
        }]
    })
}

pub fn finding(problem: &str, severity: &str, origin: &str) -> Value {
    serde_json::json!({
        "problem": problem,
        "severity": severity,
        "severity_explanation": format!("Because: {problem}"),
        "preexisting": origin != "resolution_introduced",
        "origin": origin,
        "locations": [{
            "file": "test.c",
            "function_or_symbol": "test_func",
            "line": 15,
            "code_snippet": "int x = 0;",
            "why_this_location_matters": "test location"
        }]
    })
}

pub fn stage_concerns_response(
    stage_id: &str,
    concerns: Vec<Value>,
    dismissed: Vec<Value>,
) -> CannedResponse {
    CannedResponse {
        stage_id: stage_id.into(),
        content: serde_json::json!({
            "concerns": concerns,
            "dismissed_concerns": dismissed,
        })
        .to_string(),
        ..Default::default()
    }
}

pub fn stage_findings_response(stage_id: &str, findings: Vec<Value>) -> CannedResponse {
    CannedResponse {
        stage_id: stage_id.into(),
        content: serde_json::json!({ "findings": findings }).to_string(),
        ..Default::default()
    }
}

pub fn stage_inline_response(stage_id: &str, text: &str) -> CannedResponse {
    CannedResponse {
        stage_id: stage_id.into(),
        content: text.into(),
        ..Default::default()
    }
}

// ── Prompt comparison ───────────────────────────────────────────────────

/// Normalize a prompt by stripping stage title lines so renumbered stages
/// compare equal.  "# Stage N. Title" becomes "# Title".
pub fn normalize_prompt(prompt: &str) -> String {
    prompt
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("# Stage ") {
                if let Some(dot) = trimmed.find('.') {
                    format!("# {}", trimmed[dot + 1..].trim())
                } else {
                    line.to_string()
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn compare_prompts(v1: &[RecordedCall], v2: &[RecordedCall]) -> Vec<String> {
    let mut diffs = Vec::new();
    if v1.len() != v2.len() {
        diffs.push(format!("Call count: V1={}, V2={}", v1.len(), v2.len()));
        return diffs;
    }
    for (i, (a, b)) in v1.iter().zip(v2.iter()).enumerate() {
        if a.stage_id != b.stage_id {
            diffs.push(format!(
                "Call {i}: stage V1={}, V2={}",
                a.stage_id, b.stage_id
            ));
        }
        let an = normalize_prompt(&a.user_prompt);
        let bn = normalize_prompt(&b.user_prompt);
        if an != bn {
            for (j, (la, lb)) in an.lines().zip(bn.lines()).enumerate() {
                if la != lb {
                    diffs.push(format!(
                        "Call {i} ({}): line {j} differs:\n  V1: {la}\n  V2: {lb}",
                        a.stage_id
                    ));
                    break;
                }
            }
        }
    }
    diffs
}

// ── RecordingProvider ───────────────────────────────────────────────────

/// Wraps a real AiProvider, forwards calls, and records request/response pairs.
pub struct RecordingProvider {
    inner: Arc<dyn AiProvider>,
    pub recordings: Mutex<Vec<RecordedExchange>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecordedExchange {
    pub stage_id: String,
    pub request_messages: Vec<RecordedMessage>,
    #[serde(default)]
    pub response_content: Option<String>,
    #[serde(default)]
    pub response_tool_calls: Option<Vec<crate::ai::ToolCall>>,
    pub response_usage: Option<AiUsage>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecordedMessage {
    pub role: String,
    pub content: Option<String>,
}

impl RecordingProvider {
    pub fn new(inner: Arc<dyn AiProvider>) -> Self {
        Self {
            inner,
            recordings: Mutex::new(Vec::new()),
        }
    }

    pub fn save_to_file(&self, path: &str) {
        let recordings = self.recordings.lock().unwrap();
        let json = serde_json::to_string_pretty(&*recordings).unwrap();
        std::fs::write(path, json).unwrap();
    }

    /// Convert recordings to CannedResponses for replay.
    pub fn to_canned_responses(&self) -> Vec<CannedResponse> {
        self.recordings
            .lock()
            .unwrap()
            .iter()
            .map(|r| CannedResponse {
                stage_id: r.stage_id.clone(),
                content: r.response_content.clone().unwrap_or_default(),
                tool_calls: r.response_tool_calls.clone(),
                ..Default::default()
            })
            .collect()
    }
}

#[async_trait]
impl AiProvider for RecordingProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let stage_id = detect_stage(&request);

        let messages: Vec<RecordedMessage> = request
            .messages
            .iter()
            .map(|m| RecordedMessage {
                role: format!("{:?}", m.role),
                content: m.content.clone(),
            })
            .collect();

        let response = self.inner.generate_content(request).await?;

        self.recordings.lock().unwrap().push(RecordedExchange {
            stage_id,
            request_messages: messages,
            response_content: response.content.clone(),
            response_tool_calls: response.tool_calls.clone(),
            response_usage: response.usage.clone(),
        });

        Ok(response)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        self.inner.estimate_tokens(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        self.inner.get_capabilities()
    }
}

/// Load recorded exchanges from a JSON fixture file and convert to
/// CannedResponses for replay.
pub fn load_fixture(path: &str) -> Vec<CannedResponse> {
    let json = std::fs::read_to_string(path).expect("fixture file");
    let exchanges: Vec<RecordedExchange> = serde_json::from_str(&json).expect("valid fixture JSON");
    exchanges
        .into_iter()
        .map(|r| CannedResponse {
            stage_id: r.stage_id,
            content: r.response_content.unwrap_or_default(),
            tool_calls: r.response_tool_calls,
            ..Default::default()
        })
        .collect()
}

// ── Synthetic Git Repo Setup ────────────────────────────────────────────

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct SyntheticRepoInfo {
    pub base_sha: String,
    pub original_sha: String,
    pub resolution_sha: String,
    pub resolution_diff: String,
    pub repo_path: PathBuf,
}

pub fn setup_synthetic_git_repo(
    source_dir: &Path,
    target_repo: &Path,
) -> Result<SyntheticRepoInfo> {
    let run = |args: &[&str], cwd: &Path| -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .current_dir(cwd)
            .output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {:?} failed in {:?}: {}",
                args,
                cwd,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };

    run(&["init"], target_repo)?;
    run(&["config", "user.name", "Alice Developer"], target_repo)?;
    run(&["config", "user.email", "alice@example.com"], target_repo)?;
    run(&["config", "commit.gpgsign", "false"], target_repo)?;

    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
            } else {
                std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    // 1. Base commit
    let base_src = source_dir.join("base");
    if base_src.exists() {
        copy_dir_all(&base_src, target_repo)?;
    }
    run(&["add", "-A"], target_repo)?;
    run(
        &["commit", "-m", "fs: sample_cache: initial skeleton"],
        target_repo,
    )?;
    let base_sha = run(&["rev-parse", "HEAD"], target_repo)?;

    // 2. Upstream original commit
    run(&["checkout", "-b", "upstream"], target_repo)?;
    let orig_src = source_dir.join("original");
    if orig_src.exists() {
        copy_dir_all(&orig_src, target_repo)?;
    }
    run(&["add", "-A"], target_repo)?;
    run(
        &[
            "commit",
            "-m",
            "fs: sample_cache: implement buffer cache pool",
        ],
        target_repo,
    )?;
    let original_sha = run(&["rev-parse", "HEAD"], target_repo)?;

    // 3. Resolution commit
    run(
        &["checkout", "-b", "target_resolution", &base_sha],
        target_repo,
    )?;
    let res_src = source_dir.join("resolution");
    if res_src.exists() {
        copy_dir_all(&res_src, target_repo)?;
    }
    run(&["add", "-A"], target_repo)?;
    run(
        &[
            "commit",
            "-m",
            "fs: sample_cache: merge upstream buffer cache pool with locking",
        ],
        target_repo,
    )?;
    let resolution_sha = run(&["rev-parse", "HEAD"], target_repo)?;

    let resolution_diff = run(&["diff", &base_sha, &resolution_sha], target_repo)?;

    Ok(SyntheticRepoInfo {
        base_sha,
        original_sha,
        resolution_sha,
        resolution_diff,
        repo_path: target_repo.to_path_buf(),
    })
}
