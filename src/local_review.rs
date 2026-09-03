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

use crate::{
    git_ops::{GitWorktree, extract_patch_metadata, get_commit_hash, resolve_git_range},
    settings::{AiSettings, Settings},
    toolbox::ToolBox,
    worker::{
        PatchInput, ReviewInput, Worker, WorkerConfig, calculate_series_range,
        prompts::PromptRegistry,
    },
};
use anyhow::{Context, Result, anyhow};
use futures::stream::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Semaphore;
use tracing::{error, info};

#[derive(Clone, Debug)]
pub struct WorkerOptions {
    pub settings_path: Option<PathBuf>,
    pub baseline: Option<String>,
    pub repo: Option<PathBuf>,
    pub worktree_dir: Option<PathBuf>,
    pub prompts: PathBuf,
    pub review_patch_index: Option<i64>,
    pub review_commit: Option<String>,
    pub no_ai: bool,
    pub reuse_worktree: Option<PathBuf>,
    pub ai_provider: Option<String>,
    pub custom_prompt: Option<String>,
    pub stages: Option<Vec<u8>>,
    pub scratch_clone: bool,
    pub current_tree: bool,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            settings_path: None,
            baseline: None,
            repo: None,
            worktree_dir: None,
            prompts: PathBuf::from("third_party/prompts/kernel"),
            review_patch_index: None,
            review_commit: None,
            no_ai: false,
            reuse_worktree: None,
            ai_provider: None,
            custom_prompt: None,
            stages: None,
            scratch_clone: false,
            current_tree: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReviewOptions {
    pub baseline: Option<String>,
    pub settings_path: Option<PathBuf>,
    pub prompts: PathBuf,
    pub no_ai: bool,
    pub ai_provider: Option<String>,
    pub custom_prompt: Option<String>,
    pub stages: Option<Vec<u8>>,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            baseline: None,
            settings_path: None,
            prompts: PathBuf::from("third_party/prompts/kernel"),
            no_ai: false,
            ai_provider: None,
            custom_prompt: None,
            stages: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ProgressEvent {
    ResolvingInput {
        input: String,
    },
    ResolvedCommits {
        commits: Vec<CommitSummary>,
    },
    BaselineResolved {
        rev: String,
        sha: String,
    },
    CurrentTreeReady {
        path: PathBuf,
    },
    WorktreeCreated {
        path: PathBuf,
    },
    ApplyingPatch {
        index: i64,
        total: usize,
        subject: String,
    },
    PatchApplied {
        index: i64,
    },
    PatchFailed {
        index: i64,
        error: String,
    },
    AiReviewStarted {
        patches: usize,
    },
    AiReviewPreScreenStarted {
        patch_index: i64,
    },
    AiReviewPlanningStarted {
        patch_index: i64,
    },
    AiReviewPlanReady {
        patch_index: i64,
        planned_stages: Vec<u8>,
    },
    AiReviewStageStarted {
        patch_index: i64,
        stage: u8,
    },
    AiReviewStageTurn {
        patch_index: i64,
        stage: u8,
        turn: usize,
        max_turns: usize,
    },
    AiReviewStageFinished {
        patch_index: i64,
        stage: u8,
    },
    AiReviewAttempt {
        patch_index: i64,
        attempt: usize,
        max_attempts: usize,
    },
    AiReviewFinished {
        patch_index: i64,
    },
    ReviewComplete,
}

#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub index: i64,
    pub sha: String,
    pub subject: String,
    pub author: String,
}

pub type ProgressCallback<'a> = dyn Fn(ProgressEvent) + Send + Sync + 'a;

pub async fn build_review_input_from_git(
    repo_path: &Path,
    input: &str,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<(ReviewInput, Vec<String>)> {
    emit(
        progress,
        ProgressEvent::ResolvingInput {
            input: input.to_string(),
        },
    );

    let shas = if input.contains("..") {
        resolve_git_range(repo_path, input).await?
    } else {
        vec![get_commit_hash(repo_path, input).await?]
    };

    let mut patches = Vec::new();
    let mut summaries = Vec::new();

    for (i, sha) in shas.iter().enumerate() {
        let meta = extract_patch_metadata(repo_path, sha)
            .await
            .with_context(|| format!("Failed to extract metadata for commit {}", sha))?;
        let index = (i + 1) as i64;
        summaries.push(CommitSummary {
            index,
            sha: short_sha(sha),
            subject: meta.subject.clone(),
            author: meta.author.clone(),
        });
        patches.push(PatchInput {
            index,
            diff: meta.diff,
            subject: Some(meta.subject),
            author: Some(meta.author),
            date: Some(meta.timestamp),
            message_id: None,
            commit_id: Some(sha.clone()),
        });
    }

    emit(
        progress,
        ProgressEvent::ResolvedCommits { commits: summaries },
    );

    let subject = patches
        .first()
        .and_then(|p| p.subject.clone())
        .unwrap_or_else(|| input.to_string());

    Ok((
        ReviewInput {
            id: 0,
            subject,
            patches,
        },
        shas,
    ))
}

pub async fn run_git_review(
    repo_path: PathBuf,
    input: String,
    options: ReviewOptions,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    let (review_input, shas) = build_review_input_from_git(&repo_path, &input, progress).await?;
    let baseline = options
        .baseline
        .clone()
        .or_else(|| shas.first().map(|sha| format!("{}^", sha)));

    run_worker(
        review_input,
        WorkerOptions {
            settings_path: options
                .settings_path
                .or_else(|| Some(Settings::local_review_path())),
            baseline,
            prompts: options.prompts,
            no_ai: options.no_ai,
            ai_provider: options.ai_provider,
            custom_prompt: options.custom_prompt,
            stages: options.stages,
            current_tree: true,
            ..WorkerOptions::default()
        },
        Some(repo_path),
        progress,
    )
    .await
}

pub async fn run_worker(
    input: ReviewInput,
    options: WorkerOptions,
    repo_override: Option<PathBuf>,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    let (mut ai, configured_repo_path, concurrency, timeout_seconds) =
        if let Some(path) = &options.settings_path {
            let local_settings = Settings::local_review_from_file(path)
                .with_context(|| format!("Failed to load settings from {}", path.display()))?;
            let review = local_settings.review;
            (
                local_settings.ai,
                None,
                review.concurrency,
                review.timeout_seconds,
            )
        } else if repo_override.is_some() {
            let local_settings =
                Settings::local_review_settings().context("Failed to load local review settings")?;
            let review = local_settings.review;
            (
                local_settings.ai,
                None,
                review.concurrency,
                review.timeout_seconds,
            )
        } else {
            let settings = Settings::new().context("Failed to load settings")?;
            (
                settings.ai,
                Some(PathBuf::from(settings.git.repository_path)),
                settings.review.concurrency,
                settings.review.timeout_seconds,
            )
        };

    if let Some(provider) = &options.ai_provider {
        ai.provider = provider.clone();
    }

    let patchset_id = input.id;
    let subject = input.subject;
    let patches = input.patches;
    let baseline_arg = if options.current_tree {
        options.baseline.clone().unwrap_or_default()
    } else {
        options
            .baseline
            .clone()
            .unwrap_or_else(|| "HEAD".to_string())
    };
    let repo_path = repo_override
        .or(configured_repo_path)
        .ok_or_else(|| anyhow!("Missing repository path"))?;

    let (worktree, baseline_sha) = if options.current_tree {
        emit(
            progress,
            ProgressEvent::CurrentTreeReady {
                path: repo_path.clone(),
            },
        );
        (
            GitWorktree::from_path(repo_path.clone(), repo_path.clone()),
            baseline_arg.clone(),
        )
    } else if let Some(path) = &options.reuse_worktree {
        let baseline_sha = get_commit_hash(&repo_path, &baseline_arg).await?;
        emit(
            progress,
            ProgressEvent::BaselineResolved {
                rev: baseline_arg.clone(),
                sha: short_sha(&baseline_sha),
            },
        );
        info!("Reusing existing worktree at {:?}", path);
        (
            GitWorktree::from_path(path.clone(), repo_path.clone()),
            baseline_sha,
        )
    } else if options.scratch_clone {
        let baseline_sha = get_commit_hash(&repo_path, &baseline_arg).await?;
        emit(
            progress,
            ProgressEvent::BaselineResolved {
                rev: baseline_arg.clone(),
                sha: short_sha(&baseline_sha),
            },
        );
        let worktree = GitWorktree::new_scratch_clone(
            &repo_path,
            &baseline_sha,
            options.worktree_dir.as_deref(),
        )
        .await?;
        emit(
            progress,
            ProgressEvent::WorktreeCreated {
                path: worktree.path.clone(),
            },
        );
        (worktree, baseline_sha)
    } else {
        let baseline_sha = get_commit_hash(&repo_path, &baseline_arg).await?;
        emit(
            progress,
            ProgressEvent::BaselineResolved {
                rev: baseline_arg.clone(),
                sha: short_sha(&baseline_sha),
            },
        );
        let worktree =
            GitWorktree::new(&repo_path, &baseline_sha, options.worktree_dir.as_deref()).await?;
        emit(
            progress,
            ProgressEvent::WorktreeCreated {
                path: worktree.path.clone(),
            },
        );
        (worktree, baseline_sha)
    };

    let result = run_worker_in_worktree(
        &worktree,
        &ai,
        concurrency,
        timeout_seconds,
        patchset_id,
        subject,
        patches,
        &baseline_arg,
        &baseline_sha,
        &options,
        progress,
    )
    .await;

    if !options.current_tree
        && let Err(e) = worktree.remove().await
    {
        error!("Failed to remove worktree: {}", e);
    }
    emit(progress, ProgressEvent::ReviewComplete);

    result
}

#[allow(clippy::too_many_arguments)]
/// Wrap the raw provider with the decorators a worker-run review needs.
///
/// Per-turn logging is implemented once, as a provider decorator, rather than
/// in each front end: both local-CLI and daemon-spawned worker reviews run this
/// same path, so wrapping here covers both.
///
/// The limiters are skipped for a daemon-spawned worker, which reaches the
/// model through a stdio provider and is throttled by the daemon instead.
fn decorate_provider(
    inner: std::sync::Arc<dyn crate::ai::AiProvider>,
    ai: &AiSettings,
    llm_semaphore: &Arc<Semaphore>,
    quota: &Arc<crate::ai::quota::QuotaManager>,
    retry_budget: &Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>>,
) -> std::sync::Arc<dyn crate::ai::AiProvider> {
    let provider: std::sync::Arc<dyn crate::ai::AiProvider> = if ai.log_turns {
        std::sync::Arc::new(crate::ai::logging_provider::LoggingProvider::new(inner))
    } else {
        inner
    };

    if ai.provider.starts_with("stdio-") {
        return provider;
    }

    let provider: std::sync::Arc<dyn crate::ai::AiProvider> = std::sync::Arc::new(
        crate::ai::concurrency_limited_provider::ConcurrencyLimitedProvider::new(
            provider,
            llm_semaphore.clone(),
        ),
    );

    // Backoff goes outermost, so a call that is waiting out a rate limit holds
    // no concurrency permit while it sleeps.
    // With a retry budget it stops at the review's deadline, as in the daemon;
    // without one it falls back to an attempt ceiling.
    std::sync::Arc::new(crate::ai::backoff_provider::BackoffProvider::new(
        provider,
        quota.clone(),
        retry_budget.clone(),
    ))
}

async fn review_single_patch(
    worktree: &GitWorktree,
    ai: &AiSettings,
    patchset_id: i64,
    subject: &str,
    p: &PatchInput,
    all_patches: &[PatchInput],
    rich_patches: &[Value],
    patch_shas: &HashMap<i64, String>,
    options: &WorkerOptions,
    baseline_sha: &str,
    llm_semaphore: &Arc<Semaphore>,
    quota: &Arc<crate::ai::quota::QuotaManager>,
    retry_budget: &Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>>,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    let mut last_error = None;
    for attempt in 1..=3 {
        emit(
            progress,
            ProgressEvent::AiReviewAttempt {
                patch_index: p.index,
                attempt,
                max_attempts: 3,
            },
        );

        if attempt > 1 {
            info!(
                "Restarting AI review for patch {} (attempt {}/3)...",
                p.index, attempt
            );
        }

        let provider =
            crate::ai::create_provider_from_ai(ai).context("Failed to create AI provider")?;
        let provider = decorate_provider(provider, ai, llm_semaphore, quota, retry_budget);
        let prompts_tool_path = Some(options.prompts.join("tool.md"));

        let mut patch_files = Vec::new();
        if let Some(sha) = patch_shas.get(&p.index) {
            let output = tokio::process::Command::new("git")
                .current_dir(&worktree.path)
                .args(["diff-tree", "--no-commit-id", "--name-only", "-r", sha])
                .output()
                .await;
            if let Ok(out) = output
                && out.status.success()
            {
                let file_list = String::from_utf8_lossy(&out.stdout);
                for file in file_list.lines() {
                    let trimmed = file.trim().to_string();
                    if !trimmed.is_empty() {
                        patch_files.push(trimmed);
                    }
                }
            }
        }
        info!(
            "Active patch files gathered for patch {}: {:?}",
            p.index, patch_files
        );

        let mut tools = ToolBox::new(worktree.path.clone(), prompts_tool_path);
        tools.set_active_patch_files(patch_files);

        if let Some(sha) = patch_shas.get(&p.index) {
            info!("Setting virtual HEAD to {} for patch {}", sha, p.index);
            tools.set_virtual_head(sha.clone());
        }

        let prompts = PromptRegistry::new(options.prompts.clone());
        let series_range = calculate_series_range(
            all_patches,
            std::slice::from_ref(p),
            patch_shas,
            baseline_sha,
        );

        let mut worker = Worker::new(
            provider,
            std::sync::Arc::new(tools),
            prompts,
            WorkerConfig {
                max_input_tokens: ai.max_input_tokens,
                max_interactions: ai.max_interactions,
                temperature: ai.temperature,
                custom_prompt: options.custom_prompt.clone(),
                series_range,
                baseline_sha: Some(baseline_sha.to_string()),
                stages: options.stages.clone(),
            },
        );

        let p_index = p.index;
        let progress_cb = progress.map(|cb| {
            move |event| match event {
                crate::worker::WorkerProgressEvent::PreScreenStarted => {
                    cb(ProgressEvent::AiReviewPreScreenStarted {
                        patch_index: p_index,
                    });
                }
                crate::worker::WorkerProgressEvent::PlanningStarted => {
                    cb(ProgressEvent::AiReviewPlanningStarted {
                        patch_index: p_index,
                    });
                }
                crate::worker::WorkerProgressEvent::ReviewStarted { planned_stages } => {
                    cb(ProgressEvent::AiReviewPlanReady {
                        patch_index: p_index,
                        planned_stages,
                    });
                }
                crate::worker::WorkerProgressEvent::StageStarted { stage } => {
                    cb(ProgressEvent::AiReviewStageStarted {
                        patch_index: p_index,
                        stage,
                    });
                }
                crate::worker::WorkerProgressEvent::StageTurn {
                    stage,
                    turn,
                    max_turns,
                } => {
                    cb(ProgressEvent::AiReviewStageTurn {
                        patch_index: p_index,
                        stage,
                        turn,
                        max_turns,
                    });
                }
                crate::worker::WorkerProgressEvent::StageFinished { stage } => {
                    cb(ProgressEvent::AiReviewStageFinished {
                        patch_index: p_index,
                        stage,
                    });
                }
            }
        });

        let patchset_val = json!({
            "id": patchset_id,
            "subject": subject,
            "patches": rich_patches,
            "patch_index": Some(p.index),
            "baseline": baseline_sha
        });

        match worker
            .run(
                patchset_val,
                progress_cb
                    .as_ref()
                    .map(|f| f as &(dyn Fn(_) + Send + Sync)),
            )
            .await
        {
            Ok(result) => {
                info!("AI review completed for patch {}.", p.index);
                emit(
                    progress,
                    ProgressEvent::AiReviewFinished {
                        patch_index: p.index,
                    },
                );

                let mut inline_content = None;
                if let Some(output) = &result.output
                    && let Some(content) = output.get("review_inline").and_then(|v| v.as_str())
                {
                    inline_content = Some(content.to_string());
                }

                let mut has_findings = false;
                if let Some(output) = &result.output
                    && let Some(findings) = output.get("findings").and_then(|f| f.as_array())
                    && !findings.is_empty()
                {
                    has_findings = true;
                }

                if has_findings && inline_content.is_none() {
                    error!(
                        "Review failure on patch {}: Findings detected but review_inline field was missing or empty.",
                        p.index
                    );
                    if attempt < 3 {
                        continue;
                    }
                }

                return Ok(json!({
                    "patch_index": p.index,
                    "review": result.output,
                    "error": result.error,
                    "inline_review": inline_content,
                    "input_context": result.input_context,
                    "history": result.history,
                    "tokens_in": result.tokens_in,
                    "tokens_out": result.tokens_out,
                    "tokens_cached": result.tokens_cached
                }));
            }
            Err(e) => {
                error!(
                    "AI review for patch {} failed with exception: {}",
                    p.index, e
                );
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("Patch review failed")))
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_in_worktree(
    worktree: &GitWorktree,
    ai: &AiSettings,
    concurrency: usize,
    timeout_seconds: u64,
    patchset_id: i64,
    subject: String,
    patches: Vec<PatchInput>,
    baseline_arg: &str,
    baseline_sha: &str,
    options: &WorkerOptions,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    info!("Worktree at {:?}", worktree.path);
    info!("Found {} patches total", patches.len());

    let mut patch_results = Vec::new();
    let mut patch_shas = HashMap::new();
    let mut patch_shows = HashMap::new();
    let mut patch_messages = HashMap::new();

    let all_applied = if options.current_tree {
        for p in &patches {
            if let Some(sha) = &p.commit_id {
                patch_shas.insert(p.index, sha.clone());
                if let Ok(show) = worktree.get_commit_show(sha).await {
                    patch_shows.insert(p.index, show);
                }
                if let Ok(msg) = worktree.get_commit_message(sha).await {
                    patch_messages.insert(p.index, msg);
                }
            }
            patch_results.push(json!({
                "index": p.index,
                "status": "present",
                "method": "current-tree",
                "subject": p.subject.clone()
            }));
        }
        true
    } else if let Some(commit_hash) = &options.review_commit {
        info!("Directly reviewing commit {}", commit_hash);
        if let Some(idx) = options.review_patch_index {
            patch_shas.insert(idx, commit_hash.clone());
            if let Ok(show) = worktree.get_commit_show(commit_hash).await {
                patch_shows.insert(idx, show);
            }
            let subject = patches
                .iter()
                .find(|p| p.index == idx)
                .and_then(|p| p.subject.clone());
            patch_results.push(json!({
                "index": idx,
                "status": "applied",
                "method": "pre-applied",
                "subject": subject
            }));
        }
        true
    } else {
        info!(
            "Applying all {} patches to validate series...",
            patches.len()
        );
        let mut applied = true;

        for p in &patches {
            emit(
                progress,
                ProgressEvent::ApplyingPatch {
                    index: p.index,
                    total: patches.len(),
                    subject: p.subject.clone().unwrap_or_else(|| "patch".to_string()),
                },
            );

            let success = apply_single_patch(
                worktree,
                p,
                &mut patch_shas,
                &mut patch_shows,
                &mut patch_messages,
                &mut patch_results,
            )
            .await;

            if success {
                emit(progress, ProgressEvent::PatchApplied { index: p.index });
            } else {
                applied = false;
                let error = patch_results
                    .last()
                    .and_then(|p| p.get("error"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("patch application failed")
                    .to_string();
                emit(
                    progress,
                    ProgressEvent::PatchFailed {
                        index: p.index,
                        error,
                    },
                );
            }
        }
        applied
    };

    let mut patches_to_review: Vec<PatchInput> =
        if let Some(target_idx) = options.review_patch_index {
            patches
                .iter()
                .filter(|p| p.index == target_idx)
                .cloned()
                .collect()
        } else {
            patches.clone()
        };

    if options.no_ai {
        info!("Skipping AI review due to --no-ai flag.");
        patches_to_review.clear();
    }

    if !all_applied {
        info!("Not all patches applied successfully. Skipping AI review.");
        return Ok(json!({
            "patchset_id": patchset_id,
            "baseline": baseline_arg,
            "patches": patch_results,
            "error": "Patch application failed"
        }));
    }

    if patches_to_review.is_empty() {
        info!("No patches matched review index or list empty. Skipping AI review.");
        return Ok(json!({
            "patchset_id": patchset_id,
            "baseline": baseline_arg,
            "patches": patch_results,
            "review": null,
            "input_context": "",
            "tokens_in": 0,
            "tokens_out": 0,
            "tokens_cached": 0
        }));
    }

    info!(
        "Patches applied. Starting AI reviews for {} patches...",
        patches_to_review.len()
    );
    emit(
        progress,
        ProgressEvent::AiReviewStarted {
            patches: patches_to_review.len(),
        },
    );

    let rich_patches: Vec<Value> = patches
        .iter()
        .map(|p| {
            let date_str = if let Some(ts) = p.date {
                use chrono::{TimeZone, Utc};
                Utc.timestamp_opt(ts, 0)
                    .single()
                    .map(|dt| dt.to_rfc2822())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            json!({
                "index": p.index,
                "subject": p.subject,
                "author": p.author,
                "date_string": date_str,
                "diff": p.diff,
                "commit_id": patch_shas.get(&p.index).cloned(),
                "git_show": patch_shows.get(&p.index).cloned(),
                "commit_message_full": patch_messages.get(&p.index).cloned()
            })
        })
        .collect();

    // Cap in-flight model calls across the whole run. The patch fan-out below
    // is bounded by `concurrency`; each patch then fans its stages out
    // concurrently on top of that, so without a shared ceiling the number of
    // simultaneous requests is unbounded.
    let llm_semaphore = Arc::new(Semaphore::new(
        crate::ai::concurrency_limited_provider::llm_permits(concurrency),
    ));
    // Shared so a rate-limit response from one request backs the whole run off.
    let quota = Arc::new(crate::ai::quota::QuotaManager::new());
    // Bound the run by [review] timeout_seconds, as the daemon does. Time spent
    // waiting out a rate limit is credited back rather than charged to it.
    // 0 disables the limit, leaving retries bounded by their attempt ceiling.
    let retry_budget: Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>> =
        (timeout_seconds > 0).then(|| {
            let deadline = Arc::new(std::sync::Mutex::new(
                tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds),
            ));
            Arc::new(crate::ai::backoff_provider::DeadlineBudget::new(deadline))
                as Arc<dyn crate::ai::backoff_provider::RetryBudget>
        });

    // Execute patch reviews concurrently with a limit
    let futures_stream = futures::stream::iter(patches_to_review.iter().map(|p| {
        let rich_patches = rich_patches.clone();
        let patch_shas = &patch_shas;
        let options = &options;
        let subject_clone = subject.clone();
        let all_patches = &patches;
        let llm_semaphore = &llm_semaphore;
        let quota = &quota;
        let retry_budget = &retry_budget;
        async move {
            review_single_patch(
                worktree,
                ai,
                patchset_id,
                &subject_clone,
                p,
                all_patches,
                &rich_patches,
                patch_shas,
                options,
                baseline_sha,
                llm_semaphore,
                quota,
                retry_budget,
                progress,
            )
            .await
        }
    }));

    let mut buffered = futures_stream.buffer_unordered(concurrency);
    let mut results = Vec::new();
    while let Some(res) = buffered.next().await {
        results.push(res?);
    }

    // Aggregate findings, inline reviews, history, input context, and concern counts
    let mut combined_findings = Vec::new();
    let mut combined_dismissed_concerns = Vec::new();
    let mut combined_inline = String::new();
    let mut combined_history = Vec::new();
    let mut combined_input_context = String::new();
    let mut total_tokens_in = 0;
    let mut total_tokens_out = 0;
    let mut total_tokens_cached = 0;
    let mut total_concerns_count = 0;
    let mut total_dismissed_concerns_count = 0;

    for res in results {
        let p_idx = res["patch_index"].as_i64().unwrap_or(0);
        let patch_subject = patches_to_review
            .iter()
            .find(|p| p.index == p_idx)
            .and_then(|p| p.subject.as_ref())
            .cloned()
            .unwrap_or_default();

        if let Some(review) = res.get("review") {
            if let Some(findings) = review.get("findings").and_then(|v| v.as_array()) {
                for f in findings {
                    let mut finding_val = f.clone();
                    finding_val["patch_index"] = json!(p_idx);
                    finding_val["patch_subject"] = json!(patch_subject);
                    combined_findings.push(finding_val);
                }
            }
            if let Some(dismissed) = review.get("dismissed_concerns").and_then(|v| v.as_array()) {
                combined_dismissed_concerns.extend(dismissed.clone());
            }
            if let Some(cc) = review.get("concerns_count").and_then(|v| v.as_u64()) {
                total_concerns_count += cc;
            }
            if let Some(dcc) = review
                .get("dismissed_concerns_count")
                .and_then(|v| v.as_u64())
            {
                total_dismissed_concerns_count += dcc;
            }
        }

        if let Some(inline) = res["inline_review"].as_str()
            && !inline.trim().is_empty()
            && inline.trim() != "No issues found."
        {
            if !combined_inline.is_empty() {
                combined_inline.push_str("\n\n");
            }
            if patches_to_review.len() > 1 {
                combined_inline
                    .push_str(&format!("--- Patch [{}]: {} ---\n", p_idx, patch_subject));
            }
            combined_inline.push_str(inline.trim());
        }

        if let Some(hist) = res.get("history").and_then(|h| h.as_array()) {
            combined_history.extend(hist.clone());
        }

        if let Some(inp) = res["input_context"].as_str()
            && !inp.is_empty()
        {
            if !combined_input_context.is_empty() {
                combined_input_context.push_str("\n\n");
            }
            combined_input_context.push_str(inp);
        }

        total_tokens_in += res["tokens_in"].as_u64().unwrap_or(0);
        total_tokens_out += res["tokens_out"].as_u64().unwrap_or(0);
        total_tokens_cached += res["tokens_cached"].as_u64().unwrap_or(0);
    }

    let review_output = json!({
        "findings": combined_findings,
        "dismissed_concerns": combined_dismissed_concerns,
        "concerns_count": total_concerns_count,
        "dismissed_concerns_count": total_dismissed_concerns_count
    });

    let combined_result = json!({
        "patchset_id": patchset_id,
        "baseline": baseline_arg,
        "patches": patch_results,
        "review": review_output,
        "inline_review": if combined_inline.is_empty() { "No issues found.".to_string() } else { combined_inline },
        "history": combined_history,
        "input_context": combined_input_context,
        "tokens_in": total_tokens_in,
        "tokens_out": total_tokens_out,
        "tokens_cached": total_tokens_cached
    });

    Ok(combined_result)
}

pub async fn run_worker_from_stdin(options: WorkerOptions) -> Result<Value> {
    let mut buffer = String::new();
    if std::io::stdin().read_line(&mut buffer)? == 0 {
        return Err(anyhow!("No input provided on stdin"));
    }
    let input: ReviewInput = serde_json::from_str(&buffer)?;
    let repo_override = options.repo.clone();
    run_worker(input, options, repo_override, None).await
}

pub fn result_has_error(result: &Value) -> bool {
    result
        .get("error")
        .and_then(|e| e.as_str())
        .map(|e| !e.is_empty())
        .unwrap_or(false)
}

pub fn result_has_high_or_critical_findings(result: &Value) -> bool {
    let Some(findings) = result
        .get("review")
        .and_then(|review| review.get("findings"))
        .and_then(|f| f.as_array())
    else {
        return false;
    };

    findings.iter().any(|finding| {
        let is_new = !finding
            .get("preexisting")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let severity = finding
            .get("severity")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        is_new && matches!(severity.as_str(), "critical" | "high")
    })
}

async fn apply_single_patch(
    worktree: &GitWorktree,
    p: &PatchInput,
    patch_shas: &mut HashMap<i64, String>,
    patch_shows: &mut HashMap<i64, String>,
    patch_messages: &mut HashMap<i64, String>,
    patch_results: &mut Vec<Value>,
) -> bool {
    if let Some(sha) = &p.commit_id {
        info!(
            "Patch {} is identified by commit ID {}, attempting direct checkout...",
            p.index, sha
        );
        return checkout_patch(
            worktree,
            p,
            sha,
            "checkout",
            patch_shas,
            patch_shows,
            patch_messages,
            patch_results,
        )
        .await;
    }

    if let Some(sha) = &p.message_id
        && sha.len() == 40
        && sha.chars().all(|c| c.is_ascii_hexdigit())
    {
        info!(
            "Patch {} message_id looks like a SHA {}, checking out...",
            p.index, sha
        );
        return checkout_patch(
            worktree,
            p,
            sha,
            "checkout",
            patch_shas,
            patch_shows,
            patch_messages,
            patch_results,
        )
        .await;
    }

    if let (Some(author), Some(subject)) = (&p.author, &p.subject) {
        let date_str = if let Some(ts) = p.date {
            use chrono::{TimeZone, Utc};
            Utc.timestamp_opt(ts, 0)
                .single()
                .map(|dt| dt.to_rfc2822())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mbox = format!(
            "From: {}\nDate: {}\nSubject: {}\n\n{}\n",
            author, date_str, subject, p.diff
        );

        match worktree.apply_patch(&mbox).await {
            Ok(_) => {
                if let Ok(sha) = get_commit_hash(&worktree.path, "HEAD").await {
                    patch_shas.insert(p.index, sha.clone());
                    if let Ok(show) = worktree.get_commit_show(&sha).await {
                        patch_shows.insert(p.index, show);
                    }
                    if let Ok(msg) = worktree.get_commit_message(&sha).await {
                        patch_messages.insert(p.index, msg);
                    }
                }
                patch_results.push(json!({
                    "index": p.index,
                    "status": "applied",
                    "method": "git-am",
                    "subject": p.subject.clone()
                }));
                return true;
            }
            Err(e) => {
                error!("git am failed: {}", e);
                patch_results.push(json!({
                    "index": p.index,
                    "status": "error",
                    "method": "git-am",
                    "subject": p.subject.clone(),
                    "error": e.to_string()
                }));
                return false;
            }
        }
    }

    patch_results.push(json!({
        "index": p.index,
        "status": "error",
        "method": "unknown",
        "error": "Missing author or subject for am apply"
    }));
    false
}

#[allow(clippy::too_many_arguments)]
async fn checkout_patch(
    worktree: &GitWorktree,
    p: &PatchInput,
    sha: &str,
    method: &str,
    patch_shas: &mut HashMap<i64, String>,
    patch_shows: &mut HashMap<i64, String>,
    patch_messages: &mut HashMap<i64, String>,
    patch_results: &mut Vec<Value>,
) -> bool {
    match worktree.reset_hard(sha).await {
        Ok(_) => {
            if let Ok(show) = worktree.get_commit_show(sha).await {
                patch_shows.insert(p.index, show);
            }
            if let Ok(msg) = worktree.get_commit_message(sha).await {
                patch_messages.insert(p.index, msg);
            }
            patch_shas.insert(p.index, sha.to_string());
            patch_results.push(json!({
                "index": p.index,
                "status": "applied",
                "method": method,
                "subject": p.subject.clone()
            }));
            true
        }
        Err(e) => {
            error!("Failed to checkout commit {}: {}", sha, e);
            patch_results.push(json!({
                "index": p.index,
                "status": "error",
                "method": method,
                "subject": p.subject.clone(),
                "error": e.to_string()
            }));
            false
        }
    }
}

fn emit(progress: Option<&ProgressCallback<'_>>, event: ProgressEvent) {
    if let Some(progress) = progress {
        progress(event);
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

pub fn print_worker_json(result: &Value) -> Result<()> {
    println!("{}", serde_json::to_string(result)?);
    std::io::stdout().flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::process::Command;
    use std::sync::Arc;

    /// A provider that does nothing; the decoration tests only care about
    /// whether it was wrapped, which `Arc::ptr_eq` answers directly.
    struct StubProvider;

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for StubProvider {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> Result<crate::ai::AiResponse> {
            unreachable!("decoration tests never issue a request")
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "stub".into(),
                context_window_size: 1000,
            }
        }
    }

    fn stub() -> Arc<dyn crate::ai::AiProvider> {
        Arc::new(StubProvider)
    }

    /// Fails the first call with a rate limit, then succeeds, so a test can
    /// tell whether the retry limiter was actually installed.
    struct RateLimitOnce {
        calls: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for RateLimitOnce {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> Result<crate::ai::AiResponse> {
            use std::sync::atomic::Ordering;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(crate::ai::gemini::GeminiError::QuotaExceeded(
                    std::time::Duration::from_millis(5),
                )
                .into());
            }
            Ok(crate::ai::AiResponse {
                content: Some("ok".into()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "rate-limit-once".into(),
                context_window_size: 1000,
            }
        }
    }

    fn dummy_request() -> crate::ai::AiRequest {
        crate::ai::AiRequest {
            system: None,
            messages: vec![crate::ai::AiMessage {
                role: crate::ai::AiRole::User,
                content: Some("hi".into()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    #[tokio::test]
    async fn test_decorate_provider_retries_rate_limits_for_in_process_reviews() -> Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};
        let mut settings = Settings::new()?;
        settings.ai.log_turns = false;
        let sem = Arc::new(Semaphore::new(4));
        let quota = Arc::new(crate::ai::quota::QuotaManager::new());

        // In-process: the limiter waits out the window and retries, so the
        // caller sees a success rather than the rate-limit error.
        settings.ai.provider = "claude-cli".to_string();
        let inner = Arc::new(RateLimitOnce {
            calls: AtomicU32::new(0),
        });
        let decorated = decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None);
        let response = decorated.generate_content(dummy_request()).await?;
        assert_eq!(response.content.as_deref(), Some("ok"));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);

        // Daemon-spawned worker: the daemon owns the retry, so the error is
        // passed straight back instead of being retried here as well.
        settings.ai.provider = "stdio-claude".to_string();
        let inner = Arc::new(RateLimitOnce {
            calls: AtomicU32::new(0),
        });
        let decorated = decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None);
        assert!(decorated.generate_content(dummy_request()).await.is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_decorate_provider_honours_the_retry_budget() -> Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};

        /// Stands in for a review whose deadline has already passed.
        struct Expired;
        impl crate::ai::backoff_provider::RetryBudget for Expired {
            fn credit_wait(&self, _slept: std::time::Duration) {}
            fn check(&self) -> Result<()> {
                Err(anyhow!("deadline exceeded"))
            }
        }

        let mut settings = Settings::new()?;
        settings.ai.log_turns = false;
        settings.ai.provider = "claude-cli".to_string();
        let sem = Arc::new(Semaphore::new(4));
        let quota = Arc::new(crate::ai::quota::QuotaManager::new());
        let budget: Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>> =
            Some(Arc::new(Expired));

        let inner = Arc::new(RateLimitOnce {
            calls: AtomicU32::new(0),
        });
        let decorated = decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &budget);
        assert!(decorated.generate_content(dummy_request()).await.is_err());
        // The budget is consulted before the request goes out, so a review that
        // is already past its deadline stops rather than retrying through it.
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn test_decorate_provider_log_turns_gate() -> Result<()> {
        let mut settings = Settings::new()?;
        // A stdio provider skips the limiters, isolating the logging decision.
        settings.ai.provider = "stdio-gemini".to_string();
        let sem = Arc::new(Semaphore::new(1));
        let quota = Arc::new(crate::ai::quota::QuotaManager::new());

        // Off: the provider is handed back untouched, so a review that does not
        // ask for turn logging pays nothing for it.
        settings.ai.log_turns = false;
        let inner = stub();
        assert!(Arc::ptr_eq(
            &inner,
            &decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None)
        ));

        // On: wrapped, so the turns are logged.
        settings.ai.log_turns = true;
        let inner = stub();
        assert!(!Arc::ptr_eq(
            &inner,
            &decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None)
        ));
        Ok(())
    }

    #[test]
    fn test_decorate_provider_skips_limiters_for_stdio_workers() -> Result<()> {
        let mut settings = Settings::new()?;
        settings.ai.log_turns = false;
        let sem = Arc::new(Semaphore::new(1));
        let quota = Arc::new(crate::ai::quota::QuotaManager::new());

        // A daemon-spawned worker is throttled by the daemon, so it must be
        // left unwrapped rather than limited twice.
        settings.ai.provider = "stdio-claude".to_string();
        let inner = stub();
        assert!(Arc::ptr_eq(
            &inner,
            &decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None)
        ));

        // A review running in-process has nothing in front of it, so it gets
        // the limiter.
        settings.ai.provider = "claude-cli".to_string();
        let inner = stub();
        assert!(!Arc::ptr_eq(
            &inner,
            &decorate_provider(inner.clone(), &settings.ai, &sem, &quota, &None)
        ));
        Ok(())
    }

    fn git(repo_path: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    async fn test_repo() -> Result<(tempfile::TempDir, PathBuf, String, String)> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        git(&repo_path, &["init"])?;
        git(&repo_path, &["config", "user.email", "test@example.com"])?;
        git(&repo_path, &["config", "user.name", "Test User"])?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Initial")?;
        git(&repo_path, &["add", "."])?;
        git(&repo_path, &["commit", "-m", "Initial"])?;
        let initial_sha = get_commit_hash(&repo_path, "HEAD").await?;

        writeln!(file, "Change")?;
        git(&repo_path, &["add", "."])?;
        git(&repo_path, &["commit", "-m", "Feature"])?;
        let feature_sha = get_commit_hash(&repo_path, "HEAD").await?;

        Ok((temp_dir, repo_path, initial_sha, feature_sha))
    }

    #[tokio::test]
    async fn test_build_review_input_from_git_single_commit() -> Result<()> {
        let (_temp, repo_path, _initial_sha, feature_sha) = test_repo().await?;
        let (input, shas) = build_review_input_from_git(&repo_path, &feature_sha, None).await?;

        assert_eq!(shas, vec![feature_sha.clone()]);
        assert_eq!(input.id, 0);
        assert_eq!(input.patches.len(), 1);
        assert_eq!(input.patches[0].index, 1);
        assert_eq!(
            input.patches[0].commit_id.as_deref(),
            Some(feature_sha.as_str())
        );
        assert_eq!(input.patches[0].subject.as_deref(), Some("Feature"));

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_remote_checkout() -> Result<()> {
        let (_temp, repo_path, initial_sha, feature_sha) = test_repo().await?;
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "INVALID DIFF content that would fail git apply".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some("some-msg-id".to_string()),
            commit_id: Some(feature_sha),
        };

        let mut patch_shas = HashMap::new();
        let mut patch_shows = HashMap::new();
        let mut patch_messages = HashMap::new();
        let mut patch_results = Vec::new();

        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        assert!(success);
        assert_eq!(patch_results[0]["status"], "applied");
        assert_eq!(patch_results[0]["method"], "checkout");
        assert!(std::fs::read_to_string(worktree.path.join("file.txt"))?.contains("Change"));

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_checkout_failure() -> Result<()> {
        let (_temp, repo_path, initial_sha, _feature_sha) = test_repo().await?;
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "Valid Diff content that would apply if we fell back".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some("some-msg-id".to_string()),
            commit_id: Some("0000000000000000000000000000000000000000".to_string()),
        };

        let mut patch_shas = HashMap::new();
        let mut patch_shows = HashMap::new();
        let mut patch_messages = HashMap::new();
        let mut patch_results = Vec::new();

        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        assert!(!success);
        assert_eq!(patch_results[0]["status"], "error");
        assert_eq!(patch_results[0]["method"], "checkout");

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_legacy_message_id_sha() -> Result<()> {
        let (_temp, repo_path, initial_sha, feature_sha) = test_repo().await?;
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "INVALID".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some(feature_sha),
            commit_id: None,
        };

        let mut patch_shas = HashMap::new();
        let mut patch_shows = HashMap::new();
        let mut patch_messages = HashMap::new();
        let mut patch_results = Vec::new();

        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        assert!(success);
        assert_eq!(patch_results[0]["status"], "applied");
        assert_eq!(patch_results[0]["method"], "checkout");
        assert!(std::fs::read_to_string(worktree.path.join("file.txt"))?.contains("Change"));

        Ok(())
    }

    #[test]
    fn test_series_range_in_local_review() {
        let p1 = PatchInput {
            index: 1,
            diff: "diff1".to_string(),
            subject: Some("Patch 1".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "diff2".to_string(),
            subject: Some("Patch 2".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let all_patches = vec![p1.clone(), p2.clone()];
        let mut patch_shas = HashMap::new();
        patch_shas.insert(1, "sha1".to_string());
        patch_shas.insert(2, "sha2".to_string());

        let range_p1 = calculate_series_range(
            &all_patches,
            std::slice::from_ref(&p1),
            &patch_shas,
            "baseline_sha",
        );
        assert_eq!(range_p1, Some("baseline_sha..sha2".to_string()));

        let range_p2 = calculate_series_range(
            &all_patches,
            std::slice::from_ref(&p2),
            &patch_shas,
            "baseline_sha",
        );
        assert_eq!(range_p2, None);
    }

    #[test]
    fn test_rich_patches_and_follow_up_context_with_patch_index_filter() {
        use crate::worker::build_follow_up_series_context;

        let p1 = PatchInput {
            index: 1,
            diff: "diff1".to_string(),
            subject: Some("Patch 1".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "diff2".to_string(),
            subject: Some("Patch 2".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let all_patches = vec![p1.clone(), p2.clone()];
        let mut patch_shas = HashMap::new();
        patch_shas.insert(1, "sha1".to_string());
        patch_shas.insert(2, "sha2".to_string());

        // When reviewing only patch 1 with index filter:
        let rich_patches: Vec<Value> = all_patches
            .iter()
            .map(|p| {
                json!({
                    "index": p.index,
                    "subject": p.subject,
                    "author": p.author,
                    "commit_id": patch_shas.get(&p.index).cloned(),
                })
            })
            .collect();

        let patchset_val = json!({
            "id": 100,
            "subject": "Series Subject",
            "patches": rich_patches,
            "patch_index": Some(1),
            "baseline": "baseline_sha"
        });

        let range = calculate_series_range(
            &all_patches,
            std::slice::from_ref(&p1),
            &patch_shas,
            "baseline_sha",
        );
        assert_eq!(range, Some("baseline_sha..sha2".to_string()));

        let context = build_follow_up_series_context(range.as_deref(), &patchset_val, "sha1");
        assert!(context.is_some());
        let ctx_str = context.unwrap();
        assert!(ctx_str.contains("Current Patch Under Review: [Patch 1 of 2] - Patch 1"));
        assert!(ctx_str.contains("Series End Commit (Final State): sha2"));
        assert!(ctx_str.contains("- [Patch 2 of 2] (commit sha2): Patch 2"));
    }

    #[test]
    fn test_inline_review_aggregation_single_and_multi_patch() {
        let single_patch = [PatchInput {
            index: 1,
            diff: "diff1".to_string(),
            subject: Some("[PATCH 1/1] test single".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: None,
        }];
        let single_res = [json!({
            "patch_index": 1,
            "inline_review": "> +int x;\n+Use unsigned int instead.",
        })];

        let mut combined_single = String::new();
        for res in &single_res {
            let p_idx = res["patch_index"].as_i64().unwrap_or(0);
            let patch_subject = single_patch
                .iter()
                .find(|p| p.index == p_idx)
                .and_then(|p| p.subject.as_ref())
                .cloned()
                .unwrap_or_default();
            if let Some(inline) = res["inline_review"].as_str()
                && !inline.trim().is_empty()
                && inline.trim() != "No issues found."
            {
                if !combined_single.is_empty() {
                    combined_single.push_str("\n\n");
                }
                if single_patch.len() > 1 {
                    combined_single
                        .push_str(&format!("--- Patch [{}]: {} ---\n", p_idx, patch_subject));
                }
                combined_single.push_str(inline.trim());
            }
        }
        assert_eq!(combined_single, "> +int x;\n+Use unsigned int instead.");
        assert!(!combined_single.contains("--- Patch [1]:"));

        let multi_patches = [
            PatchInput {
                index: 1,
                diff: "diff1".to_string(),
                subject: Some("[PATCH 1/2] test first".to_string()),
                author: None,
                date: None,
                message_id: None,
                commit_id: None,
            },
            PatchInput {
                index: 2,
                diff: "diff2".to_string(),
                subject: Some("[PATCH 2/2] test second".to_string()),
                author: None,
                date: None,
                message_id: None,
                commit_id: None,
            },
        ];
        let multi_res = [
            json!({
                "patch_index": 1,
                "inline_review": "Comment on patch 1",
            }),
            json!({
                "patch_index": 2,
                "inline_review": "Comment on patch 2",
            }),
        ];

        let mut combined_multi = String::new();
        for res in &multi_res {
            let p_idx = res["patch_index"].as_i64().unwrap_or(0);
            let patch_subject = multi_patches
                .iter()
                .find(|p| p.index == p_idx)
                .and_then(|p| p.subject.as_ref())
                .cloned()
                .unwrap_or_default();
            if let Some(inline) = res["inline_review"].as_str()
                && !inline.trim().is_empty()
                && inline.trim() != "No issues found."
            {
                if !combined_multi.is_empty() {
                    combined_multi.push_str("\n\n");
                }
                if multi_patches.len() > 1 {
                    combined_multi
                        .push_str(&format!("--- Patch [{}]: {} ---\n", p_idx, patch_subject));
                }
                combined_multi.push_str(inline.trim());
            }
        }
        assert!(
            combined_multi
                .contains("--- Patch [1]: [PATCH 1/2] test first ---\nComment on patch 1")
        );
        assert!(
            combined_multi
                .contains("--- Patch [2]: [PATCH 2/2] test second ---\nComment on patch 2")
        );
    }
}
