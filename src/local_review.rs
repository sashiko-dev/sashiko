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
    worker::{PatchInput, ReviewInput, Worker, WorkerConfig, prompts::PromptRegistry},
};
use anyhow::{Context, Result, anyhow};
use futures::stream::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};
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
    let (mut ai, configured_repo_path, concurrency) = if let Some(path) = &options.settings_path {
        let local_settings = Settings::local_review_from_file(path)
            .with_context(|| format!("Failed to load settings from {}", path.display()))?;
        let concurrency = local_settings
            .review
            .and_then(|r| r.concurrency)
            .unwrap_or(4);
        (local_settings.ai, None, concurrency)
    } else if repo_override.is_some() {
        let local_settings =
            Settings::local_review_settings().context("Failed to load local review settings")?;
        let concurrency = local_settings
            .review
            .and_then(|r| r.concurrency)
            .unwrap_or(4);
        (local_settings.ai, None, concurrency)
    } else {
        let settings = Settings::new().context("Failed to load settings")?;
        (
            settings.ai,
            Some(PathBuf::from(settings.git.repository_path)),
            settings.review.concurrency,
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
async fn review_single_patch(
    worktree: &GitWorktree,
    ai: &AiSettings,
    patchset_id: i64,
    subject: &str,
    p: &PatchInput,
    rich_patches: &[Value],
    patch_shas: &HashMap<i64, String>,
    options: &WorkerOptions,
    baseline_sha: &str,
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
        let series_range = patch_shas
            .get(&p.index)
            .map(|sha| format!("{}..{}", baseline_sha, sha));

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
            "patch_index": Some(p.index)
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

    let rich_patches: Vec<Value> = patches_to_review
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

    // Execute patch reviews concurrently with a limit
    let futures_stream = futures::stream::iter(patches_to_review.iter().map(|p| {
        let rich_patches = rich_patches.clone();
        let patch_shas = &patch_shas;
        let options = &options;
        let subject_clone = subject.clone();
        async move {
            review_single_patch(
                worktree,
                ai,
                patchset_id,
                &subject_clone,
                p,
                &rich_patches,
                patch_shas,
                options,
                baseline_sha,
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

    // Aggregate findings and inline reviews
    let mut combined_findings = Vec::new();
    let mut combined_inline = String::new();
    let mut total_tokens_in = 0;
    let mut total_tokens_out = 0;
    let mut total_tokens_cached = 0;

    for res in results {
        let p_idx = res["patch_index"].as_i64().unwrap_or(0);
        let patch_subject = patches_to_review
            .iter()
            .find(|p| p.index == p_idx)
            .and_then(|p| p.subject.as_ref())
            .cloned()
            .unwrap_or_default();

        if let Some(review) = res.get("review")
            && let Some(findings) = review.get("findings").and_then(|v| v.as_array())
        {
            for f in findings {
                let mut finding_val = f.clone();
                finding_val["patch_index"] = json!(p_idx);
                finding_val["patch_subject"] = json!(patch_subject);
                combined_findings.push(finding_val);
            }
        }

        if let Some(inline) = res["inline_review"].as_str()
            && !inline.trim().is_empty()
            && inline.trim() != "No issues found."
        {
            if !combined_inline.is_empty() {
                combined_inline.push_str("\n\n");
            }
            combined_inline.push_str(&format!("--- Patch [{}]: {} ---\n", p_idx, patch_subject));
            combined_inline.push_str(inline.trim());
        }

        total_tokens_in += res["tokens_in"].as_u64().unwrap_or(0);
        total_tokens_out += res["tokens_out"].as_u64().unwrap_or(0);
        total_tokens_cached += res["tokens_cached"].as_u64().unwrap_or(0);
    }

    let review_output = json!({
        "findings": combined_findings
    });

    let combined_result = json!({
        "patchset_id": patchset_id,
        "baseline": baseline_arg,
        "patches": patch_results,
        "review": review_output,
        "inline_review": if combined_inline.is_empty() { "No issues found.".to_string() } else { combined_inline },
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
}
