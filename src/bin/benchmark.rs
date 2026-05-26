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

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::StreamExt;
use regex::Regex;
use reqwest::Client;
use sashiko::ai::{
    AiErrorClass, AiMessage, AiProvider, AiRequest, AiRole, classify_ai_error, create_provider,
};
use sashiko::db::Database;
use sashiko::settings::Settings;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

// These regexes are literals. A failure here means the source pattern was
// edited into an invalid regex, so fail during one-time initialization.
static EVAL_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(DETECTED|PARTIAL_STRONG|PARTIAL_WEAK|PARTIALLY_DETECTED|PARTIAL|MISSED)\b")
        .expect("valid benchmark evaluation status regex")
});

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the benchmark file
    #[arg(short, long)]
    file: String,

    /// Override the default port (reads from settings by default)
    #[arg(short, long)]
    port: Option<u16>,

    /// Override the default repo URL (default: kernel.org linux.git)
    #[arg(short, long)]
    repo: Option<String>,

    /// Only run the evaluation phase on existing DB results, skipping ingestion and waiting
    #[arg(long)]
    analyze_only: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct BenchmarkEntry {
    #[serde(rename = "Commit")]
    commit: String,
    #[serde(rename = "Fixed-by")]
    _fixed_by: Option<String>,
    #[serde(rename = "subsystem")]
    _subsystem: Option<String>,
    #[serde(rename = "problem_description")]
    problem_description: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SubmitRequest {
    Remote { sha: String, repo: String },
}

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    commit: String,
    problem_description: String,
    found: bool,
    status: String, // DETECTED, PARTIAL_STRONG, PARTIAL_WEAK, REVIEWED_MISSED, FAILED_*, NOT_REVIEWED, SKIPPED, NOT_FOUND_IN_DB
    explanation: String,
    raw_status: Option<String>,
    raw_explanation: Option<String>,
    review_status: Option<String>,
    failure_reason: Option<String>,
    fallback_mode: Option<String>,
    findings_count: usize,
    concerns_count: usize,
    stage8_input_concerns_count: Option<usize>,
    stage8_output_concerns_count: Option<usize>,
    stage8_dropped_concerns_count: Option<usize>,
    stage8_dropped_concerns: Option<serde_json::Value>,
    stage9_input_concerns_count: Option<usize>,
    stage9_findings_count: Option<usize>,
    stage9_dropped_candidates_count: Option<usize>,
    stage9_dropped_candidates: Option<serde_json::Value>,
    tokens_in: u32,
    tokens_out: u32,
    turns: u32,
    duration_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize tracing
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(env_filter)
        .with_writer(sashiko::logging::IgnoreBrokenPipe(std::io::stdout))
        .init();

    // Initialize settings and DB
    let settings = Settings::new().context("Failed to load settings")?;
    let db = Arc::new(
        Database::new(&settings.database)
            .await
            .context("Failed to connect to database")?,
    );

    // Load benchmark data
    let benchmark_path = Path::new(&args.file);
    let file =
        File::open(benchmark_path).with_context(|| format!("Failed to open {}", args.file))?;
    let reader = BufReader::new(file);
    let benchmark_entries: Vec<BenchmarkEntry> = serde_json::from_reader(reader)
        .with_context(|| format!("Failed to parse {}", args.file))?;

    let total_entries = benchmark_entries.len();
    info!("Loaded {} benchmark entries.", total_entries);

    if !args.analyze_only {
        let port = args.port.unwrap_or(settings.server.port);
        let repo_url = args.repo.clone().unwrap_or_else(|| {
            "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git".to_string()
        });

        let target_url = if settings.server.host.contains(':') {
            format!("http://[::1]:{}/api/submit", port)
        } else {
            format!("http://{}:{}/api/submit", settings.server.host, port)
        };
        let client = Client::new();

        // --- Phase 1: Ingestion ---
        info!("--- Phase 1: Ingesting Patches ---");
        for entry in &benchmark_entries {
            info!("Submitting commit: {}", entry.commit);
            let payload = SubmitRequest::Remote {
                sha: entry.commit.clone(),
                repo: repo_url.clone(),
            };

            let res = client.post(&target_url).json(&payload).send().await;
            match res {
                Ok(response) => {
                    if response.status().is_success() {
                        info!("Successfully submitted {}", entry.commit);
                    } else {
                        let status = response.status();
                        let text = response.text().await.unwrap_or_default();
                        error!(
                            "Failed to submit {}: Status {} Body: {}",
                            entry.commit, status, text
                        );
                    }
                }
                Err(e) => {
                    error!("Failed to send request for {}: {}", entry.commit, e);
                }
            }
        }

        // --- Phase 2: Wait for Reviews to Finish ---
        info!("--- Phase 2: Waiting for Reviews to Complete ---");
        loop {
            let mut all_completed = true;
            let mut missing_patches = 0;
            let mut pending_reviews = 0;
            let mut completed_reviews = 0;

            for entry in &benchmark_entries {
                // Find patch ID
                let mut rows = db
                    .conn
                    .query(
                        "SELECT id FROM patches WHERE message_id = ?",
                        libsql::params![entry.commit.clone()],
                    )
                    .await?;

                let patch_id = if let Ok(Some(row)) = rows.next().await {
                    row.get::<i64>(0).unwrap_or_default()
                } else {
                    // Patch not found yet (maybe still downloading/parsing)
                    all_completed = false;
                    missing_patches += 1;
                    continue;
                };

                // Check review status
                let mut rows = db
                    .conn
                    .query(
                        "SELECT status FROM reviews WHERE patch_id = ? ORDER BY id DESC LIMIT 1",
                        libsql::params![patch_id],
                    )
                    .await?;

                if let Ok(Some(row)) = rows.next().await {
                    let status: String = row.get(0).unwrap_or_default();
                    if status == "Pending" || status == "In Review" {
                        all_completed = false;
                        pending_reviews += 1;
                    } else {
                        completed_reviews += 1;
                    }
                } else {
                    // No review created yet
                    all_completed = false;
                    pending_reviews += 1;
                }
            }

            if all_completed {
                info!("All {} patches have been reviewed.", total_entries);
                break;
            }

            info!(
                "Waiting... Completed: {}, Pending: {}, Missing Patches: {}",
                completed_reviews, pending_reviews, missing_patches
            );
            sleep(Duration::from_secs(5)).await;
        }
    }

    // --- Phase 3: Evaluate Results ---
    info!("--- Phase 3: Evaluating Results ---");
    let ai_provider = create_provider(&settings).context("Failed to create AI provider")?;
    let processed_count = Arc::new(AtomicUsize::new(0));
    let concurrency = settings.review.concurrency;
    info!("Running evaluation with concurrency: {}", concurrency);

    let results: Vec<BenchmarkResult> = futures::stream::iter(benchmark_entries)
        .map(|entry| {
            let db = db.clone();
            let client = ai_provider.clone();
            let processed_count = processed_count.clone();
            async move {
                let res = process_entry(db, client, entry).await;
                let current = processed_count.fetch_add(1, Ordering::Relaxed) + 1;
                if current.is_multiple_of(10) {
                    info!("Progress: {}/{}", current, total_entries);
                }
                res
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    // Aggregate Stats
    let mut status_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut detected_count = 0usize;
    let mut partial_strong_count = 0usize;
    let mut partial_weak_count = 0usize;
    let mut reviewed_missed_count = 0usize;
    let mut reviewed_with_valid_report = 0usize;
    let mut skipped_size_count = 0usize;
    let mut skipped_count = 0usize;
    let mut terminal_count = 0usize;

    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut total_turns: u64 = 0;
    let mut total_duration: u64 = 0;
    let mut valid_metric_count: u64 = 0;
    let mut total_findings: u64 = 0;
    let mut total_concerns: u64 = 0;
    let mut tokens_in_values = Vec::new();
    let mut turns_values = Vec::new();
    let mut duration_values = Vec::new();
    let mut findings_values = Vec::new();

    for res in &results {
        *status_counts.entry(res.status.as_str()).or_default() += 1;
        if is_terminal_benchmark_status(&res.status) {
            terminal_count += 1;
        }
        match res.status.as_str() {
            "DETECTED" => {
                detected_count += 1;
                reviewed_with_valid_report += 1;
            }
            "PARTIAL_STRONG" | "PARTIAL" => {
                partial_strong_count += 1;
                reviewed_with_valid_report += 1;
            }
            "PARTIAL_WEAK" => {
                partial_weak_count += 1;
                reviewed_with_valid_report += 1;
            }
            "REVIEWED_MISSED" => {
                reviewed_missed_count += 1;
                reviewed_with_valid_report += 1;
            }
            "SKIPPED" => {
                skipped_count += 1;
            }
            "SKIPPED_SIZE" => {
                skipped_size_count += 1;
            }
            _ => {}
        }

        if !matches!(res.status.as_str(), "SKIPPED_SIZE" | "SKIPPED")
            && (res.turns > 0 || res.duration_secs > 0)
        {
            total_tokens_in += res.tokens_in as u64;
            total_tokens_out += res.tokens_out as u64;
            total_turns += res.turns as u64;
            total_duration += res.duration_secs;
            total_findings += res.findings_count as u64;
            total_concerns += res.concerns_count as u64;
            tokens_in_values.push(res.tokens_in as u64);
            turns_values.push(res.turns as u64);
            duration_values.push(res.duration_secs);
            findings_values.push(res.findings_count as u64);
            valid_metric_count += 1;
        }
    }
    let actionable_signal_count = detected_count + partial_strong_count;
    let broad_signal_count = actionable_signal_count + partial_weak_count;
    let ai_reviewed_count = terminal_count.saturating_sub(skipped_size_count + skipped_count);

    // Output results
    let output_file = File::create("benchmark_results.json")?;
    serde_json::to_writer_pretty(output_file, &results)?;

    info!("Benchmark Complete.");
    info!("Total Entries: {}", results.len());
    info!("Detected (Exact): {}", detected_count);
    info!("Partial Strong: {}", partial_strong_count);
    info!("Partial Weak: {}", partial_weak_count);
    info!("Reviewed Missed: {}", reviewed_missed_count);
    for (status, count) in &status_counts {
        info!("Status {}: {}", status, count);
    }
    info!(
        "Terminal Coverage: {}/{} ({:.1}%)",
        terminal_count,
        results.len(),
        pct(terminal_count, results.len())
    );
    info!(
        "AI-Reviewed Rows: {}/{} ({:.1}%)",
        ai_reviewed_count,
        results.len(),
        pct(ai_reviewed_count, results.len())
    );
    info!(
        "Reviewed Rows With Valid Reports: {}/{} ({:.1}%)",
        reviewed_with_valid_report,
        results.len(),
        pct(reviewed_with_valid_report, results.len())
    );
    info!("Skipped Size Rows: {}", skipped_size_count);
    info!(
        "Reviewed-Only Actionable Useful Signal: {}/{} ({:.1}%)",
        actionable_signal_count,
        ai_reviewed_count,
        pct(actionable_signal_count, ai_reviewed_count)
    );
    info!(
        "End-to-End Actionable Useful Signal: {}/{} ({:.1}%)",
        actionable_signal_count,
        results.len(),
        pct(actionable_signal_count, results.len())
    );
    info!(
        "Reviewed-Only Broad Useful Signal: {}/{} ({:.1}%)",
        broad_signal_count,
        ai_reviewed_count,
        pct(broad_signal_count, ai_reviewed_count)
    );
    info!(
        "End-to-End Broad Useful Signal: {}/{} ({:.1}%)",
        broad_signal_count,
        results.len(),
        pct(broad_signal_count, results.len())
    );
    info!(
        "Detection Rate on Reviewed Patches: {}/{} ({:.1}%)",
        detected_count,
        ai_reviewed_count,
        pct(detected_count, ai_reviewed_count)
    );
    info!(
        "End-to-End Detection Rate: {}/{} ({:.1}%)",
        detected_count,
        results.len(),
        pct(detected_count, results.len())
    );
    info!("Total Concerns (Before Stage 8): {}", total_concerns);
    info!("Total Findings (Final Report): {}", total_findings);

    if valid_metric_count > 0 {
        info!("--- Performance Metrics (averages per reviewed patch) ---");
        info!(
            "Avg Tokens In:  {}",
            total_tokens_in.checked_div(valid_metric_count).unwrap_or(0)
        );
        info!(
            "Avg Tokens Out: {}",
            total_tokens_out
                .checked_div(valid_metric_count)
                .unwrap_or(0)
        );
        info!(
            "Avg Turns:      {:.1}",
            total_turns as f64 / valid_metric_count as f64
        );
        info!(
            "Turns per Reviewed Row: {:.1}",
            total_turns as f64 / valid_metric_count as f64
        );
        info!(
            "Avg Time:       {}s",
            total_duration.checked_div(valid_metric_count).unwrap_or(0)
        );
        info!("--- Cost Metrics ---");
        info!(
            "Median Tokens In: {}",
            median_u64(&mut tokens_in_values).unwrap_or(0)
        );
        info!(
            "p95 Tokens In:    {}",
            percentile_u64(&mut tokens_in_values, 95).unwrap_or(0)
        );
        info!(
            "p95 Turns:        {}",
            percentile_u64(&mut turns_values, 95).unwrap_or(0)
        );
        info!(
            "p95 Time:         {}s",
            percentile_u64(&mut duration_values, 95).unwrap_or(0)
        );
        info!(
            "Median Findings:  {}",
            median_u64(&mut findings_values).unwrap_or(0)
        );
        info!(
            "p95 Findings:     {}",
            percentile_u64(&mut findings_values, 95).unwrap_or(0)
        );
        info!(
            "Tokens In per Detection: {}",
            total_tokens_in
                .checked_div(detected_count as u64)
                .unwrap_or(0)
        );
        info!(
            "Tokens In per Partial Strong: {}",
            total_tokens_in
                .checked_div(partial_strong_count as u64)
                .unwrap_or(0)
        );
        info!(
            "Tokens In per Actionable Useful Signal: {}",
            total_tokens_in
                .checked_div(actionable_signal_count as u64)
                .unwrap_or(0)
        );
        info!(
            "Tokens In per Broad Useful Signal: {}",
            total_tokens_in
                .checked_div(broad_signal_count as u64)
                .unwrap_or(0)
        );
        info!(
            "Actionable Signal per Million Input Tokens: {:.2}",
            signal_per_million_tokens(actionable_signal_count, total_tokens_in)
        );
    }

    info!("Detailed results written to benchmark_results.json");

    Ok(())
}

async fn process_entry(
    db: Arc<Database>,
    client: Arc<dyn AiProvider>,
    entry: BenchmarkEntry,
) -> BenchmarkResult {
    if entry.problem_description.is_none() {
        return BenchmarkResult {
            commit: entry.commit,
            problem_description: "".to_string(),
            found: false,
            status: "SKIPPED".to_string(),
            explanation: "No problem description provided".to_string(),
            raw_status: None,
            raw_explanation: None,
            review_status: None,
            failure_reason: None,
            fallback_mode: None,
            findings_count: 0,
            concerns_count: 0,
            stage8_input_concerns_count: None,
            stage8_output_concerns_count: None,
            stage8_dropped_concerns_count: None,
            stage8_dropped_concerns: None,
            stage9_input_concerns_count: None,
            stage9_findings_count: None,
            stage9_dropped_candidates_count: None,
            stage9_dropped_candidates: None,
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            duration_secs: 0,
        };
    }
    let problem_description = entry.problem_description.clone().unwrap();

    // 1. Find Patch ID
    let patch_id_result = db
        .conn
        .query(
            "SELECT id FROM patches WHERE message_id = ?",
            libsql::params![entry.commit.clone()],
        )
        .await;

    let patch_id = match patch_id_result {
        Ok(mut rows) => {
            if let Ok(Some(row)) = rows.next().await {
                Some(row.get::<i64>(0).unwrap_or_default())
            } else {
                None
            }
        }
        Err(e) => {
            error!("DB Error finding patch {}: {}", entry.commit, e);
            None
        }
    };

    if patch_id.is_none() {
        warn!("Patch not found for commit {}", entry.commit);
        return BenchmarkResult {
            commit: entry.commit,
            problem_description,
            found: false,
            status: "NOT_FOUND_IN_DB".to_string(),
            explanation: "Patch not found in database.".to_string(),
            raw_status: None,
            raw_explanation: None,
            review_status: None,
            failure_reason: None,
            fallback_mode: None,
            findings_count: 0,
            concerns_count: 0,
            stage8_input_concerns_count: None,
            stage8_output_concerns_count: None,
            stage8_dropped_concerns_count: None,
            stage8_dropped_concerns: None,
            stage9_input_concerns_count: None,
            stage9_findings_count: None,
            stage9_dropped_candidates_count: None,
            stage9_dropped_candidates: None,
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            duration_secs: 0,
        };
    }
    let patch_id = patch_id.unwrap();

    // 2. Find Review
    let review_result = db
        .conn
        .query(
            "SELECT id, summary, result_description, interaction_id, created_at, status FROM reviews WHERE patch_id = ? ORDER BY id DESC LIMIT 1",
            libsql::params![patch_id],
        )
        .await;

    let review_data = match review_result {
        Ok(mut rows) => {
            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0).unwrap_or_default();
                let summary: Option<String> = row.get(1).ok();
                let result_desc: Option<String> = row.get(2).ok();
                let interaction_id: Option<String> = row.get(3).ok();
                let created_at: Option<i64> = row.get(4).ok();
                let review_status: Option<String> = row.get(5).ok();
                Some((
                    id,
                    summary,
                    result_desc,
                    interaction_id,
                    created_at,
                    review_status,
                ))
            } else {
                None
            }
        }
        Err(_) => None,
    };

    if review_data.is_none() {
        warn!("Review not found for patch {}", patch_id);
        return BenchmarkResult {
            commit: entry.commit,
            problem_description,
            found: false,
            status: "NOT_REVIEWED".to_string(),
            explanation: "Patch found but no review exists.".to_string(),
            raw_status: None,
            raw_explanation: None,
            review_status: None,
            failure_reason: None,
            fallback_mode: None,
            findings_count: 0,
            concerns_count: 0,
            stage8_input_concerns_count: None,
            stage8_output_concerns_count: None,
            stage8_dropped_concerns_count: None,
            stage8_dropped_concerns: None,
            stage9_input_concerns_count: None,
            stage9_findings_count: None,
            stage9_dropped_candidates_count: None,
            stage9_dropped_candidates: None,
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            duration_secs: 0,
        };
    }
    let (review_id, summary, result_desc, interaction_id, review_created_at, review_status) =
        review_data.unwrap();

    // Metrics Tracking
    let mut tokens_in = 0;
    let mut tokens_out = 0;
    let mut duration_secs = 0;
    let mut turns = 1; // Minimum 1 turn for the initial prompt
    let mut concerns_count = 0;
    let mut fallback_mode = None;
    let mut stage8_input_concerns_count = None;
    let mut stage8_output_concerns_count = None;
    let mut stage8_dropped_concerns_count = None;
    let mut stage8_dropped_concerns = None;
    let mut stage9_input_concerns_count = None;
    let mut stage9_findings_count = None;
    let mut stage9_dropped_candidates_count = None;
    let mut stage9_dropped_candidates = None;

    if let Some(iid) = interaction_id {
        let int_rows = db
            .conn
            .query(
                "SELECT tokens_in, tokens_out, created_at, output_raw FROM ai_interactions WHERE id = ?",
                libsql::params![iid],
            )
            .await;

        if let Ok(mut rows) = int_rows
            && let Ok(Some(row)) = rows.next().await
        {
            tokens_in = row.get::<i64>(0).unwrap_or(0) as u32;
            tokens_out = row.get::<i64>(1).unwrap_or(0) as u32;
            let int_created_at = row.get::<i64>(2).unwrap_or(0);

            if let Ok(Some(output_raw)) = row.get::<Option<String>>(3)
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output_raw)
            {
                if let Some(count) = parsed.get("concerns_count").and_then(|v| v.as_u64()) {
                    concerns_count = count as usize;
                }
                fallback_mode = parsed
                    .get("fallback_mode")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                stage8_input_concerns_count = parsed
                    .get("stage8_input_concerns_count")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                stage8_output_concerns_count = parsed
                    .get("stage8_output_concerns_count")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                if let Some(value) = parsed.get("stage8_dropped_concerns") {
                    stage8_dropped_concerns_count = value.as_array().map(|v| v.len());
                    stage8_dropped_concerns = Some(value.clone());
                }
                stage9_input_concerns_count = parsed
                    .get("stage9_input_concerns_count")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                stage9_findings_count = parsed
                    .get("stage9_findings_count")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                if let Some(value) = parsed.get("stage9_dropped_candidates") {
                    stage9_dropped_candidates_count = value.as_array().map(|v| v.len());
                    stage9_dropped_candidates = Some(value.clone());
                }
            }

            if let Some(start_time) = review_created_at
                && int_created_at >= start_time
            {
                duration_secs = (int_created_at - start_time) as u64;
            }
        }
    }

    // Number of turns based on tool usages
    let tool_usages_result = db
        .conn
        .query(
            "SELECT COUNT(*) FROM tool_usages WHERE review_id = ?",
            libsql::params![review_id],
        )
        .await;

    if let Ok(mut rows) = tool_usages_result
        && let Ok(Some(row)) = rows.next().await
    {
        let tool_count: i64 = row.get(0).unwrap_or(0);
        turns = 1 + tool_count as u32; // Each tool call adds a turn, plus final response
    }

    // 3. Find Findings
    let findings_result = db
        .conn
        .query(
            "SELECT problem, severity, severity_explanation FROM findings WHERE review_id = ?",
            libsql::params![review_id],
        )
        .await;

    let mut findings_text = String::new();
    let mut findings_count = 0;

    if let Ok(mut rows) = findings_result {
        while let Ok(Some(row)) = rows.next().await {
            let msg: String = row.get(0).unwrap_or_default();
            let severity: i32 = row.get(1).unwrap_or(0);
            let explanation: Option<String> = row.get(2).ok();

            findings_text.push_str(&format!("- [Severity {}] {}\n", severity, msg));
            if let Some(e) = explanation {
                findings_text.push_str(&format!("  Explanation: {}\n", e));
            }
            findings_count += 1;
        }
    }

    if findings_count == 0 {
        findings_text.push_str("(No structured findings recorded in DB)\n");
    }

    if matches!(review_status.as_deref(), Some("Skipped")) {
        let skipped_status =
            classify_benchmark_skipped_status(summary.as_deref(), result_desc.as_deref());
        let explanation = result_desc
            .clone()
            .or(summary.clone())
            .unwrap_or_else(|| "Review skipped before AI review.".to_string());
        info!(
            "Commit {}: {} ({})",
            entry.commit, skipped_status, explanation
        );
        return BenchmarkResult {
            commit: entry.commit,
            problem_description,
            found: false,
            status: skipped_status.to_string(),
            explanation,
            raw_status: None,
            raw_explanation: None,
            review_status,
            failure_reason: result_desc,
            fallback_mode,
            findings_count,
            concerns_count,
            stage8_input_concerns_count,
            stage8_output_concerns_count,
            stage8_dropped_concerns_count,
            stage8_dropped_concerns,
            stage9_input_concerns_count,
            stage9_findings_count,
            stage9_dropped_candidates_count,
            stage9_dropped_candidates,
            tokens_in,
            tokens_out,
            turns,
            duration_secs,
        };
    }

    if let Some(failure_status) =
        classify_failed_review(review_status.as_deref(), result_desc.as_deref())
    {
        let explanation = result_desc
            .clone()
            .unwrap_or_else(|| "Review failed without a recorded reason.".to_string());
        info!(
            "Commit {}: {} ({})",
            entry.commit, failure_status, explanation
        );
        return BenchmarkResult {
            commit: entry.commit,
            problem_description,
            found: false,
            status: failure_status.to_string(),
            explanation,
            raw_status: None,
            raw_explanation: None,
            review_status,
            failure_reason: result_desc,
            fallback_mode,
            findings_count,
            concerns_count,
            stage8_input_concerns_count,
            stage8_output_concerns_count,
            stage8_dropped_concerns_count,
            stage8_dropped_concerns,
            stage9_input_concerns_count,
            stage9_findings_count,
            stage9_dropped_candidates_count,
            stage9_dropped_candidates,
            tokens_in,
            tokens_out,
            turns,
            duration_secs,
        };
    }

    // 4. Evaluate with AI provider
    let review_summary = format!(
        "{}\n{}",
        summary.unwrap_or_default(),
        result_desc.as_deref().unwrap_or_default()
    );

    let prompt = format!(
        "I am benchmarking an automated code review tool.\n\n\
        The known issue (ground truth) is:\n\
        {}\n\n\
        The tool produced the following findings:\n\
        {}\n\n\
        The review summary was:\n\
        {}\n\n\
        Task:\n\
        Determine if ANY of the findings or the review summary describes the known issue.\n\
        - DETECTED requires the same specific problem and root cause (e.g., 'memory leak in function X', 'double free', 'missing lock').\n\
        - PARTIAL_STRONG applies when a finding points at the same affected function/resource/path and plausible failure consequence, but has imperfect root-cause wording or attribution.\n\
        - PARTIAL_WEAK applies when a finding is in the same subsystem or nearby mechanism, but a human must infer the actual benchmark bug from incomplete or adjacent evidence.\n\
        - For example, a finding that mentions a leak or bad error handling involving the same allocation/resource should be PARTIAL_STRONG even if it labels the top-line issue incorrectly.\n\
        - For resource leak benchmarks, DETECTED applies when the finding identifies the same resource/helper and the same error path, even if the headline says resource management or use-after-free instead of leak.\n\
        - General warnings about code style, complexity, or unrelated bugs do NOT count.\n\
        - If a finding describes the problem but with slight inaccuracy (e.g. wrong variable name but correct logic), it is PARTIAL_STRONG.\n\
        - If no finding matches the problem, it is MISSED.\n\n\
        Respond with EXACTLY one of: [DETECTED, PARTIAL_STRONG, PARTIAL_WEAK, MISSED].\n\
        Then provide a short one-sentence explanation referencing the specific finding that matches (if any).",
        problem_description, findings_text, review_summary
    );

    info!("Evaluating commit {}...", entry.commit);

    let r = loop {
        let req = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some(prompt.clone()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        match client.generate_content(req).await {
            Ok(r) => break r,
            Err(e) => match classify_ai_error(&e) {
                AiErrorClass::RateLimit { retry_after }
                | AiErrorClass::Transient { retry_after } => {
                    warn!(
                        "API error ({}), pausing for {:?} before retry...",
                        e, retry_after
                    );
                    tokio::time::sleep(retry_after).await;
                }
                AiErrorClass::Fatal => {
                    return BenchmarkResult {
                        commit: entry.commit,
                        problem_description,
                        found: false,
                        status: "UNKNOWN".to_string(),
                        explanation: format!("Evaluation failed: {}", e),
                        raw_status: None,
                        raw_explanation: None,
                        review_status,
                        failure_reason: result_desc,
                        fallback_mode,
                        findings_count,
                        concerns_count,
                        stage8_input_concerns_count,
                        stage8_output_concerns_count,
                        stage8_dropped_concerns_count,
                        stage8_dropped_concerns,
                        stage9_input_concerns_count,
                        stage9_findings_count,
                        stage9_dropped_candidates_count,
                        stage9_dropped_candidates,
                        tokens_in,
                        tokens_out,
                        turns,
                        duration_secs,
                    };
                }
            },
        }
    };

    let (status, explanation) = {
        let text = r.content.unwrap_or_else(|| "Unknown".to_string());

        let (status_raw, expl_raw) = if let Some(cap) = EVAL_STATUS_RE.captures(&text) {
            let s = cap[1].to_uppercase();
            let remaining = EVAL_STATUS_RE.replace(&text, "").to_string();
            (s, remaining)
        } else {
            ("UNKNOWN".to_string(), text.clone())
        };

        let expl = expl_raw
            .trim()
            .trim_start_matches([':', '-', ' ', '\n'])
            .to_string();
        (status_raw, expl)
    };

    let raw_status = normalize_eval_status(&status);
    let raw_explanation = explanation;
    let status = raw_status.clone();
    let explanation = raw_explanation.clone();
    let found = matches!(
        status.as_str(),
        "DETECTED" | "PARTIAL_STRONG" | "PARTIAL_WEAK"
    );
    info!("Commit {}: {} ({})", entry.commit, status, explanation);

    BenchmarkResult {
        commit: entry.commit,
        problem_description,
        found,
        status,
        explanation,
        raw_status: Some(raw_status),
        raw_explanation: Some(raw_explanation),
        review_status,
        failure_reason: result_desc,
        fallback_mode,
        findings_count,
        concerns_count,
        stage8_input_concerns_count,
        stage8_output_concerns_count,
        stage8_dropped_concerns_count,
        stage8_dropped_concerns,
        stage9_input_concerns_count,
        stage9_findings_count,
        stage9_dropped_candidates_count,
        stage9_dropped_candidates,
        tokens_in,
        tokens_out,
        turns,
        duration_secs,
    }
}

fn pct(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

fn signal_per_million_tokens(signal_count: usize, tokens_in: u64) -> f64 {
    if tokens_in == 0 {
        0.0
    } else {
        signal_count as f64 * 1_000_000.0 / tokens_in as f64
    }
}

fn normalize_eval_status(status: &str) -> String {
    match status {
        "PARTIALLY_DETECTED" | "PARTIAL" => "PARTIAL_STRONG".to_string(),
        "MISSED" => "REVIEWED_MISSED".to_string(),
        _ => status.to_string(),
    }
}

fn is_terminal_benchmark_status(status: &str) -> bool {
    matches!(
        status,
        "DETECTED"
            | "PARTIAL_STRONG"
            | "PARTIAL_WEAK"
            | "PARTIAL"
            | "REVIEWED_MISSED"
            | "FAILED_BUDGET"
            | "FAILED_PROTOCOL"
            | "FAILED_MAX_TURNS"
            | "FAILED_NO_REPORT"
            | "SKIPPED_SIZE"
            | "SKIPPED"
    )
}

fn median_u64(values: &mut [u64]) -> Option<u64> {
    percentile_u64(values, 50)
}

fn percentile_u64(values: &mut [u64], percentile: usize) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = (values.len() * percentile).div_ceil(100).saturating_sub(1);
    values.get(rank).copied()
}

fn classify_failed_review(
    review_status: Option<&str>,
    result_desc: Option<&str>,
) -> Option<&'static str> {
    if !matches!(review_status, Some("Failed")) {
        return None;
    }

    let desc = result_desc.unwrap_or_default().to_ascii_lowercase();
    if desc.contains("tokenbudgetexceeded")
        || desc.contains("token budget exceeded")
        || desc.contains("output token budget exceeded")
        || desc.contains("max_input_tokens")
        || desc.contains("preflight cap")
        || desc.contains("prompt estimate")
    {
        return Some("FAILED_BUDGET");
    }
    if desc.contains("max interactions exceeded") {
        return Some("FAILED_MAX_TURNS");
    }
    if desc.contains("failed to produce valid")
        || desc.contains("format validation")
        || desc.contains("json")
        || desc.contains("schema")
    {
        return Some("FAILED_PROTOCOL");
    }
    if desc.contains("without valid result")
        || desc.contains("missing review content")
        || desc.contains("null response")
        || desc.contains("missing or empty")
        || desc.contains("stage 10 failed")
    {
        return Some("FAILED_NO_REPORT");
    }

    Some("FAILED_PROTOCOL")
}

