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
use sashiko::ai::{AiMessage, AiProvider, AiRequest, AiRole, create_provider};
use sashiko::ai::gemini::GeminiError;
use sashiko::ai::claude::ClaudeError;
use sashiko::ai::openai::OpenAiCompatError;
use sashiko::db::Database;
use sashiko::settings::Settings;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the benchmark file
    #[arg(short, long)]
    file: String,
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

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    commit: String,
    problem_description: String,
    found: bool,
    status: String, // "DETECTED", "PARTIALLY_DETECTED", "MISSED", "UNKNOWN", "NOT_REVIEWED", "SKIPPED"
    explanation: String,
    findings_count: usize,
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

    // Initialize AI provider for evaluation based on settings
    let ai_provider = create_provider(&settings).context("Failed to create AI provider")?;

    let processed_count = Arc::new(AtomicUsize::new(0));

    // Process concurrently using settings
    let concurrency = settings.review.concurrency;
    info!("Running with concurrency: {}", concurrency);

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
    let mut detected_count = 0;
    let mut partially_detected_count = 0;
    let mut missed_count = 0;
    let mut not_reviewed_count = 0;
    let mut skipped_count = 0;

    for res in &results {
        match res.status.as_str() {
            "DETECTED" => detected_count += 1,
            "PARTIALLY_DETECTED" => partially_detected_count += 1,
            "MISSED" => missed_count += 1,
            "NOT_REVIEWED" | "NOT_FOUND_IN_DB" => not_reviewed_count += 1,
            "SKIPPED" => skipped_count += 1,
            _ => {}
        }
    }

    // Output results
    let output_file = File::create("benchmark_results.json")?;
    serde_json::to_writer_pretty(output_file, &results)?;

    info!("Benchmark Complete.");
    info!("Total Entries: {}", results.len());
    info!("Detected (Exact): {}", detected_count);
    info!("Partially Detected: {}", partially_detected_count);
    info!("Missed: {}", missed_count);
    info!("Not Reviewed/Found: {}", not_reviewed_count);
    info!("Skipped (No Description): {}", skipped_count);
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
            findings_count: 0,
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
            findings_count: 0,
        };
    }
    let patch_id = patch_id.unwrap();

    // 2. Find Review
    let review_result = db
        .conn
        .query(
            "SELECT id, summary, result_description FROM reviews WHERE patch_id = ? ORDER BY id DESC LIMIT 1",
            libsql::params![patch_id],
        )
        .await;

    let review_data = match review_result {
        Ok(mut rows) => {
            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0).unwrap_or_default();
                let summary: Option<String> = row.get(1).ok();
                let result_desc: Option<String> = row.get(2).ok();
                Some((id, summary, result_desc))
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
            findings_count: 0,
        };
    }
    let (review_id, summary, result_desc) = review_data.unwrap();

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

    // 4. Evaluate with AI provider
    let review_summary = format!(
        "{}\n{}",
        summary.unwrap_or_default(),
        result_desc.unwrap_or_default()
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
        Determine if ANY of the findings or the review summary EXACTLY describes the known issue.\n\
        - The description must match the specific problem (e.g., 'memory leak in function X', 'double free', 'missing lock').\n\
        - General warnings about code style, complexity, or unrelated bugs do NOT count.\n\
        - If a finding describes the problem but with slight inaccuracy (e.g. wrong variable name but correct logic), it is PARTIALLY_DETECTED.\n\
        - If no finding matches the problem, it is MISSED.\n\n\
        Respond with EXACTLY one of: [DETECTED, PARTIALLY_DETECTED, MISSED].\n\
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
            Err(e) => {
                let retry_duration = e
                    .downcast_ref::<GeminiError>()
                    .and_then(|err| match err {
                        GeminiError::QuotaExceeded(d) | GeminiError::TransientError(d, _) => {
                            Some(*d)
                        }
                        _ => None,
                    })
                    .or_else(|| {
                        e.downcast_ref::<ClaudeError>().and_then(|err| match err {
                            ClaudeError::RateLimitExceeded(d)
                            | ClaudeError::OverloadedError(d) => Some(*d),
                            _ => None,
                        })
                    })
                    .or_else(|| {
                        e.downcast_ref::<OpenAiCompatError>()
                            .and_then(|err| match err {
                                OpenAiCompatError::RateLimitExceeded(d)
                                | OpenAiCompatError::TransientError(d, _) => Some(*d),
                                _ => None,
                            })
                    });

                let duration =
                    retry_duration.unwrap_or(std::time::Duration::from_secs(30));
                warn!("API error ({}), pausing for {:?} before retry...", e, duration);
                tokio::time::sleep(duration).await;
            }
        }
    };

    let (status, explanation) = {
        let text = r.content.unwrap_or_else(|| "Unknown".to_string());

        let re_status = Regex::new(r"(?i)\b(DETECTED|PARTIALLY_DETECTED|MISSED)\b").unwrap();
        let (status_raw, expl_raw) = if let Some(cap) = re_status.captures(&text) {
            let s = cap[1].to_uppercase();
            let remaining = re_status.replace(&text, "").to_string();
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

    let found = status == "DETECTED" || status == "PARTIALLY_DETECTED";
    info!("Commit {}: {} ({})", entry.commit, status, explanation);

    BenchmarkResult {
        commit: entry.commit,
        problem_description,
        found,
        status,
        explanation,
        findings_count,
    }
}
