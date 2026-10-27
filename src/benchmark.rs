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

use crate::ai::{AiProvider, LlmSession, SessionRunner, ValidationError, create_provider};
use crate::baseline::CommitId;
use crate::db::{AttributedSubsystem, Database, Severity};
use crate::events::Event;
use crate::fetcher::FetchAgent;
use crate::project::ProjectId;
use crate::reviewer::{Reviewer, WorkerSubprocessOverrides};
use crate::settings::Settings;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::stream::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

const DEFAULT_KERNEL_REPO_URL: &str =
    "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git";

#[derive(Debug, Clone)]
pub struct BenchmarkOptions {
    pub file: PathBuf,
    pub repo: Option<String>,
    pub db: Option<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub analyze_only: bool,
    pub concurrency: Option<usize>,
    pub report_preexisting: bool,
    pub project: ProjectId,
    pub prompts: PathBuf,
    pub settings_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BenchmarkEntry {
    #[serde(rename = "Commit")]
    pub commit: String,
    #[serde(rename = "Fixed-by", default, skip_serializing_if = "Option::is_none")]
    pub fixed_by: Option<String>,
    #[serde(rename = "subsystem", default, skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    #[serde(
        rename = "problem_description",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub problem_description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkRunPaths {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub db_path: PathBuf,
    pub worktree_dir: PathBuf,
    pub results_json_path: PathBuf,
    pub summary_md_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BenchmarkRunMetadata {
    pub run_id: String,
    pub started_at: String,
    pub finished_at: String,
    pub wall_clock_secs: u64,
    pub sashiko_version: String,
    pub sashiko_git_sha: String,
    pub sashiko_git_dirty: bool,
    pub prompts_git_hash: String,
    pub benchmark_file: String,
    pub total_entries: usize,
    pub unique_commits: usize,
    pub project: String,
    pub ai_provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub concurrency: usize,
    pub report_preexisting: bool,
    pub analyze_only: bool,
    pub database_path: String,
    pub results_json_path: String,
    pub summary_md_path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct MetricDistribution {
    pub total: u64,
    pub mean: f64,
    pub p50: u64,
    pub p90: u64,
    pub max: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct SeverityCounts {
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct SubsystemBenchmarkSummary {
    pub subsystem: String,
    pub total: usize,
    pub detected: usize,
    pub partially_detected: usize,
    pub missed: usize,
    pub detection_rate_pct: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BenchmarkSummaryStats {
    pub total_entries: usize,
    pub evaluated_entries: usize,
    pub unique_commits: usize,
    pub reviewed_commits: usize,
    pub detected: usize,
    pub partially_detected: usize,
    pub missed: usize,
    pub not_reviewed: usize,
    pub skipped: usize,
    pub exact_detection_rate_pct: f64,
    pub total_detection_rate_pct: f64,
    pub missed_not_raised: usize,
    pub missed_wrongly_dismissed: usize,
    pub missed_preexisting_filtered: usize,
    pub missed_review_failed: usize,
    pub total_patch_findings: usize,
    pub total_preexisting_findings: usize,
    pub matched_findings: usize,
    pub unmatched_extra_findings: usize,
    pub avg_patch_findings_per_commit: f64,
    pub clean_target_only_commits: usize,
    pub findings_by_severity: SeverityCounts,
    pub total_concerns_raised: usize,
    pub total_concerns_dismissed: usize,
    pub concern_to_finding_conversion_pct: f64,
    pub detections_by_stage: BTreeMap<String, usize>,
    pub false_dismissals_by_stage: BTreeMap<String, usize>,
    pub by_subsystem: Vec<SubsystemBenchmarkSummary>,
    pub tokens_in: MetricDistribution,
    pub tokens_out: MetricDistribution,
    pub tokens_cached: MetricDistribution,
    pub cache_hit_rate_pct: f64,
    pub turns: MetricDistribution,
    pub duration_secs: MetricDistribution,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MatchedFindingSummary {
    pub index: usize,
    pub severity: String,
    pub problem: String,
    pub preexisting: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub stages: Vec<String>,
    pub prompts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct BenchmarkFindingRecord {
    pub index: usize,
    pub severity: String,
    pub problem: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    pub preexisting: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub stages: Vec<String>,
    pub prompts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct BenchmarkDismissedRecord {
    pub index: usize,
    pub stage: String,
    pub description: String,
    pub reasoning: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct BenchmarkStageMetric {
    pub name: String,
    pub status: String,
    pub turns: u32,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub concerns_count: usize,
    pub dismissed_count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BenchmarkEntryResult {
    pub commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    pub problem_description: String,
    pub found: bool,
    pub status: String,
    pub explanation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_finding: Option<MatchedFindingSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub miss_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismissed_by_stage: Option<String>,
    pub findings_count: usize,
    pub patch_findings_count: usize,
    pub preexisting_findings_count: usize,
    pub extra_findings_count: usize,
    pub concerns_count: usize,
    pub dismissed_concerns_count: usize,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub turns: u32,
    pub duration_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patchset_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_id: Option<i64>,
    pub planned_stages: Vec<String>,
    pub selected_guides: Vec<String>,
    pub stages: Vec<BenchmarkStageMetric>,
    pub findings: Vec<BenchmarkFindingRecord>,
    pub dismissed_concerns: Vec<BenchmarkDismissedRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BenchmarkRunReport {
    pub metadata: BenchmarkRunMetadata,
    pub summary: BenchmarkSummaryStats,
    pub results: Vec<BenchmarkEntryResult>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CommitReviewData {
    pub subject: Option<String>,
    pub patchset_id: Option<i64>,
    pub patch_id: Option<i64>,
    pub review_id: Option<i64>,
    pub review_status: Option<String>,
    pub summary_text: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub turns: u32,
    pub duration_secs: u64,
    pub concerns_count: usize,
    pub dismissed_concerns_count: usize,
    pub planned_stages: Vec<String>,
    pub selected_guides: Vec<String>,
    pub stages: Vec<BenchmarkStageMetric>,
    pub findings: Vec<BenchmarkFindingRecord>,
    pub dismissed_concerns: Vec<BenchmarkDismissedRecord>,
}

impl CommitReviewData {
    pub fn patch_findings_count(&self) -> usize {
        self.findings.iter().filter(|f| !f.preexisting).count()
    }

    pub fn preexisting_findings_count(&self) -> usize {
        self.findings.iter().filter(|f| f.preexisting).count()
    }

    pub fn is_completed_review(&self) -> bool {
        self.review_id.is_some()
            && !matches!(
                self.review_status.as_deref(),
                None | Some("" | "Pending" | "In Review" | "Failed" | "Skipped" | "Cancelled")
            )
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub(crate) struct BenchmarkJudgeVerdict {
    pub status: String,
    #[serde(default)]
    pub matched_finding_index: Option<usize>,
    #[serde(default)]
    pub miss_reason: Option<String>,
    #[serde(default)]
    pub dismissed_by_stage: Option<String>,
    pub explanation: String,
}

/// Resolves the Sashiko repository's short commit SHA and dirty status.
///
/// Prefers the live git checkout when running inside the Sashiko repository so
/// local uncommitted edits are reflected via the returned dirty flag, falling
/// back to the compile-time `GIT_HASH` embedded by `build.rs`.
pub fn resolve_sashiko_git_sha(cwd: &Path) -> (String, bool) {
    let compile_sha = env!("GIT_HASH");
    let fallback_short = compile_sha
        .chars()
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();

    let Ok(out) = crate::git_cmd::in_dir(cwd)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
    else {
        return (fallback_short, false);
    };
    if !out.status.success() {
        return (fallback_short, false);
    }

    let head_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if head_sha.is_empty() || !head_sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return (fallback_short, false);
    }

    let dirty = crate::git_cmd::in_dir(cwd)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .map(|s| s.status.success() && !String::from_utf8_lossy(&s.stdout).trim().is_empty())
        .unwrap_or(false);

    (head_sha, dirty)
}

/// Sanitizes a benchmark suite stem for safe filesystem path construction.
pub fn sanitize_suite_name(file_path: &Path) -> String {
    let raw = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("benchmark");
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "benchmark".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Extracts unique commit SHAs in their first-seen order.
pub fn deduplicate_benchmark_commits(entries: &[BenchmarkEntry]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut unique = Vec::new();
    for entry in entries {
        let trimmed = entry.commit.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
            unique.push(trimmed.to_string());
        }
    }
    unique
}

/// Resolves or creates the per-run directory and preserved SQLite database path.
pub fn prepare_benchmark_run_paths(
    base_dir: &Path,
    benchmark_file: &Path,
    timestamp_tag: &str,
    sashiko_sha: &str,
    explicit_db: Option<&Path>,
    explicit_output_dir: Option<&Path>,
    analyze_only: bool,
) -> Result<BenchmarkRunPaths> {
    let suite = sanitize_suite_name(benchmark_file);
    let runs_root = match explicit_output_dir {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        Some(dir) => base_dir.join(dir),
        None => base_dir.join("benchmarks").join("runs"),
    };

    if analyze_only && explicit_db.is_none() {
        if let Some(existing) = find_latest_run_for_suite(&runs_root, &suite)? {
            return Ok(existing);
        }
        bail!(
            "No existing benchmark database found for suite '{}' in {}; \
             run without --analyze-only first or pass --db <path>",
            suite,
            runs_root.display()
        );
    }

    let run_id = format!("{timestamp_tag}_{sashiko_sha}_{suite}");
    let run_dir = runs_root.join(&run_id);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("Failed to create benchmark run directory {:?}", run_dir))?;

    let db_filename = format!("sashiko-{timestamp_tag}-{sashiko_sha}.db");
    let db_path = match explicit_db {
        Some(path) => path.to_path_buf(),
        None => run_dir.join(db_filename),
    };

    let worktree_dir = run_dir.join("worktrees");
    let results_json_path = run_dir.join("results.json");
    let summary_md_path = run_dir.join("summary.md");

    Ok(BenchmarkRunPaths {
        run_id,
        run_dir,
        db_path,
        worktree_dir,
        results_json_path,
        summary_md_path,
    })
}

fn find_latest_run_for_suite(runs_root: &Path, suite: &str) -> Result<Option<BenchmarkRunPaths>> {
    if !runs_root.exists() {
        return Ok(None);
    }
    let suffix = format!("_{suite}");
    let mut matching_dirs: Vec<PathBuf> = std::fs::read_dir(runs_root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(&suffix))
        })
        .collect();
    matching_dirs.sort();

    for dir in matching_dirs.into_iter().rev() {
        let mut db_files: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|ext| ext.to_str()) == Some("db")
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("sashiko-"))
            })
            .collect();
        db_files.sort();
        if let Some(db_path) = db_files.pop() {
            let run_id = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("existing")
                .to_string();
            return Ok(Some(BenchmarkRunPaths {
                run_id,
                worktree_dir: dir.join("worktrees"),
                results_json_path: dir.join("results.json"),
                summary_md_path: dir.join("summary.md"),
                run_dir: dir,
                db_path,
            }));
        }
    }
    Ok(None)
}

/// Executes a full self-contained benchmark run (or `--analyze-only` evaluation)
/// and returns the structured [`BenchmarkRunReport`].
pub async fn run_benchmark(
    options: BenchmarkOptions,
    mut settings: Settings,
) -> Result<BenchmarkRunReport> {
    let started_wall = Instant::now();
    let now_utc = chrono::Utc::now();
    let started_at = now_utc.to_rfc3339();
    let timestamp_tag = now_utc.format("%Y%m%d-%H%M%S").to_string();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (sashiko_git_sha, sashiko_git_dirty) = resolve_sashiko_git_sha(&cwd);

    let file = File::open(&options.file)
        .with_context(|| format!("Failed to open benchmark file {}", options.file.display()))?;
    let reader = BufReader::new(file);
    let entries: Vec<BenchmarkEntry> = serde_json::from_reader(reader)
        .with_context(|| format!("Failed to parse benchmark file {}", options.file.display()))?;
    if entries.is_empty() {
        bail!(
            "Benchmark file {} contains no entries",
            options.file.display()
        );
    }

    let unique_commits = deduplicate_benchmark_commits(&entries);
    let paths = prepare_benchmark_run_paths(
        &cwd,
        &options.file,
        &timestamp_tag,
        &sashiko_git_sha,
        options.db.as_deref(),
        options.output_dir.as_deref(),
        options.analyze_only,
    )?;

    let auto_preexisting = options
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().contains("preexisting"));
    let report_preexisting =
        options.report_preexisting || settings.linux_bug.enabled || auto_preexisting;

    configure_benchmark_settings(&mut settings, &options, &paths, report_preexisting)?;

    info!(
        "Benchmark run {} using preserved database {}",
        paths.run_id,
        paths.db_path.display()
    );
    let db = Arc::new(
        Database::new(&settings.database)
            .await
            .with_context(|| format!("Failed to open database at {}", paths.db_path.display()))?,
    );
    db.migrate().await?;
    db.ensure_project_stamp(options.project).await?;

    if options.project.uses_maintainers() && crate::maintainers::get_global_maintainers().is_none()
    {
        let repo_path = PathBuf::from(&settings.git.repository_path);
        if let Ok(idx) = crate::maintainers::MaintainersIndex::from_top_of_trunk(&repo_path) {
            crate::maintainers::init_global_maintainers(Arc::new(idx));
        }
    }

    if !options.analyze_only {
        ingest_benchmark_commits(&db, &settings, options.repo.as_deref(), &unique_commits).await?;
        run_benchmark_reviews(
            &db,
            &settings,
            &options,
            &paths,
            report_preexisting,
            &unique_commits,
        )
        .await?;
    }

    info!("Evaluating {} benchmark entries...", entries.len());
    let commit_map = load_all_commit_review_data(&db, &unique_commits).await?;
    let ai_provider = create_provider(&settings).context("Failed to create AI provider")?;
    let ai_provider: Arc<dyn AiProvider> =
        Arc::new(crate::ai::backoff_provider::BackoffProvider::new(
            ai_provider,
            Arc::new(crate::ai::quota::QuotaManager::with_limits(
                settings.ai.max_rps,
                settings.ai.max_tpm,
            )),
            None,
        ));
    let results = evaluate_benchmark_entries(
        &entries,
        &commit_map,
        ai_provider,
        settings.review.concurrency,
    )
    .await;

    let finished_at = chrono::Utc::now().to_rfc3339();
    let wall_clock_secs = started_wall.elapsed().as_secs();

    let metadata = BenchmarkRunMetadata {
        run_id: paths.run_id.clone(),
        started_at,
        finished_at,
        wall_clock_secs,
        sashiko_version: env!("CARGO_PKG_VERSION").to_string(),
        sashiko_git_sha,
        sashiko_git_dirty,
        prompts_git_hash: crate::prompt_bundle::PROMPT_BUNDLE_REVISION.to_string(),
        benchmark_file: options.file.display().to_string(),
        total_entries: entries.len(),
        unique_commits: unique_commits.len(),
        project: options.project.as_str().to_string(),
        ai_provider: settings.ai.provider.clone(),
        model: settings.ai.model.clone(),
        effort: settings.effective_effort().map(|e| e.as_str().to_string()),
        concurrency: settings.review.concurrency,
        report_preexisting,
        analyze_only: options.analyze_only,
        database_path: paths.db_path.display().to_string(),
        results_json_path: paths.results_json_path.display().to_string(),
        summary_md_path: paths.summary_md_path.display().to_string(),
    };

    let summary = build_benchmark_summary(&entries, &results, &commit_map);
    let report = BenchmarkRunReport {
        metadata,
        summary,
        results,
    };

    write_benchmark_artifacts(&report, &paths)?;
    Ok(report)
}

fn configure_benchmark_settings(
    settings: &mut Settings,
    options: &BenchmarkOptions,
    paths: &BenchmarkRunPaths,
    report_preexisting: bool,
) -> Result<()> {
    settings.project.kind = Some(options.project);
    settings.database.url = paths.db_path.to_string_lossy().into_owned();
    settings.database.token = String::new();
    settings.review.worktree_dir = Some(paths.worktree_dir.to_string_lossy().into_owned());
    if let Some(concurrency) = options.concurrency {
        settings.review.concurrency = concurrency.max(1);
    }
    settings.linux_bug.enabled = report_preexisting;
    // Outbound notifications are never sent during a benchmark run.
    settings.smtp = None;
    settings.forge.enabled = false;

    if !options.analyze_only {
        if let Some(repo) = options.repo.as_deref()
            && Path::new(repo).exists()
        {
            settings.git.repository_path = repo.to_string();
        }
        if settings.git.repository_path.trim().is_empty() {
            bail!(
                "[git] repository_path must be set in Settings.toml or provided via --repo <path> \
                 to run benchmark reviews"
            );
        }
    }
    Ok(())
}

async fn commit_has_completed_review(db: &Database, commit: &str) -> Result<bool> {
    let bracketed = format!("<{commit}>");
    let mut rows = db
        .conn
        .query(
            "SELECT 1
             FROM patches p
             JOIN patchsets ps ON ps.id = p.patchset_id
             JOIN reviews r ON r.patch_id = p.id
             WHERE p.message_id IN (?1, ?2)
               AND ps.status = 'Reviewed'
               AND r.status = 'Reviewed'
             LIMIT 1",
            libsql::params![commit.to_string(), bracketed],
        )
        .await?;
    if rows.next().await?.is_some() {
        return Ok(true);
    }
    if commit.len() < 40 {
        let root_msg_id = format!("{commit}@sashiko.local");
        let mut fallback_rows = db
            .conn
            .query(
                "SELECT 1
                 FROM patchsets ps
                 JOIN patches p ON p.patchset_id = ps.id
                 JOIN reviews r ON r.patch_id = p.id
                 WHERE ps.cover_letter_message_id = ?1
                   AND ps.status = 'Reviewed'
                   AND r.status = 'Reviewed'
                 LIMIT 1",
                libsql::params![root_msg_id],
            )
            .await?;
        if fallback_rows.next().await?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn ingest_benchmark_commits(
    db: &Database,
    settings: &Settings,
    repo_override: Option<&str>,
    unique_commits: &[String],
) -> Result<()> {
    info!(
        "--- Phase 1: Ingesting {} unique commits ---",
        unique_commits.len()
    );
    let mut to_fetch = Vec::new();
    for sha in unique_commits {
        if commit_has_completed_review(db, sha).await.unwrap_or(false) {
            info!(
                "Commit {} already has a completed review in benchmark DB, skipping",
                sha
            );
            continue;
        }
        to_fetch.push(sha.clone());
    }
    if to_fetch.is_empty() {
        return Ok(());
    }

    let repo_url = Some(repo_override.unwrap_or(DEFAULT_KERNEL_REPO_URL).to_string());
    let repo_path = PathBuf::from(&settings.git.repository_path);
    let (tx, mut rx) = mpsc::channel::<Event>(to_fetch.len().saturating_mul(2).max(32));
    let (mut fetch_agent, _fetch_req_tx) =
        FetchAgent::new(repo_path, tx, settings.forge.api_token.clone());

    fetch_agent
        .fetch_and_submit_batch(repo_url, to_fetch.clone())
        .await;
    drop(fetch_agent);

    let mut ingested = 0usize;
    let mut failures = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            Event::PatchSubmitted {
                article_id,
                message_id,
                subject,
                author,
                message,
                diff,
                base_commit,
                timestamp,
                index,
                total,
                ..
            } => {
                persist_benchmark_patch(
                    db,
                    &article_id,
                    &message_id,
                    &subject,
                    &author,
                    &message,
                    &diff,
                    base_commit.as_deref(),
                    timestamp,
                    index,
                    total,
                )
                .await?;
                ingested += 1;
            }
            Event::IngestionFailed {
                article_id, error, ..
            } => {
                error!("Failed to ingest {}: {}", article_id, error);
                failures.push((article_id, error));
            }
            _ => {}
        }
    }

    if !failures.is_empty() {
        bail!(
            "{} of {} benchmark commits failed to ingest. First failure: {} ({})",
            failures.len(),
            to_fetch.len(),
            failures[0].0,
            failures[0].1
        );
    }

    info!("Successfully ingested {} unique commits.", ingested);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn persist_benchmark_patch(
    db: &Database,
    article_id: &str,
    message_id: &str,
    subject: &str,
    author: &str,
    body: &str,
    diff: &str,
    base_commit: Option<&str>,
    timestamp: i64,
    index: u32,
    total: u32,
) -> Result<()> {
    let root_msg_id = format!("{article_id}@sashiko.local");
    let baseline_id = match base_commit.and_then(CommitId::parse) {
        Some(commit) => Some(
            db.create_baseline(None, None, Some(commit.as_str()))
                .await?,
        ),
        None => None,
    };

    let thread_id = db
        .ensure_thread_for_message(&root_msg_id, timestamp)
        .await?;
    let in_reply_to = if total == 1 {
        None
    } else {
        Some(root_msg_id.as_str())
    };
    db.create_message_with_references(
        message_id,
        thread_id,
        in_reply_to,
        author,
        subject,
        timestamp,
        body,
        "submitted",
        "",
        None,
        Some("git-fetch"),
        Some(&root_msg_id),
    )
    .await?;

    let sid = db.ensure_subsystem("from git", "git-import").await?;
    let maintainer_sections: Vec<AttributedSubsystem> = crate::maintainers::sections_for_diff(diff)
        .into_iter()
        .map(AttributedSubsystem::from_maintainers)
        .collect();

    let patchset_id = db
        .create_patchset(
            thread_id,
            Some(&root_msg_id),
            message_id,
            subject,
            author,
            timestamp,
            total,
            2,
            "submitted",
            "",
            None,
            index,
            baseline_id,
            true,
            None,
            None,
        )
        .await?
        .ok_or_else(|| anyhow!("Failed to create patchset for commit {}", message_id))?;

    let _ = db.add_subsystem_to_patchset(patchset_id, sid).await;
    let _ = db
        .add_patchset_maintainer_sections(patchset_id, &maintainer_sections)
        .await;

    let git_patch_id = crate::prerequisites::calculate_git_patch_id(diff)
        .await
        .ok()
        .flatten();
    let patch_id = db
        .create_patch_with_git_patch_id(
            patchset_id,
            message_id,
            index,
            diff,
            git_patch_id.as_deref(),
        )
        .await?;
    let _ = db.add_subsystem_to_patch(patch_id, sid).await;

    // Reset non-Reviewed prior attempts when re-running against an existing DB
    // so Reviewer picks up and re-reviews previously Failed/Cancelled commits.
    let mut rev_rows = db
        .conn
        .query(
            "SELECT 1 FROM reviews WHERE patch_id = ? AND status = 'Reviewed' LIMIT 1",
            libsql::params![patch_id],
        )
        .await?;
    let has_completed_review = rev_rows.next().await?.is_some();
    drop(rev_rows);

    db.execute(
        "DELETE FROM findings WHERE review_id IN (SELECT id FROM reviews WHERE patch_id = ? AND status != 'Reviewed')",
        libsql::params![patch_id],
    )
    .await?;
    db.execute(
        "DELETE FROM tool_usages WHERE review_id IN (SELECT id FROM reviews WHERE patch_id = ? AND status != 'Reviewed')",
        libsql::params![patch_id],
    )
    .await?;
    db.execute(
        "DELETE FROM reviews WHERE patch_id = ? AND status != 'Reviewed'",
        libsql::params![patch_id],
    )
    .await?;

    if has_completed_review {
        db.execute(
            "UPDATE patches SET status = 'Reviewed', apply_error = NULL WHERE id = ? AND COALESCE(status, '') != 'Reviewed'",
            libsql::params![patch_id],
        )
        .await?;
        db.execute(
            "UPDATE patchsets SET status = 'Reviewed', failed_reason = NULL WHERE id = ? AND COALESCE(status, '') != 'Reviewed'",
            libsql::params![patchset_id],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE patches SET status = 'Pending', apply_error = NULL WHERE id = ?",
            libsql::params![patch_id],
        )
        .await?;
        db.execute(
            "UPDATE patchsets SET status = 'Pending', failed_reason = NULL WHERE id = ?",
            libsql::params![patchset_id],
        )
        .await?;
    }
    Ok(())
}

struct AbortTaskOnDrop(tokio::task::AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn run_benchmark_reviews(
    db: &Arc<Database>,
    settings: &Settings,
    options: &BenchmarkOptions,
    paths: &BenchmarkRunPaths,
    report_preexisting: bool,
    unique_commits: &[String],
) -> Result<()> {
    info!(
        "--- Phase 2: Reviewing {} unique commits (concurrency: {}) ---",
        unique_commits.len(),
        settings.review.concurrency
    );
    let worker_overrides = WorkerSubprocessOverrides {
        prompts: Some(options.prompts.clone()),
        settings_path: options.settings_path.clone(),
        report_preexisting,
    };
    let reviewer = Arc::new(
        Reviewer::new(db.clone(), settings.clone())
            .await
            .with_worker_overrides(worker_overrides),
    );
    let reviewer_task = {
        let reviewer = reviewer.clone();
        tokio::spawn(async move {
            reviewer.start().await;
        })
    };
    let _abort_guard = AbortTaskOnDrop(reviewer_task.abort_handle());

    let mut last_logged = (usize::MAX, usize::MAX);
    loop {
        let (completed, pending) = count_commit_review_progress(db, unique_commits).await?;
        if completed >= unique_commits.len() {
            info!("All {} unique commits have been reviewed.", completed);
            break;
        }
        if (completed, pending) != last_logged {
            info!(
                "Review progress: {}/{} completed ({} in flight / pending)",
                completed,
                unique_commits.len(),
                pending
            );
            last_logged = (completed, pending);
        }
        sleep(Duration::from_secs(2)).await;
    }

    reviewer_task.abort();
    let _ = reviewer_task.await;

    if paths.worktree_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&paths.worktree_dir)
    {
        warn!(
            "Failed to remove temporary benchmark worktree dir {:?}: {}",
            paths.worktree_dir, e
        );
    }
    Ok(())
}

async fn lookup_benchmark_patch_row(
    db: &Database,
    commit: &str,
) -> Result<Option<(i64, i64, Option<String>, Option<String>, Option<String>)>> {
    let bracketed = format!("<{commit}>");
    let mut rows = db
        .conn
        .query(
            "SELECT p.id, p.patchset_id, p.status, ps.status, m.subject
             FROM patches p
             JOIN patchsets ps ON ps.id = p.patchset_id
             LEFT JOIN messages m ON m.message_id = p.message_id
             WHERE p.message_id IN (?1, ?2)
             ORDER BY p.id DESC LIMIT 1",
            libsql::params![commit.to_string(), bracketed],
        )
        .await?;
    if let Some(row) = rows.next().await? {
        return Ok(Some((
            row.get(0).unwrap_or_default(),
            row.get(1).unwrap_or_default(),
            row.get(2).ok(),
            row.get(3).ok(),
            row.get(4).ok(),
        )));
    }
    drop(rows);

    if commit.len() < 40 {
        let root_msg_id = format!("{commit}@sashiko.local");
        let mut fallback_rows = db
            .conn
            .query(
                "SELECT p.id, p.patchset_id, p.status, ps.status, m.subject
                 FROM patchsets ps
                 JOIN patches p ON p.patchset_id = ps.id
                 LEFT JOIN messages m ON m.message_id = p.message_id
                 WHERE ps.cover_letter_message_id = ?1
                 ORDER BY p.id DESC LIMIT 1",
                libsql::params![root_msg_id],
            )
            .await?;
        if let Some(row) = fallback_rows.next().await? {
            return Ok(Some((
                row.get(0).unwrap_or_default(),
                row.get(1).unwrap_or_default(),
                row.get(2).ok(),
                row.get(3).ok(),
                row.get(4).ok(),
            )));
        }
    }

    Ok(None)
}

async fn count_commit_review_progress(
    db: &Database,
    unique_commits: &[String],
) -> Result<(usize, usize)> {
    let mut completed = 0usize;
    let mut pending = 0usize;

    for commit in unique_commits {
        let Some((patch_id, _patchset_id, patch_status_opt, patchset_status_opt, _subject)) =
            lookup_benchmark_patch_row(db, commit).await?
        else {
            pending += 1;
            continue;
        };
        let patch_status = patch_status_opt.unwrap_or_default();
        let patchset_status = patchset_status_opt.unwrap_or_default();

        // A patchset is only complete once Reviewer::review_patchset_task has
        // finished all retries, cleaned up its worktree, and updated
        // patchsets.status out of Pending / In Review / Fetching.
        let patchset_terminal = !matches!(
            patchset_status.as_str(),
            "" | "Pending" | "In Review" | "Fetching"
        );
        if !patchset_terminal {
            pending += 1;
            continue;
        }

        let mut rev_rows = db
            .conn
            .query(
                "SELECT status FROM reviews WHERE patch_id = ?
                 ORDER BY CASE WHEN status = 'Reviewed' THEN 0 ELSE 1 END ASC, id DESC LIMIT 1",
                libsql::params![patch_id],
            )
            .await?;

        if let Some(rev_row) = rev_rows.next().await? {
            let rev_status: String = rev_row.get(0).unwrap_or_default();
            if rev_status == "Pending" || rev_status == "In Review" {
                pending += 1;
            } else {
                completed += 1;
            }
        } else if matches!(
            patch_status.as_str(),
            "Skipped" | "Failed" | "Failed To Apply" | "Cancelled"
        ) || matches!(
            patchset_status.as_str(),
            "Skipped" | "Failed" | "Failed To Apply" | "Cancelled" | "Reviewed" | "Incomplete"
        ) {
            completed += 1;
        } else {
            pending += 1;
        }
    }

    Ok((completed, pending))
}

async fn load_all_commit_review_data(
    db: &Database,
    unique_commits: &[String],
) -> Result<HashMap<String, CommitReviewData>> {
    let mut map = HashMap::with_capacity(unique_commits.len());
    for commit in unique_commits {
        let data = load_commit_review_data(db, commit).await?;
        map.insert(commit.clone(), data);
    }
    Ok(map)
}

pub(crate) async fn load_commit_review_data(
    db: &Database,
    commit: &str,
) -> Result<CommitReviewData> {
    let mut data = CommitReviewData::default();

    let Some((patch_id, patchset_id, patch_status, patchset_status, subject)) =
        lookup_benchmark_patch_row(db, commit).await?
    else {
        return Ok(data);
    };

    data.patch_id = Some(patch_id);
    data.patchset_id = Some(patchset_id);
    data.subject = subject;
    data.review_status = match patch_status.as_deref() {
        Some(s) if !matches!(s, "" | "Pending" | "In Review") => patch_status,
        _ => patchset_status.or(patch_status),
    };

    let mut rev_rows = db
        .conn
        .query(
            "SELECT id, status, summary, result_description, interaction_id, created_at
             FROM reviews
             WHERE patch_id = ?
             ORDER BY CASE WHEN status = 'Reviewed' THEN 0 ELSE 1 END ASC, id DESC LIMIT 1",
            libsql::params![patch_id],
        )
        .await?;

    let Some(rev_row) = rev_rows.next().await? else {
        return Ok(data);
    };

    let review_id: i64 = rev_row.get(0).unwrap_or_default();
    let review_status: String = rev_row.get(1).unwrap_or_default();
    let summary: Option<String> = rev_row.get(2).ok();
    let result_desc: Option<String> = rev_row.get(3).ok();
    let interaction_id: Option<String> = rev_row.get(4).ok();
    let review_created_at: Option<i64> = rev_row.get(5).ok();

    data.review_id = Some(review_id);
    if !matches!(review_status.as_str(), "" | "Pending" | "In Review")
        || data.review_status.is_none()
    {
        data.review_status = Some(review_status);
    }
    data.summary_text = format!(
        "{}\n{}",
        summary.unwrap_or_default(),
        result_desc.unwrap_or_default()
    )
    .trim()
    .to_string();

    let mut parsed_output: Option<Value> = None;
    if let Some(iid) = interaction_id {
        let mut int_rows = db
            .conn
            .query(
                "SELECT tokens_in, tokens_out, tokens_cached, created_at, output_raw
                 FROM ai_interactions
                 WHERE id = ?",
                libsql::params![iid],
            )
            .await?;
        if let Some(int_row) = int_rows.next().await? {
            data.tokens_in = int_row.get::<i64>(0).unwrap_or(0).max(0) as u32;
            data.tokens_out = int_row.get::<i64>(1).unwrap_or(0).max(0) as u32;
            data.tokens_cached = int_row.get::<i64>(2).unwrap_or(0).max(0) as u32;
            let int_created_at = int_row.get::<i64>(3).unwrap_or(0);
            if let Some(start_time) = review_created_at
                && int_created_at >= start_time
            {
                data.duration_secs = (int_created_at - start_time) as u64;
            }
            if let Ok(Some(raw_str)) = crate::compression::get_compressed_string_opt(&int_row, 4)
                && let Ok(val) = serde_json::from_str::<Value>(&raw_str)
            {
                parsed_output = Some(val);
            }
        }
    }

    let mut tool_rows = db
        .conn
        .query(
            "SELECT COUNT(*) FROM tool_usages WHERE review_id = ?",
            libsql::params![review_id],
        )
        .await?;
    if let Some(tool_row) = tool_rows.next().await? {
        let tool_count = tool_row.get::<i64>(0).unwrap_or(0).max(0) as u32;
        data.turns = 1 + tool_count;
    } else {
        data.turns = 1;
    }

    if let Some(ref output) = parsed_output {
        populate_findings_and_stages_from_output(&mut data, output);
    }

    if data.findings.is_empty() {
        load_findings_table_fallback(db, review_id, &mut data.findings).await?;
    }

    // Always merge pre-existing bugs linked to this review from the bugs table,
    // even when patch findings were already loaded from output_raw or findings.
    load_bugs_for_review(db, review_id, &mut data.findings).await?;

    if let Some(ref output) = parsed_output {
        populate_dismissed_from_output(&mut data, output);
    }

    if data.turns <= 1 && !data.stages.is_empty() {
        let stage_turns: u32 = data.stages.iter().map(|s| s.turns).sum();
        if stage_turns > 0 {
            data.turns = stage_turns;
        }
    }

    Ok(data)
}

fn populate_findings_and_stages_from_output(data: &mut CommitReviewData, output: &Value) {
    if let Some(c) = output.get("concerns_count").and_then(Value::as_u64) {
        data.concerns_count = c as usize;
    }
    if let Some(d) = output
        .get("dismissed_concerns_count")
        .and_then(Value::as_u64)
    {
        data.dismissed_concerns_count = d as usize;
    }

    if let Some(review_map) = output.get("review_map") {
        if let Some(guides) = review_map
            .get("prompts")
            .and_then(|p| p.get("pre_screen"))
            .and_then(Value::as_array)
        {
            data.selected_guides = guides
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }

        if let Some(stages) = review_map.get("stages").and_then(Value::as_array) {
            for s in stages {
                let name = s
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let skipped = s.get("skipped").and_then(Value::as_bool).unwrap_or(false);
                let turns = s.get("turns").and_then(Value::as_u64).unwrap_or(0) as u32;
                let tokens_in = s.get("tokens_in").and_then(Value::as_u64).unwrap_or(0) as u32;
                let tokens_out = s.get("tokens_out").and_then(Value::as_u64).unwrap_or(0) as u32;
                let tokens_cached =
                    s.get("tokens_cached").and_then(Value::as_u64).unwrap_or(0) as u32;
                let concerns_count =
                    s.get("concerns_count").and_then(Value::as_u64).unwrap_or(0) as usize;
                let dismissed_count = s
                    .get("dismissed_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;

                if !skipped
                    && !matches!(
                        name.as_str(),
                        "pre-screen" | "planning" | "verification" | "report" | "summary"
                    )
                    && !name.starts_with("post-verification")
                {
                    data.planned_stages.push(name.clone());
                }

                data.stages.push(BenchmarkStageMetric {
                    name,
                    status: if skipped {
                        "skipped".to_string()
                    } else {
                        "executed".to_string()
                    },
                    turns,
                    tokens_in,
                    tokens_out,
                    tokens_cached,
                    concerns_count,
                    dismissed_count,
                });
            }
        }
    }

    if let Some(findings_arr) = output.get("findings").and_then(Value::as_array) {
        for f in findings_arr {
            let problem = f
                .get("problem")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if problem.is_empty() {
                continue;
            }
            let severity = f
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("Low")
                .to_string();
            let explanation = f
                .get("severity_explanation")
                .or_else(|| f.get("reasoning"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let preexisting = f
                .get("preexisting")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let origin = f
                .get("origin")
                .or_else(|| f.get("post_verification_stage"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let stages = extract_str_vec(f.get("stages"));
            let prompts = extract_str_vec(f.get("prompts"));

            let next_idx = data.findings.len() + 1;
            data.findings.push(BenchmarkFindingRecord {
                index: next_idx,
                severity,
                problem,
                explanation,
                preexisting,
                origin,
                stages,
                prompts,
            });
        }
    }
}

fn populate_dismissed_from_output(data: &mut CommitReviewData, output: &Value) {
    if let Some(dismissed_arr) = output.get("dismissed_concerns").and_then(Value::as_array) {
        for d in dismissed_arr {
            push_dismissed_item(&mut data.dismissed_concerns, d, None, false);
        }
    }

    if let Some(concerns_arr) = output.get("concerns").and_then(Value::as_array) {
        let finding_problems: BTreeSet<String> = data
            .findings
            .iter()
            .map(|f| f.problem.trim().to_ascii_lowercase())
            .collect();
        for c in concerns_arr {
            let desc = c
                .get("description")
                .or_else(|| c.get("problem"))
                .or_else(|| c.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !desc.is_empty() && finding_problems.contains(&desc) {
                continue;
            }
            push_dismissed_item(
                &mut data.dismissed_concerns,
                c,
                Some("preexisting-filtered"),
                true,
            );
        }
    }

    if data.dismissed_concerns_count == 0 {
        data.dismissed_concerns_count = data.dismissed_concerns.len();
    }
}

fn push_dismissed_item(
    out: &mut Vec<BenchmarkDismissedRecord>,
    item: &Value,
    default_stage: Option<&str>,
    force_default_stage: bool,
) {
    let description = item
        .get("description")
        .or_else(|| item.get("problem"))
        .or_else(|| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let reasoning = item
        .get("reasoning")
        .or_else(|| item.get("dismissal_reasoning"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if description.is_empty() && reasoning.is_empty() {
        return;
    }

    let stage = if force_default_stage {
        default_stage.unwrap_or("verification").to_string()
    } else {
        item.get("post_verification_stage")
            .or_else(|| item.get("origin"))
            .or_else(|| item.get("stage"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| extract_str_vec(item.get("stages")).into_iter().next())
            .or_else(|| default_stage.map(str::to_string))
            .unwrap_or_else(|| "verification".to_string())
    };

    let next_index = out.len() + 1;
    out.push(BenchmarkDismissedRecord {
        index: next_index,
        stage,
        description,
        reasoning,
    });
}

fn extract_str_vec(val: Option<&Value>) -> Vec<String> {
    val.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn load_findings_table_fallback(
    db: &Database,
    review_id: i64,
    findings: &mut Vec<BenchmarkFindingRecord>,
) -> Result<()> {
    let mut rows = db
        .conn
        .query(
            "SELECT problem, severity, severity_explanation, preexisting
             FROM findings
             WHERE review_id = ?
             ORDER BY id ASC",
            libsql::params![review_id],
        )
        .await?;

    let mut seen_problems: BTreeSet<String> = findings
        .iter()
        .map(|f| f.problem.trim().to_ascii_lowercase())
        .collect();
    while let Some(row) = rows.next().await? {
        let problem: String = row.get(0).unwrap_or_default();
        if problem.trim().is_empty() || !seen_problems.insert(problem.trim().to_ascii_lowercase()) {
            continue;
        }
        let sev_int: i32 = row.get(1).unwrap_or(1);
        let severity = match sev_int {
            4 => Severity::Critical,
            3 => Severity::High,
            2 => Severity::Medium,
            _ => Severity::Low,
        }
        .as_str()
        .to_string();
        let explanation: Option<String> = row.get(2).ok();
        let preexisting = row.get::<Option<bool>>(3).ok().flatten().unwrap_or(false);
        let next_idx = findings.len() + 1;
        findings.push(BenchmarkFindingRecord {
            index: next_idx,
            severity,
            problem,
            explanation,
            preexisting,
            origin: None,
            stages: Vec::new(),
            prompts: Vec::new(),
        });
    }

    Ok(())
}

async fn load_bugs_for_review(
    db: &Database,
    review_id: i64,
    findings: &mut Vec<BenchmarkFindingRecord>,
) -> Result<()> {
    let mut seen_problems: BTreeSet<String> = findings
        .iter()
        .map(|f| f.problem.trim().to_ascii_lowercase())
        .collect();

    if let Ok(bugs) = db.list_bugs_for_review(review_id).await {
        for (bug, _) in bugs {
            let problem = bug.problem().trim().to_string();
            if problem.is_empty() || !seen_problems.insert(problem.to_ascii_lowercase()) {
                continue;
            }
            let next_idx = findings.len() + 1;
            findings.push(BenchmarkFindingRecord {
                index: next_idx,
                severity: bug.severity().as_str().to_string(),
                problem,
                explanation: bug.severity_explanation(),
                preexisting: true,
                origin: Some("bug_repository".to_string()),
                stages: Vec::new(),
                prompts: Vec::new(),
            });
        }
    }

    Ok(())
}

async fn evaluate_benchmark_entries(
    entries: &[BenchmarkEntry],
    commit_map: &HashMap<String, CommitReviewData>,
    ai_provider: Arc<dyn AiProvider>,
    concurrency: usize,
) -> Vec<BenchmarkEntryResult> {
    let total_entries = entries.len();
    let processed_count = Arc::new(AtomicUsize::new(0));

    futures_util::stream::iter(entries.iter().cloned().enumerate())
        .map(|(idx, entry)| {
            let client = ai_provider.clone();
            let commit_data = commit_map.get(entry.commit.trim()).cloned();
            let processed_count = processed_count.clone();
            async move {
                let res = evaluate_single_entry(client, entry, commit_data).await;
                let current = processed_count.fetch_add(1, Ordering::Relaxed) + 1;
                if current.is_multiple_of(5) || current == total_entries {
                    info!("Evaluation progress: {}/{}", current, total_entries);
                }
                (idx, res)
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .fold(vec![None; total_entries], |mut acc, (idx, item)| {
            acc[idx] = Some(item);
            acc
        })
        .into_iter()
        .flatten()
        .collect()
}

pub(crate) async fn evaluate_single_entry(
    client: Arc<dyn AiProvider>,
    entry: BenchmarkEntry,
    commit_data: Option<CommitReviewData>,
) -> BenchmarkEntryResult {
    let Some(problem_description) = entry
        .problem_description
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return empty_entry_result(
            &entry,
            "",
            "SKIPPED",
            "No problem description provided",
            None,
        );
    };

    let Some(data) = commit_data else {
        return empty_entry_result(
            &entry,
            &problem_description,
            "NOT_FOUND_IN_DB",
            "Patch not found in database.",
            Some("review_failed"),
        );
    };

    if data.patch_id.is_none() {
        return empty_entry_result(
            &entry,
            &problem_description,
            "NOT_FOUND_IN_DB",
            "Patch not found in database.",
            Some("review_failed"),
        );
    }

    if !data.is_completed_review() {
        return build_entry_result_from_data(
            &entry,
            &problem_description,
            &data,
            "NOT_REVIEWED".to_string(),
            format!(
                "Patch has no completed review (status: {}).",
                data.review_status.as_deref().unwrap_or("None")
            ),
            None,
            Some("review_failed".to_string()),
            None,
        );
    }

    let prompt = build_judge_prompt(&problem_description, &data);
    let mut session = BenchmarkEvalSession::new(prompt);
    let runner = SessionRunner::new(client.as_ref());

    match runner.run(&mut session).await {
        Ok(run_res) => {
            let verdict = run_res.output;
            let found = verdict.status == "DETECTED" || verdict.status == "PARTIALLY_DETECTED";
            let matched_finding = if found {
                verdict
                    .matched_finding_index
                    .and_then(|idx| data.findings.iter().find(|f| f.index == idx))
                    .or_else(|| {
                        if verdict.matched_finding_index.is_none() && data.findings.len() == 1 {
                            data.findings.first()
                        } else {
                            None
                        }
                    })
                    .map(|f| MatchedFindingSummary {
                        index: f.index,
                        severity: f.severity.clone(),
                        problem: f.problem.clone(),
                        preexisting: f.preexisting,
                        origin: f.origin.clone(),
                        stages: f.stages.clone(),
                        prompts: f.prompts.clone(),
                    })
            } else {
                None
            };

            let (miss_reason, dismissed_by_stage) = if verdict.status == "MISSED" {
                let reason = verdict
                    .miss_reason
                    .unwrap_or_else(|| "not_raised".to_string());
                (Some(reason), verdict.dismissed_by_stage)
            } else {
                (None, None)
            };

            build_entry_result_from_data(
                &entry,
                &problem_description,
                &data,
                verdict.status,
                verdict.explanation,
                matched_finding,
                miss_reason,
                dismissed_by_stage,
            )
        }
        Err(e) => build_entry_result_from_data(
            &entry,
            &problem_description,
            &data,
            "UNKNOWN".to_string(),
            format!("Evaluation failed: {e}"),
            None,
            None,
            None,
        ),
    }
}

fn empty_entry_result(
    entry: &BenchmarkEntry,
    problem_description: &str,
    status: &str,
    explanation: &str,
    miss_reason: Option<&str>,
) -> BenchmarkEntryResult {
    BenchmarkEntryResult {
        commit: entry.commit.clone(),
        subject: None,
        fixed_by: entry.fixed_by.clone(),
        subsystem: entry.subsystem.clone(),
        problem_description: problem_description.to_string(),
        found: false,
        status: status.to_string(),
        explanation: explanation.to_string(),
        matched_finding: None,
        miss_reason: miss_reason.map(str::to_string),
        dismissed_by_stage: None,
        findings_count: 0,
        patch_findings_count: 0,
        preexisting_findings_count: 0,
        extra_findings_count: 0,
        concerns_count: 0,
        dismissed_concerns_count: 0,
        tokens_in: 0,
        tokens_out: 0,
        tokens_cached: 0,
        turns: 0,
        duration_secs: 0,
        review_id: None,
        patchset_id: None,
        patch_id: None,
        planned_stages: Vec::new(),
        selected_guides: Vec::new(),
        stages: Vec::new(),
        findings: Vec::new(),
        dismissed_concerns: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_entry_result_from_data(
    entry: &BenchmarkEntry,
    problem_description: &str,
    data: &CommitReviewData,
    status: String,
    explanation: String,
    matched_finding: Option<MatchedFindingSummary>,
    miss_reason: Option<String>,
    dismissed_by_stage: Option<String>,
) -> BenchmarkEntryResult {
    let found = status == "DETECTED" || status == "PARTIALLY_DETECTED";
    let patch_findings_count = data.patch_findings_count();
    let preexisting_findings_count = data.preexisting_findings_count();
    let findings_count = data.findings.len();
    let matched_patch_finding = matched_finding.as_ref().is_some_and(|mf| !mf.preexisting);
    let extra_findings_count = if matched_patch_finding {
        patch_findings_count.saturating_sub(1)
    } else {
        patch_findings_count
    };

    BenchmarkEntryResult {
        commit: entry.commit.clone(),
        subject: data.subject.clone(),
        fixed_by: entry.fixed_by.clone(),
        subsystem: entry.subsystem.clone(),
        problem_description: problem_description.to_string(),
        found,
        status,
        explanation,
        matched_finding,
        miss_reason,
        dismissed_by_stage,
        findings_count,
        patch_findings_count,
        preexisting_findings_count,
        extra_findings_count,
        concerns_count: data.concerns_count,
        dismissed_concerns_count: data.dismissed_concerns_count,
        tokens_in: data.tokens_in,
        tokens_out: data.tokens_out,
        tokens_cached: data.tokens_cached,
        turns: data.turns,
        duration_secs: data.duration_secs,
        review_id: data.review_id,
        patchset_id: data.patchset_id,
        patch_id: data.patch_id,
        planned_stages: data.planned_stages.clone(),
        selected_guides: data.selected_guides.clone(),
        stages: data.stages.clone(),
        findings: data.findings.clone(),
        dismissed_concerns: data.dismissed_concerns.clone(),
    }
}

pub(crate) fn build_judge_prompt(problem_description: &str, data: &CommitReviewData) -> String {
    let mut findings_block = String::new();
    if data.findings.is_empty() {
        findings_block.push_str("(No findings reported)\n");
    } else {
        for f in &data.findings {
            let stages_label = if f.stages.is_empty() {
                "unknown".to_string()
            } else {
                f.stages.join(", ")
            };
            findings_block.push_str(&format!(
                "[F{}] [Severity: {}] [Preexisting: {}] [Stages: {}] {}\n",
                f.index, f.severity, f.preexisting, stages_label, f.problem
            ));
            if let Some(ref expl) = f.explanation {
                findings_block.push_str(&format!("     Explanation: {}\n", expl.trim()));
            }
        }
    }

    let mut dismissed_block = String::new();
    if data.dismissed_concerns.is_empty() {
        dismissed_block.push_str("(No dismissed concerns recorded)\n");
    } else {
        for d in &data.dismissed_concerns {
            dismissed_block.push_str(&format!(
                "[D{}] [Stage: {}] {}\n     Dismissal Reasoning: {}\n",
                d.index, d.stage, d.description, d.reasoning
            ));
        }
    }

    format!(
        "You are evaluating an automated Linux kernel code review tool against a known ground-truth bug.\n\n\
        Ground Truth Bug Description:\n\
        {problem_description}\n\n\
        Reported Findings:\n\
        {findings_block}\n\
        Review Summary:\n\
        {}\n\n\
        Dismissed / Filtered Candidate Concerns:\n\
        {dismissed_block}\n\
        Task:\n\
        1. Determine whether ANY of the Reported Findings (`[F1]`, `[F2]`, ...) or the Review Summary describes the Ground Truth Bug:\n\
           - `\"DETECTED\"`: A reported finding accurately identifies the specific root cause and location/mechanism.\n\
           - `\"PARTIALLY_DETECTED\"`: A reported finding identifies the general defect or symptom in the right area with minor inaccuracy.\n\
           - `\"MISSED\"`: None of the reported findings match the ground truth bug.\n\
        2. When `status` is `\"DETECTED\"` or `\"PARTIALLY_DETECTED\"`, set `matched_finding_index` to the integer `1..=N` of the best matching `[F<index>]` finding (or `null` if matched only in the summary).\n\
        3. When `status` is `\"MISSED\"`, inspect the Dismissed / Filtered Candidate Concerns (`[D1]`, `[D2]`, ...) to diagnose why it was missed:\n\
           - Set `miss_reason` to `\"wrongly_dismissed\"` if a dismissed concern in `[D1..]` raised the ground truth bug and was later dismissed as a false positive, and set `dismissed_by_stage` to that `[D<index>]`'s `Stage`.\n\
           - Set `miss_reason` to `\"preexisting_filtered\"` if a concern with `Stage: preexisting-filtered` matched the ground truth bug.\n\
           - Set `miss_reason` to `\"not_raised\"` if no specialist stage or dismissed concern ever mentioned the ground truth bug.\n\n\
        Respond ONLY with a valid JSON object matching this schema:\n\
        {{\n\
          \"status\": \"DETECTED\" | \"PARTIALLY_DETECTED\" | \"MISSED\",\n\
          \"matched_finding_index\": <number or null>,\n\
          \"miss_reason\": \"not_raised\" | \"wrongly_dismissed\" | \"preexisting_filtered\" | null,\n\
          \"dismissed_by_stage\": <string or null>,\n\
          \"explanation\": \"<one-sentence explanation referencing [F#] or [D#] when applicable>\"\n\
        }}",
        if data.summary_text.is_empty() {
            "(empty)"
        } else {
            &data.summary_text
        }
    )
}

pub(crate) fn parse_judge_verdict(raw: &str) -> Result<BenchmarkJudgeVerdict, ValidationError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::FormatViolation(
            "Empty response from benchmark judge".to_string(),
        ));
    }

    let json_candidate = extract_json_object(trimmed).unwrap_or(trimmed);
    if let Ok(mut parsed) = serde_json::from_str::<BenchmarkJudgeVerdict>(json_candidate) {
        parsed.status = parsed.status.trim().to_ascii_uppercase();
        if !matches!(
            parsed.status.as_str(),
            "DETECTED" | "PARTIALLY_DETECTED" | "MISSED"
        ) {
            return Err(ValidationError::FormatViolation(format!(
                "Invalid status '{}' in judge JSON; expected DETECTED, PARTIALLY_DETECTED, or MISSED",
                parsed.status
            )));
        }
        return Ok(parsed);
    }

    // Fallback for plain-text responses `[DETECTED | PARTIALLY_DETECTED | MISSED]: explanation`
    let re_status = Regex::new(r"(?i)\b(DETECTED|PARTIALLY_DETECTED|MISSED)\b").unwrap();
    if let Some(cap) = re_status.captures(trimmed) {
        let status = cap[1].to_ascii_uppercase();
        let remaining = re_status.replace(trimmed, "").to_string();
        let explanation = remaining
            .trim()
            .trim_start_matches([':', '-', ' ', '\n', ']', '['])
            .to_string();
        let matched_finding_index = extract_finding_tag_index(&explanation);
        return Ok(BenchmarkJudgeVerdict {
            miss_reason: (status == "MISSED").then(|| "not_raised".to_string()),
            status,
            matched_finding_index,
            dismissed_by_stage: None,
            explanation,
        });
    }

    Err(ValidationError::FormatViolation(
        "Judge response was neither valid JSON nor contained DETECTED/PARTIALLY_DETECTED/MISSED"
            .to_string(),
    ))
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then_some(&text[start..=end])
}

fn extract_finding_tag_index(text: &str) -> Option<usize> {
    let re = Regex::new(r"\[F(\d+)\]").ok()?;
    let caps = re.captures(text)?;
    caps.get(1)?.as_str().parse::<usize>().ok()
}

struct BenchmarkEvalSession {
    prompt: String,
}

impl BenchmarkEvalSession {
    fn new(prompt: String) -> Self {
        Self { prompt }
    }
}

#[async_trait]
impl LlmSession for BenchmarkEvalSession {
    type Output = BenchmarkJudgeVerdict;

    fn system_prompt(&self) -> String {
        String::new()
    }

    fn initial_user_prompt(&self) -> String {
        self.prompt.clone()
    }

    fn tools(&self) -> Option<Vec<crate::ai::AiTool>> {
        None
    }

    fn temperature(&self) -> Option<f32> {
        Some(0.0)
    }

    fn context_tag(&self) -> Option<String> {
        Some("benchmark-judge".to_string())
    }

    async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
        bail!("BenchmarkEvalSession does not use tools")
    }

    fn validate(
        &mut self,
        response: &crate::ai::AiResponse,
    ) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        parse_judge_verdict(text)
    }

    fn handle_provider_error(
        &mut self,
        error: &anyhow::Error,
        _attempt: usize,
        has_active_tools: bool,
    ) -> crate::ai::ErrorAction {
        if crate::ai::session::is_malformed_tool_call_error(&error.to_string()) {
            crate::ai::ErrorAction::RetryWithFeedback(
                crate::ai::session::malformed_tool_call_feedback(has_active_tools),
            )
        } else {
            crate::ai::ErrorAction::Fail
        }
    }
}

/// Computes `total`, `mean`, `p50`, `p90`, and `max` over a slice of `u64` values.
pub fn compute_distribution(values: &[u64]) -> MetricDistribution {
    if values.is_empty() {
        return MetricDistribution::default();
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let total: u64 = sorted.iter().sum();
    let mean = round_one_decimal(total as f64 / sorted.len() as f64);
    let p50 = percentile_nearest_rank(&sorted, 50);
    let p90 = percentile_nearest_rank(&sorted, 90);
    let max = *sorted.last().unwrap_or(&0);
    MetricDistribution {
        total,
        mean,
        p50,
        p90,
        max,
    }
}

fn percentile_nearest_rank(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * pct) / 100;
    sorted[idx]
}

fn round_one_decimal(val: f64) -> f64 {
    (val * 10.0).round() / 10.0
}

pub(crate) fn build_benchmark_summary(
    entries: &[BenchmarkEntry],
    results: &[BenchmarkEntryResult],
    commit_map: &HashMap<String, CommitReviewData>,
) -> BenchmarkSummaryStats {
    let total_entries = results.len();
    let unique_commits_list = deduplicate_benchmark_commits(entries);
    let unique_commits = unique_commits_list.len();

    let mut detected = 0usize;
    let mut partially_detected = 0usize;
    let mut missed = 0usize;
    let mut not_reviewed = 0usize;
    let mut skipped = 0usize;

    let mut missed_not_raised = 0usize;
    let mut missed_wrongly_dismissed = 0usize;
    let mut missed_preexisting_filtered = 0usize;
    let mut missed_review_failed = 0usize;

    let mut detections_by_stage: BTreeMap<String, usize> = BTreeMap::new();
    let mut false_dismissals_by_stage: BTreeMap<String, usize> = BTreeMap::new();
    let mut subsystem_buckets: BTreeMap<String, (usize, usize, usize, usize)> = BTreeMap::new();
    let mut matched_findings_by_commit: HashMap<String, BTreeSet<usize>> = HashMap::new();
    let mut matched_patch_findings_by_commit: HashMap<String, BTreeSet<usize>> = HashMap::new();

    for res in results {
        let subsys = res
            .subsystem
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unspecified")
            .to_string();
        let bucket = subsystem_buckets.entry(subsys).or_insert((0, 0, 0, 0));

        match res.status.as_str() {
            "DETECTED" => {
                detected += 1;
                bucket.0 += 1;
                bucket.1 += 1;
            }
            "PARTIALLY_DETECTED" => {
                partially_detected += 1;
                bucket.0 += 1;
                bucket.2 += 1;
            }
            "MISSED" => {
                missed += 1;
                bucket.0 += 1;
                bucket.3 += 1;
                match res.miss_reason.as_deref() {
                    Some("wrongly_dismissed") => {
                        missed_wrongly_dismissed += 1;
                        let stage = res
                            .dismissed_by_stage
                            .as_deref()
                            .unwrap_or("verification")
                            .to_string();
                        *false_dismissals_by_stage.entry(stage).or_insert(0) += 1;
                    }
                    Some("preexisting_filtered") => missed_preexisting_filtered += 1,
                    Some("review_failed") => missed_review_failed += 1,
                    _ => missed_not_raised += 1,
                }
            }
            "NOT_REVIEWED" | "NOT_FOUND_IN_DB" => {
                not_reviewed += 1;
                missed_review_failed += 1;
            }
            "SKIPPED" => {
                skipped += 1;
            }
            _ => {}
        }

        if let Some(ref mf) = res.matched_finding {
            let commit_key = res.commit.trim().to_string();
            matched_findings_by_commit
                .entry(commit_key.clone())
                .or_default()
                .insert(mf.index);
            if !mf.preexisting {
                matched_patch_findings_by_commit
                    .entry(commit_key)
                    .or_default()
                    .insert(mf.index);
            }
            if mf.stages.is_empty() {
                if let Some(ref origin) = mf.origin {
                    *detections_by_stage.entry(origin.clone()).or_insert(0) += 1;
                } else {
                    *detections_by_stage
                        .entry("unspecified".to_string())
                        .or_insert(0) += 1;
                }
            } else {
                for stage in &mf.stages {
                    *detections_by_stage.entry(stage.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    let evaluated_entries = detected + partially_detected + missed;
    let exact_detection_rate_pct = if evaluated_entries > 0 {
        round_one_decimal((detected as f64 * 100.0) / evaluated_entries as f64)
    } else {
        0.0
    };
    let total_detection_rate_pct = if evaluated_entries > 0 {
        round_one_decimal(
            ((detected + partially_detected) as f64 * 100.0) / evaluated_entries as f64,
        )
    } else {
        0.0
    };

    // Deduplicated per-commit metrics
    let mut reviewed_commits = 0usize;
    let mut total_patch_findings = 0usize;
    let mut total_preexisting_findings = 0usize;
    let mut matched_findings = 0usize;
    let mut unmatched_extra_findings = 0usize;
    let mut clean_target_only_commits = 0usize;
    let mut findings_by_severity = SeverityCounts::default();
    let mut total_concerns_raised = 0usize;
    let mut total_concerns_dismissed = 0usize;

    let mut tokens_in_vec = Vec::new();
    let mut tokens_out_vec = Vec::new();
    let mut tokens_cached_vec = Vec::new();
    let mut turns_vec = Vec::new();
    let mut duration_vec = Vec::new();

    for sha in &unique_commits_list {
        let Some(data) = commit_map.get(sha) else {
            continue;
        };
        if !data.is_completed_review() {
            continue;
        }
        reviewed_commits += 1;
        let patch_f = data.patch_findings_count();
        let pre_f = data.preexisting_findings_count();
        total_patch_findings += patch_f;
        total_preexisting_findings += pre_f;
        total_concerns_raised += data.concerns_count;
        total_concerns_dismissed += data.dismissed_concerns_count;

        for f in &data.findings {
            match f.severity.to_ascii_lowercase().as_str() {
                "critical" => findings_by_severity.critical += 1,
                "high" => findings_by_severity.high += 1,
                "medium" => findings_by_severity.medium += 1,
                _ => findings_by_severity.low += 1,
            }
        }

        let matched_for_commit = matched_findings_by_commit
            .get(sha)
            .map(|s| s.len())
            .unwrap_or(0);
        let matched_patch_for_commit = matched_patch_findings_by_commit
            .get(sha)
            .map(|s| s.len())
            .unwrap_or(0);
        matched_findings += matched_for_commit;
        let extra = patch_f.saturating_sub(matched_patch_for_commit);
        unmatched_extra_findings += extra;
        if matched_for_commit > 0 && extra == 0 {
            clean_target_only_commits += 1;
        }

        tokens_in_vec.push(data.tokens_in as u64);
        tokens_out_vec.push(data.tokens_out as u64);
        tokens_cached_vec.push(data.tokens_cached as u64);
        turns_vec.push(data.turns as u64);
        duration_vec.push(data.duration_secs);
    }

    let avg_patch_findings_per_commit = if reviewed_commits > 0 {
        round_one_decimal(total_patch_findings as f64 / reviewed_commits as f64)
    } else {
        0.0
    };
    let concern_to_finding_conversion_pct = if total_concerns_raised > 0 {
        round_one_decimal((total_patch_findings as f64 * 100.0) / total_concerns_raised as f64)
    } else {
        0.0
    };

    let tokens_in = compute_distribution(&tokens_in_vec);
    let tokens_out = compute_distribution(&tokens_out_vec);
    let tokens_cached = compute_distribution(&tokens_cached_vec);
    let turns = compute_distribution(&turns_vec);
    let duration_secs = compute_distribution(&duration_vec);

    let cache_hit_rate_pct = if tokens_in.total > 0 {
        round_one_decimal((tokens_cached.total as f64 * 100.0) / tokens_in.total as f64)
    } else {
        0.0
    };

    let by_subsystem: Vec<SubsystemBenchmarkSummary> = subsystem_buckets
        .into_iter()
        .map(|(subsystem, (total, det, part, miss))| {
            let rate = if total > 0 {
                round_one_decimal(((det + part) as f64 * 100.0) / total as f64)
            } else {
                0.0
            };
            SubsystemBenchmarkSummary {
                subsystem,
                total,
                detected: det,
                partially_detected: part,
                missed: miss,
                detection_rate_pct: rate,
            }
        })
        .collect();

    BenchmarkSummaryStats {
        total_entries,
        evaluated_entries,
        unique_commits,
        reviewed_commits,
        detected,
        partially_detected,
        missed,
        not_reviewed,
        skipped,
        exact_detection_rate_pct,
        total_detection_rate_pct,
        missed_not_raised,
        missed_wrongly_dismissed,
        missed_preexisting_filtered,
        missed_review_failed,
        total_patch_findings,
        total_preexisting_findings,
        matched_findings,
        unmatched_extra_findings,
        avg_patch_findings_per_commit,
        clean_target_only_commits,
        findings_by_severity,
        total_concerns_raised,
        total_concerns_dismissed,
        concern_to_finding_conversion_pct,
        detections_by_stage,
        false_dismissals_by_stage,
        by_subsystem,
        tokens_in,
        tokens_out,
        tokens_cached,
        cache_hit_rate_pct,
        turns,
        duration_secs,
    }
}

/// Formats a Markdown summary report for `summary.md`.
pub fn format_benchmark_summary_markdown(report: &BenchmarkRunReport) -> String {
    let m = &report.metadata;
    let s = &report.summary;
    let mut out = String::new();

    out.push_str(&format!("# Sashiko Benchmark Report: `{}`\n\n", m.run_id));
    out.push_str("## Run Metadata\n\n");
    out.push_str(&format!(
        "- **Sashiko Version**: `{}` (`{}{}`)\n",
        m.sashiko_version,
        m.sashiko_git_sha,
        if m.sashiko_git_dirty { "-dirty" } else { "" }
    ));
    out.push_str(&format!(
        "- **Prompts Revision**: `{}`\n",
        m.prompts_git_hash
    ));
    out.push_str(&format!("- **Benchmark Suite**: `{}`\n", m.benchmark_file));
    out.push_str(&format!(
        "- **Model / Provider**: `{}` (`{}`), effort: `{}`\n",
        m.model,
        m.ai_provider,
        m.effort.as_deref().unwrap_or("default")
    ));
    out.push_str(&format!(
        "- **Entries / Unique Commits**: {} entries across {} unique commits (reviewed: {})\n",
        s.total_entries, s.unique_commits, s.reviewed_commits
    ));
    out.push_str(&format!(
        "- **Wall-Clock Time**: {}s (`{}` -> `{}`)\n",
        m.wall_clock_secs, m.started_at, m.finished_at
    ));
    out.push_str(&format!(
        "- **Preserved Database**: `{}`\n\n",
        m.database_path
    ));

    out.push_str("## Detection Accuracy\n\n");
    out.push_str("| Metric | Count | Rate |\n|---|---:|---:|\n");
    out.push_str(&format!(
        "| **Detected (Exact)** | {} | {:.1}% |\n",
        s.detected, s.exact_detection_rate_pct
    ));
    out.push_str(&format!(
        "| **Partially Detected** | {} | {:.1}% |\n",
        s.partially_detected,
        (s.total_detection_rate_pct - s.exact_detection_rate_pct).max(0.0)
    ));
    out.push_str(&format!(
        "| **Total Detected** | **{} / {}** | **{:.1}%** |\n",
        s.detected + s.partially_detected,
        s.evaluated_entries,
        s.total_detection_rate_pct
    ));
    out.push_str(&format!(
        "| **Missed** | {} | {:.1}% |\n",
        s.missed,
        if s.evaluated_entries > 0 {
            (s.missed as f64 * 100.0) / s.evaluated_entries as f64
        } else {
            0.0
        }
    ));
    out.push_str(&format!(
        "| Not Reviewed / Failed | {} | - |\n",
        s.not_reviewed
    ));
    out.push_str(&format!(
        "| Skipped (No Description) | {} | - |\n\n",
        s.skipped
    ));

    out.push_str("## Miss Diagnosis & Precision\n\n");
    out.push_str(&format!(
        "- **Miss Breakdown**: `not_raised`: {}, `wrongly_dismissed`: {}, `preexisting_filtered`: {}, `review_failed`: {}\n",
        s.missed_not_raised,
        s.missed_wrongly_dismissed,
        s.missed_preexisting_filtered,
        s.missed_review_failed
    ));
    out.push_str(&format!(
        "- **Patch Findings**: {} total ({:.1} avg/commit) — {} matched target bugs, {} extra/unmatched findings\n",
        s.total_patch_findings,
        s.avg_patch_findings_per_commit,
        s.matched_findings,
        s.unmatched_extra_findings
    ));
    out.push_str(&format!(
        "- **Clean Target-Only Commits**: {} / {}\n",
        s.clean_target_only_commits, s.reviewed_commits
    ));
    out.push_str(&format!(
        "- **Pre-existing Findings**: {}\n",
        s.total_preexisting_findings
    ));
    out.push_str(&format!(
        "- **Findings by Severity**: Critical: {}, High: {}, Medium: {}, Low: {}\n",
        s.findings_by_severity.critical,
        s.findings_by_severity.high,
        s.findings_by_severity.medium,
        s.findings_by_severity.low
    ));
    out.push_str(&format!(
        "- **Concerns Raised / Dismissed**: {} raised, {} dismissed ({:.1}% concern-to-finding conversion)\n\n",
        s.total_concerns_raised, s.total_concerns_dismissed, s.concern_to_finding_conversion_pct
    ));

    out.push_str("## Resource & Latency Distributions (Per Unique Reviewed Commit)\n\n");
    out.push_str("| Metric | Total | Mean | P50 | P90 | Max |\n|---|---:|---:|---:|---:|---:|\n");
    out.push_str(&format!(
        "| Tokens In | {} | {:.1} | {} | {} | {} |\n",
        s.tokens_in.total, s.tokens_in.mean, s.tokens_in.p50, s.tokens_in.p90, s.tokens_in.max
    ));
    out.push_str(&format!(
        "| Tokens Out | {} | {:.1} | {} | {} | {} |\n",
        s.tokens_out.total, s.tokens_out.mean, s.tokens_out.p50, s.tokens_out.p90, s.tokens_out.max
    ));
    out.push_str(&format!(
        "| Tokens Cached ({:.1}% hit) | {} | {:.1} | {} | {} | {} |\n",
        s.cache_hit_rate_pct,
        s.tokens_cached.total,
        s.tokens_cached.mean,
        s.tokens_cached.p50,
        s.tokens_cached.p90,
        s.tokens_cached.max
    ));
    out.push_str(&format!(
        "| Turns | {} | {:.1} | {} | {} | {} |\n",
        s.turns.total, s.turns.mean, s.turns.p50, s.turns.p90, s.turns.max
    ));
    out.push_str(&format!(
        "| Duration (s) | {} | {:.1} | {} | {} | {} |\n\n",
        s.duration_secs.total,
        s.duration_secs.mean,
        s.duration_secs.p50,
        s.duration_secs.p90,
        s.duration_secs.max
    ));

    if !s.by_subsystem.is_empty() {
        out.push_str("## Subsystem Breakdown\n\n");
        out.push_str("| Subsystem | Detected | Partial | Missed | Total | Rate |\n|---|---:|---:|---:|---:|---:|\n");
        for sub in &s.by_subsystem {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {:.1}% |\n",
                sub.subsystem,
                sub.detected,
                sub.partially_detected,
                sub.missed,
                sub.total,
                sub.detection_rate_pct
            ));
        }
        out.push('\n');
    }

    out.push_str("## Per-Entry Results\n\n");
    out.push_str("| Commit | Subsystem | Status | Matched / Miss Reason | Findings (Patch/Extra) | Turns | Time (s) | Explanation |\n|---|---|---|---|---:|---:|---:|---|\n");
    for r in &report.results {
        let short_commit: String = r.commit.chars().take(12).collect();
        let subsys = r.subsystem.as_deref().unwrap_or("-");
        let detail = if let Some(ref mf) = r.matched_finding {
            let stages = if mf.stages.is_empty() {
                "-".to_string()
            } else {
                mf.stages.join("+")
            };
            format!("F{} [{}] ({})", mf.index, mf.severity, stages)
        } else if let Some(ref mr) = r.miss_reason {
            if let Some(ref stage) = r.dismissed_by_stage {
                format!("{mr} ({stage})")
            } else {
                mr.clone()
            }
        } else {
            "-".to_string()
        };
        let clean_expl = r.explanation.replace(['\n', '|'], " ");
        out.push_str(&format!(
            "| `{}` | `{}` | **{}** | `{}` | {}/{} | {} | {} | {} |\n",
            short_commit,
            subsys,
            r.status,
            detail,
            r.patch_findings_count,
            r.extra_findings_count,
            r.turns,
            r.duration_secs,
            clean_expl
        ));
    }

    out
}

fn write_benchmark_artifacts(report: &BenchmarkRunReport, paths: &BenchmarkRunPaths) -> Result<()> {
    let json_bytes = serde_json::to_vec_pretty(report)?;
    std::fs::write(&paths.results_json_path, &json_bytes).with_context(|| {
        format!(
            "Failed to write benchmark results JSON to {}",
            paths.results_json_path.display()
        )
    })?;

    // Backwards-compatible copy in the current working directory.
    let legacy_path = Path::new("benchmark_results.json");
    if paths.results_json_path != legacy_path {
        let _ = std::fs::write(legacy_path, &json_bytes);
    }

    let md = format_benchmark_summary_markdown(report);
    let mut md_file = File::create(&paths.summary_md_path).with_context(|| {
        format!(
            "Failed to write benchmark Markdown summary to {}",
            paths.summary_md_path.display()
        )
    })?;
    md_file.write_all(md.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_deduplicate_benchmark_commits_preserves_order() {
        let entries = vec![
            BenchmarkEntry {
                commit: "aaa111".to_string(),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: Some("bug 1".to_string()),
            },
            BenchmarkEntry {
                commit: "bbb222".to_string(),
                fixed_by: None,
                subsystem: Some("mm".to_string()),
                problem_description: Some("bug 2".to_string()),
            },
            BenchmarkEntry {
                commit: "aaa111".to_string(),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: Some("bug 3 on same commit".to_string()),
            },
        ];

        let unique = deduplicate_benchmark_commits(&entries);
        assert_eq!(unique, vec!["aaa111".to_string(), "bbb222".to_string()]);
    }

    #[test]
    fn test_prepare_benchmark_run_paths_creates_run_dir_with_sha_and_date() -> Result<()> {
        let temp = tempdir()?;
        let bench_file = Path::new("projects/linux/benchmarks/benchmark_tiny.json");
        let paths = prepare_benchmark_run_paths(
            temp.path(),
            bench_file,
            "20261019-153000",
            "715294a276ef",
            None,
            None,
            false,
        )?;

        assert_eq!(paths.run_id, "20261019-153000_715294a276ef_benchmark_tiny");
        assert!(paths.run_dir.exists());
        assert_eq!(
            paths.db_path.file_name().and_then(|n| n.to_str()),
            Some("sashiko-20261019-153000-715294a276ef.db")
        );
        assert_eq!(paths.results_json_path, paths.run_dir.join("results.json"));
        assert_eq!(paths.summary_md_path, paths.run_dir.join("summary.md"));

        // Create the DB file and verify analyze_only auto-discovers it even when
        // explicit_output_dir is passed (as the CLI default does).
        std::fs::write(&paths.db_path, b"sqlite")?;
        let discovered = prepare_benchmark_run_paths(
            temp.path(),
            bench_file,
            "20261019-160000",
            "715294a276ef",
            None,
            Some(Path::new("benchmarks/runs")),
            true,
        )?;
        assert_eq!(discovered.db_path, paths.db_path);
        assert_eq!(discovered.run_dir, paths.run_dir);

        // Verify analyze_only fails cleanly when no prior run exists for a suite.
        let missing_err = prepare_benchmark_run_paths(
            temp.path(),
            Path::new("projects/linux/benchmarks/nonexistent_suite.json"),
            "20261019-160000",
            "715294a276ef",
            None,
            Some(Path::new("benchmarks/runs")),
            true,
        )
        .expect_err("Expected error when analyze_only finds no existing run");
        assert!(
            missing_err
                .to_string()
                .contains("No existing benchmark database found")
        );
        Ok(())
    }

    #[test]
    fn test_compute_distribution_stats() {
        let empty = compute_distribution(&[]);
        assert_eq!(empty, MetricDistribution::default());

        let dist = compute_distribution(&[100, 200, 300, 400, 1000]);
        assert_eq!(dist.total, 2000);
        assert_eq!(dist.mean, 400.0);
        assert_eq!(dist.p50, 300);
        assert_eq!(dist.p90, 400);
        assert_eq!(dist.max, 1000);
    }

    #[test]
    fn test_parse_judge_verdict_json_and_fallback() {
        let json_resp = r#"```json
        {
          "status": "DETECTED",
          "matched_finding_index": 2,
          "miss_reason": null,
          "dismissed_by_stage": null,
          "explanation": "Matches [F2] which reports the exact use-after-free."
        }
        ```"#;
        let parsed = parse_judge_verdict(json_resp).unwrap();
        assert_eq!(parsed.status, "DETECTED");
        assert_eq!(parsed.matched_finding_index, Some(2));
        assert!(parsed.explanation.contains("[F2]"));

        let missed_json = r#"{
          "status": "MISSED",
          "matched_finding_index": null,
          "miss_reason": "wrongly_dismissed",
          "dismissed_by_stage": "verification",
          "explanation": "[D1] raised the null deref in locking stage, but verification dismissed it."
        }"#;
        let parsed_miss = parse_judge_verdict(missed_json).unwrap();
        assert_eq!(parsed_miss.status, "MISSED");
        assert_eq!(
            parsed_miss.miss_reason.as_deref(),
            Some("wrongly_dismissed")
        );
        assert_eq!(
            parsed_miss.dismissed_by_stage.as_deref(),
            Some("verification")
        );

        let plain_resp = "PARTIALLY_DETECTED: Finding [F1] identifies the leak on the error path.";
        let parsed_plain = parse_judge_verdict(plain_resp).unwrap();
        assert_eq!(parsed_plain.status, "PARTIALLY_DETECTED");
        assert_eq!(parsed_plain.matched_finding_index, Some(1));
    }

    #[test]
    fn test_build_benchmark_summary_deduplicates_multi_entry_commit_metrics() {
        let entries = vec![
            BenchmarkEntry {
                commit: "b168ed458dde".to_string(),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: Some("First bug in commit".to_string()),
            },
            BenchmarkEntry {
                commit: "b168ed458dde".to_string(),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: Some("Second bug in same commit".to_string()),
            },
        ];

        let finding_1 = BenchmarkFindingRecord {
            index: 1,
            severity: "High".to_string(),
            problem: "First bug".to_string(),
            explanation: None,
            preexisting: false,
            origin: Some("verification".to_string()),
            stages: vec!["resources".to_string()],
            prompts: vec![],
        };
        let finding_2 = BenchmarkFindingRecord {
            index: 2,
            severity: "Medium".to_string(),
            problem: "Unrelated noise finding".to_string(),
            explanation: None,
            preexisting: false,
            origin: Some("verification".to_string()),
            stages: vec!["implementation".to_string()],
            prompts: vec![],
        };

        let mut commit_map = HashMap::new();
        commit_map.insert(
            "b168ed458dde".to_string(),
            CommitReviewData {
                subject: Some("net: sample commit".to_string()),
                patchset_id: Some(1),
                patch_id: Some(1),
                review_id: Some(1),
                review_status: Some("Reviewed".to_string()),
                summary_text: "Summary".to_string(),
                tokens_in: 50_000,
                tokens_out: 5_000,
                tokens_cached: 25_000,
                turns: 8,
                duration_secs: 40,
                concerns_count: 3,
                dismissed_concerns_count: 1,
                planned_stages: vec!["resources".to_string()],
                selected_guides: vec![],
                stages: vec![],
                findings: vec![finding_1.clone(), finding_2],
                dismissed_concerns: vec![BenchmarkDismissedRecord {
                    index: 1,
                    stage: "verification".to_string(),
                    description: "Second bug".to_string(),
                    reasoning: "Assumed caller held lock".to_string(),
                }],
            },
        );

        let results = vec![
            BenchmarkEntryResult {
                commit: "b168ed458dde".to_string(),
                subject: Some("net: sample commit".to_string()),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: "First bug in commit".to_string(),
                found: true,
                status: "DETECTED".to_string(),
                explanation: "Matches [F1]".to_string(),
                matched_finding: Some(MatchedFindingSummary {
                    index: 1,
                    severity: "High".to_string(),
                    problem: "First bug".to_string(),
                    preexisting: false,
                    origin: Some("verification".to_string()),
                    stages: vec!["resources".to_string()],
                    prompts: vec![],
                }),
                miss_reason: None,
                dismissed_by_stage: None,
                findings_count: 2,
                patch_findings_count: 2,
                preexisting_findings_count: 0,
                extra_findings_count: 1,
                concerns_count: 3,
                dismissed_concerns_count: 1,
                tokens_in: 50_000,
                tokens_out: 5_000,
                tokens_cached: 25_000,
                turns: 8,
                duration_secs: 40,
                review_id: Some(1),
                patchset_id: Some(1),
                patch_id: Some(1),
                planned_stages: vec!["resources".to_string()],
                selected_guides: vec![],
                stages: vec![],
                findings: vec![],
                dismissed_concerns: vec![],
            },
            BenchmarkEntryResult {
                commit: "b168ed458dde".to_string(),
                subject: Some("net: sample commit".to_string()),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: "Second bug in same commit".to_string(),
                found: false,
                status: "MISSED".to_string(),
                explanation: "[D1] was dismissed in verification".to_string(),
                matched_finding: None,
                miss_reason: Some("wrongly_dismissed".to_string()),
                dismissed_by_stage: Some("verification".to_string()),
                findings_count: 2,
                patch_findings_count: 2,
                preexisting_findings_count: 0,
                extra_findings_count: 2,
                concerns_count: 3,
                dismissed_concerns_count: 1,
                tokens_in: 50_000,
                tokens_out: 5_000,
                tokens_cached: 25_000,
                turns: 8,
                duration_secs: 40,
                review_id: Some(1),
                patchset_id: Some(1),
                patch_id: Some(1),
                planned_stages: vec!["resources".to_string()],
                selected_guides: vec![],
                stages: vec![],
                findings: vec![],
                dismissed_concerns: vec![],
            },
        ];

        let summary = build_benchmark_summary(&entries, &results, &commit_map);
        assert_eq!(summary.total_entries, 2);
        assert_eq!(summary.unique_commits, 1);
        assert_eq!(summary.reviewed_commits, 1);
        assert_eq!(summary.detected, 1);
        assert_eq!(summary.missed, 1);
        assert_eq!(summary.missed_wrongly_dismissed, 1);
        assert_eq!(
            summary.false_dismissals_by_stage.get("verification"),
            Some(&1)
        );
        assert_eq!(summary.detections_by_stage.get("resources"), Some(&1));
        // Tokens and findings are counted once for the unique commit, not doubled!
        assert_eq!(summary.tokens_in.total, 50_000);
        assert_eq!(summary.tokens_cached.total, 25_000);
        assert_eq!(summary.cache_hit_rate_pct, 50.0);
        assert_eq!(summary.total_patch_findings, 2);
        assert_eq!(summary.matched_findings, 1);
        assert_eq!(summary.unmatched_extra_findings, 1);
    }

    #[test]
    fn test_failed_reviews_excluded_from_reviewed_commits_and_sequential_finding_indices() {
        let mut data = CommitReviewData::default();
        let output = serde_json::json!({
            "findings": [
                { "problem": "   ", "severity": "Low" },
                { "problem": "Patch null deref", "severity": "High", "preexisting": false }
            ],
            "concerns": [
                { "description": "Patch null deref" },
                { "description": "Pre-existing race in foo" },
                { "description": "Filtered pre-existing concern" }
            ]
        });
        populate_findings_and_stages_from_output(&mut data, &output);
        assert_eq!(data.findings.len(), 1);
        assert_eq!(data.findings[0].index, 1);

        // Simulate merging a pre-existing bug from the bugs table before filtering concerns.
        data.findings.push(BenchmarkFindingRecord {
            index: data.findings.len() + 1,
            severity: "High".to_string(),
            problem: "Pre-existing race in foo".to_string(),
            explanation: None,
            preexisting: true,
            origin: Some("bug_repository".to_string()),
            stages: vec![],
            prompts: vec![],
        });
        populate_dismissed_from_output(&mut data, &output);
        assert_eq!(data.dismissed_concerns.len(), 1);
        assert_eq!(
            data.dismissed_concerns[0].description,
            "Filtered pre-existing concern"
        );

        // A failed review with a non-null review_id must not count as a reviewed commit.
        let entries = vec![BenchmarkEntry {
            commit: "failed123".to_string(),
            fixed_by: None,
            subsystem: Some("mm".to_string()),
            problem_description: Some("bug".to_string()),
        }];
        let mut commit_map = HashMap::new();
        commit_map.insert(
            "failed123".to_string(),
            CommitReviewData {
                review_id: Some(99),
                review_status: Some("Failed".to_string()),
                tokens_in: 1000,
                ..Default::default()
            },
        );
        let results = vec![empty_entry_result(
            &entries[0],
            "bug",
            "NOT_REVIEWED",
            "Failed",
            Some("review_failed"),
        )];
        let summary = build_benchmark_summary(&entries, &results, &commit_map);
        assert_eq!(summary.reviewed_commits, 0);
        assert_eq!(summary.tokens_in.total, 0);
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn test_persist_and_load_commit_review_data_resets_and_prefers_reviewed() {
        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let sha = "1111111111111111111111111111111111111111";
        persist_benchmark_patch(
            &db,
            sha,
            sha,
            "net: fix race",
            "Author <a@b.c>",
            "diff --git a/foo.c b/foo.c\n",
            "body",
            None,
            1_700_000_000,
            1,
            1,
        )
        .await
        .unwrap();

        let (patch_id, patchset_id, _, _, _) = lookup_benchmark_patch_row(&db, sha)
            .await
            .unwrap()
            .expect("patch row should exist");

        // Case 1: Patchset fails before any review row is created (e.g., Failed To Apply),
        // while patches.status is still Pending. load_commit_review_data should report
        // patchset-level status ("Failed To Apply").
        db.execute(
            "UPDATE patchsets SET status = 'Failed To Apply', failed_reason = 'conflict' WHERE id = ?",
            libsql::params![patchset_id],
        )
        .await
        .unwrap();
        let data_before_review = load_commit_review_data(&db, sha).await.unwrap();
        assert_eq!(
            data_before_review.review_status.as_deref(),
            Some("Failed To Apply")
        );

        // Case 2: Patchset is Cancelled with a Failed review row. Re-running
        // persist_benchmark_patch must delete the Failed review and reset both
        // patch and patchset out of Cancelled back to Pending.
        let failed_rev_id = db
            .create_review(patchset_id, Some(patch_id), "gemini", "pro", None, None)
            .await
            .unwrap();
        db.execute(
            "UPDATE reviews SET status = 'Failed' WHERE id = ?",
            libsql::params![failed_rev_id],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patchsets SET status = 'Cancelled' WHERE id = ?",
            libsql::params![patchset_id],
        )
        .await
        .unwrap();

        persist_benchmark_patch(
            &db,
            sha,
            sha,
            "net: fix race",
            "Author <a@b.c>",
            "diff --git a/foo.c b/foo.c\n",
            "body",
            None,
            1_700_000_000,
            1,
            1,
        )
        .await
        .unwrap();

        let (_, _, patch_status, patchset_status, _) =
            lookup_benchmark_patch_row(&db, sha).await.unwrap().unwrap();
        assert_eq!(patch_status.as_deref(), Some("Pending"));
        assert_eq!(patchset_status.as_deref(), Some("Pending"));

        // Case 3: When both an older Reviewed row and a newer Failed row exist,
        // load_commit_review_data prefers the Reviewed row, and persist_benchmark_patch
        // cleans up the newer non-Reviewed row while keeping the commit Reviewed.
        let reviewed_id = db
            .create_review(patchset_id, Some(patch_id), "gemini", "pro", None, None)
            .await
            .unwrap();
        db.execute(
            "UPDATE reviews SET status = 'Reviewed', summary = 'Found issue' WHERE id = ?",
            libsql::params![reviewed_id],
        )
        .await
        .unwrap();
        let newer_failed_id = db
            .create_review(patchset_id, Some(patch_id), "gemini", "pro", None, None)
            .await
            .unwrap();
        db.execute(
            "UPDATE reviews SET status = 'Failed', summary = 'Transient error' WHERE id = ?",
            libsql::params![newer_failed_id],
        )
        .await
        .unwrap();

        let loaded = load_commit_review_data(&db, sha).await.unwrap();
        assert_eq!(loaded.review_id, Some(reviewed_id));
        assert_eq!(loaded.review_status.as_deref(), Some("Reviewed"));

        persist_benchmark_patch(
            &db,
            sha,
            sha,
            "net: fix race",
            "Author <a@b.c>",
            "diff --git a/foo.c b/foo.c\n",
            "body",
            None,
            1_700_000_000,
            1,
            1,
        )
        .await
        .unwrap();

        let mut remaining = db
            .conn
            .query(
                "SELECT id, status FROM reviews WHERE patch_id = ?",
                libsql::params![patch_id],
            )
            .await
            .unwrap();
        let row = remaining.next().await.unwrap().expect("Reviewed row kept");
        assert_eq!(row.get::<i64>(0).unwrap(), reviewed_id);
        assert_eq!(row.get::<String>(1).unwrap(), "Reviewed");
        assert!(remaining.next().await.unwrap().is_none());
    }
}