fn classify_benchmark_skipped_status(
    summary: Option<&str>,
    result_desc: Option<&str>,
) -> &'static str {
    let reason = result_desc
        .or(summary)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if reason.contains("exceeds size limits") || reason.contains("size limits") {
        "SKIPPED_SIZE"
    } else {
        "SKIPPED"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank() {
        let mut values = vec![10, 20, 30, 40, 50];

        assert_eq!(median_u64(&mut values), Some(30));
        assert_eq!(percentile_u64(&mut values, 95), Some(50));
    }

    #[test]
    fn normalizes_legacy_partial_status_to_partial_strong() {
        assert_eq!(
            normalize_eval_status("PARTIALLY_DETECTED"),
            "PARTIAL_STRONG"
        );
        assert_eq!(normalize_eval_status("PARTIAL"), "PARTIAL_STRONG");
        assert_eq!(normalize_eval_status("PARTIAL_WEAK"), "PARTIAL_WEAK");
        assert_eq!(normalize_eval_status("MISSED"), "REVIEWED_MISSED");
    }

    #[test]
    fn signal_per_million_tokens_handles_empty_denominator() {
        assert_eq!(signal_per_million_tokens(3, 0), 0.0);
        assert_eq!(signal_per_million_tokens(2, 4_000_000), 0.5);
    }

    #[test]
    fn terminal_status_includes_size_skips() {
        assert!(is_terminal_benchmark_status("SKIPPED_SIZE"));
        assert!(is_terminal_benchmark_status("FAILED_BUDGET"));
        assert!(is_terminal_benchmark_status("PARTIAL_STRONG"));
        assert!(is_terminal_benchmark_status("PARTIAL_WEAK"));
        assert!(!is_terminal_benchmark_status("NOT_REVIEWED"));
    }
}
