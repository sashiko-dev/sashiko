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

use crate::ReviewStatus;
use crate::ai::cache::CacheManager;
use crate::ai::proxy::QuotaManager;
use crate::ai::{AiProvider, AiRequest, AiResponse, create_provider};
use crate::baseline::{BaselineRegistry, BaselineResolution, extract_files_from_diff};
use crate::db::{AiInteractionParams, Database, Finding, PatchsetRow, Severity, ToolUsage};
use crate::git_ops::{GitWorktree, ensure_remote, get_commit_hash};
use crate::settings::Settings;
use crate::utils::redact_secret;
use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore};
use tracing::{error, info, warn};

#[derive(Clone)]
struct ReviewContext {
    db: Arc<Database>,
    settings: Settings,
    baseline_registry: Arc<BaselineRegistry>,
    quota_manager: Arc<QuotaManager>,
    cache_manager: Arc<CacheManager>,
    active_cache_name: Arc<Mutex<Option<String>>>,
    current_cache_name: Option<String>,
    target_review_count: usize,
    provider: Arc<dyn AiProvider>,
}

enum PatchResult {
    Success,
    ApplyFailed,
    ReviewFailed,
}

#[derive(Serialize)]
struct BaselineAttempt {
    baseline: String,
    status: String,
    log: String,
}

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");
    format!("{:x}-{:x}", since_the_epoch.as_micros(), std::process::id())
}

/// The `Reviewer` service orchestrates the review process for patchsets.
///
/// It manages:
/// - Baseline resolution and worktree preparation.
/// - AI-based code review execution.
/// - Patch application verification.
/// - Interaction with the database and external tools.
pub struct Reviewer {
    db: Arc<Database>,
    settings: Settings,
    semaphore: Arc<Semaphore>,
    baseline_registry: Arc<BaselineRegistry>,
    quota_manager: Arc<QuotaManager>,
    cache_manager: Arc<CacheManager>,
    active_cache_name: Arc<Mutex<Option<String>>>,
    provider: Arc<dyn AiProvider>,
}

use crate::worker::tools::ToolBox;

impl Reviewer {
    /// Creates a new `Reviewer` instance.
    ///
    /// # Arguments
    ///
    /// * `db` - The database connection.
    /// * `settings` - Application settings.
    pub fn new(db: Arc<Database>, settings: Settings) -> Self {
        let concurrency = settings.review.concurrency;
        let repo_path = PathBuf::from(&settings.git.repository_path);

        let baseline_registry = match BaselineRegistry::new(&repo_path) {
            Ok(r) => Arc::new(r),
            Err(e) => {
                error!(
                    "Failed to initialize BaselineRegistry: {}. Using empty registry.",
                    e
                );
                Arc::new(BaselineRegistry::new(&repo_path).unwrap_or_else(|_| {
                    panic!("Critical error initializing BaselineRegistry: {}", e)
                }))
            }
        };

        // Initialize Provider
        let provider = create_provider(&settings).expect("Failed to create AI provider");

        // Initialize CacheManager
        // Assuming prompts are in "third_party/prompts/kernel" in CWD.
        let prompts_dir = PathBuf::from("third_party/prompts/kernel");

        // We need tool definitions for the cache.
        // We use dummy paths for ToolBox here because we only need declarations,
        // not execution capability.
        let tools_def = ToolBox::new(PathBuf::from("."), None).get_declarations_generic();

        let cache_manager = Arc::new(CacheManager::new(
            prompts_dir,
            provider.clone(),
            settings.ai.model.clone(),
            "3600s".to_string(),
            Some(tools_def),
        ));

        Self {
            db,
            settings,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            baseline_registry,
            quota_manager: Arc::new(QuotaManager::new()),
            cache_manager,
            active_cache_name: Arc::new(Mutex::new(None)),
            provider,
        }
    }

    /// Starts the reviewer service loop.
    ///
    /// This method runs indefinitely, polling the database for pending patchsets
    /// and processing them. It handles concurrency limits and worktree cleanup.
    pub async fn start(&self) {
        info!(
            "Starting Reviewer service with concurrency limit: {}",
            self.settings.review.concurrency
        );

        if self.settings.ai.no_ai {
            info!(
                "AI interactions disabled via settings. Reviewer service will skip AI analysis but verify patch application."
            );
        }

        // Ensure Context Cache
        if self
            .settings
            .ai
            .gemini
            .as_ref()
            .map(|g| g.explicit_prompt_caching)
            .unwrap_or(false)
        {
            match self.cache_manager.ensure_cache(None).await {
                Ok(name) => {
                    info!("AI Context Cache initialized: {}", name);
                    let mut guard = self.active_cache_name.lock().await;
                    *guard = Some(name);
                }
                Err(e) => {
                    error!(
                        "Failed to initialize AI Context Cache: {}. Proceeding without cache (higher cost/latency).",
                        e
                    );
                }
            }
        } else {
            info!("Explicit caching disabled via settings.");
        }

        let worktree_dir = PathBuf::from(&self.settings.review.worktree_dir);
        if worktree_dir.exists() {
            info!(
                "Cleaning up previous worktree directory: {:?}",
                worktree_dir
            );
            if let Err(e) = std::fs::remove_dir_all(&worktree_dir) {
                error!("Failed to cleanup worktree directory: {}", e);
            }
        }
        if let Err(e) = std::fs::create_dir_all(&worktree_dir) {
            error!("Failed to create worktree directory: {}", e);
        }

        match self.db.reset_reviewing_status().await {
            Ok(count) => {
                if count > 0 {
                    info!("Recovered {} interrupted reviews (reset to Pending)", count);
                }
            }
            Err(e) => error!("Failed to reset reviewing status: {}", e),
        }

        loop {
            match self.process_pending_patchsets().await {
                Ok(_) => {}
                Err(e) => error!("Error in reviewer loop: {}", e),
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        }
    }

    async fn process_pending_patchsets(&self) -> Result<()> {
        let patchsets = self.db.get_pending_patchsets(10).await?;

        if patchsets.is_empty() {
            return Ok(());
        }

        info!("Found {} pending patchsets for review", patchsets.len());

        let current_cache_name = self.active_cache_name.lock().await.clone();

        for patchset in patchsets {
            let permit = self.semaphore.clone().acquire_owned().await?;
            let target_review_count = patchset.target_review_count.unwrap_or(1) as usize;

            let context = ReviewContext {
                db: self.db.clone(),
                settings: self.settings.clone(),
                baseline_registry: self.baseline_registry.clone(),
                quota_manager: self.quota_manager.clone(),
                cache_manager: self.cache_manager.clone(),
                active_cache_name: self.active_cache_name.clone(),
                current_cache_name: current_cache_name.clone(),
                target_review_count,
                provider: self.provider.clone(),
            };

            tokio::spawn(async move {
                let _permit = permit;
                Self::review_patchset_task(context, patchset).await;
            });
        }

        Ok(())
    }

    async fn review_patchset_task(ctx: ReviewContext, patchset: PatchsetRow) {
        let patchset_id = patchset.id;
        info!("Starting review for patchset {}", patchset_id);

        if let Err(e) = ctx
            .db
            .update_patchset_status(patchset_id, ReviewStatus::InReview.as_str())
            .await
        {
            error!(
                "Failed to update status to In Review for {}: {}",
                patchset_id, e
            );
            return;
        }

        let diffs = match ctx.db.get_patch_diffs(patchset_id).await {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to fetch diffs for {}: {}", patchset_id, e);
                let _ = ctx.db.update_patchset_status(patchset_id, "Failed").await;
                return;
            }
        };

        // patches_json for input payload (contains all patches)
        let patches_json: Vec<_> = diffs
            .iter()
            .map(|(_id, idx, diff, subj, auth, date, msg_id)| {
                let is_sha = msg_id.len() == 40 && msg_id.chars().all(|c| c.is_ascii_hexdigit());
                json!({
                    "index": idx,
                    "diff": diff,
                    "subject": subj,
                    "author": auth,
                    "date": date,
                    "message_id": msg_id,
                    "commit_id": if is_sha { Some(msg_id) } else { None }
                })
            })
            .collect();

        // Determine Baseline Candidates
        let mut all_files = Vec::new();
        for p in patches_json.iter() {
            if let Some(diff_str) = p["diff"].as_str() {
                let files = extract_files_from_diff(diff_str);
                all_files.extend(files);
            }
        }

        let body = if let Some(mid) = &patchset.message_id {
            ctx.db.get_message_body(mid).await.unwrap_or(None)
        } else if let Some(first_patch_msg_id) =
            patches_json.first().and_then(|p| p["message_id"].as_str())
        {
            ctx.db
                .get_message_body(first_patch_msg_id)
                .await
                .unwrap_or(None)
        } else {
            None
        };

        let subject = patchset.subject.clone().unwrap_or("Unknown".to_string());
        let candidates = if let Some(bid) = patchset.baseline_id {
            if let Ok(Some(commit)) = ctx.db.get_baseline_commit(bid).await {
                info!(
                    "Using forced baseline commit {} from ingestion for patchset {}",
                    commit, patchset_id
                );
                vec![BaselineResolution::Commit(commit)]
            } else {
                ctx.baseline_registry
                    .resolve_candidates(&all_files, &subject, body.as_deref())
            }
        } else {
            ctx.baseline_registry
                .resolve_candidates(&all_files, &subject, body.as_deref())
        };

        // 1. Find a working baseline (apply series)
        let (found_baseline, patch_commits, logs) =
            Self::prepare_baseline_worktree(&ctx, patchset_id, &candidates, &diffs).await;

        let prompts_hash = get_commit_hash(Path::new("."), "HEAD").await.ok();

        // Save findings to patchset
        if let Some((resolution, baseline_id, worktree)) = found_baseline {
            let _ = ctx
                .db
                .update_patchset_baseline_info(
                    patchset_id,
                    Some(baseline_id),
                    Some(ctx.settings.ai.model.as_str()),
                    prompts_hash.as_deref(),
                    Some(logs.as_str()),
                    Some(ctx.settings.ai.provider.as_str()),
                )
                .await;

            // patches_json for input payload (contains all patches)
            let patches_json: Vec<_> = diffs
                .iter()
                .map(|(_id, idx, diff, subj, auth, date, msg_id)| {
                    let resolved_sha = patch_commits.get(idx);
                    let is_msg_sha =
                        msg_id.len() == 40 && msg_id.chars().all(|c| c.is_ascii_hexdigit());

                    let commit_id = if let Some(sha) = resolved_sha {
                        Some(sha.as_str())
                    } else if is_msg_sha {
                        Some(msg_id.as_str())
                    } else {
                        None
                    };

                    json!({
                        "index": idx,
                        "diff": diff,
                        "subject": subj,
                        "author": auth,
                        "date": date,
                        "message_id": msg_id,
                        "commit_id": commit_id
                    })
                })
                .collect();

            let input_payload = json!({
                "id": patchset_id,
                "subject": patchset.subject.clone().unwrap_or("Unknown".to_string()),
                "patches": patches_json
            });

            let skip_filters: Vec<String> = patchset
                .skip_filters
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let only_filters: Vec<String> = patchset
                .only_filters
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();

            let compile_glob = |pattern: &str| -> regex::Regex {
                let mut re = String::from("^");
                for c in pattern.chars() {
                    match c {
                        '*' => re.push_str(".*"),
                        '?' => re.push('.'),
                        '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\' => {
                            re.push('\\');
                            re.push(c);
                        }
                        _ => re.push(c),
                    }
                }
                re.push('$');
                regex::Regex::new(&re).unwrap_or_else(|_| regex::Regex::new("a^").unwrap())
            };

            let skip_regexes: Vec<_> = skip_filters.iter().map(|f| compile_glob(f)).collect();
            let only_regexes: Vec<_> = only_filters.iter().map(|f| compile_glob(f)).collect();

            // 2. Run Reviews
            let mut review_success = true; // Optimistic
            let mut failed_patches = 0;

            for (patch_id, index, diff, _subj, _auth, _date, _msg_id) in &diffs {
                let mut should_skip = false;

                // Opt-out logic
                if skip_regexes.iter().any(|re| re.is_match(_subj)) {
                    info!("Skipping patch {} (subject matches skip filter)", patch_id);
                    should_skip = true;
                }

                // Opt-in logic (if only_filters is not empty, subject MUST match at least one)
                if !should_skip
                    && !only_regexes.is_empty()
                    && !only_regexes.iter().any(|re| re.is_match(_subj))
                {
                    info!(
                        "Skipping patch {} (subject does not match any only filter)",
                        patch_id
                    );
                    should_skip = true;
                }

                if should_skip {
                    let _ = ctx.db.update_patch_status(*patch_id, "Skipped").await;
                    continue;
                }
                let commit_sha = patch_commits.get(index).cloned();
                let baseline_ref = resolution.as_str();

                if let Some(sha) = &commit_sha {
                    if let Ok(true) = worktree.is_merge_commit(sha).await {
                        info!(
                            "Skipping review for merge commit {} (ps={} idx={})",
                            sha, patchset_id, index
                        );
                        continue;
                    }
                    if let Ok(true) = worktree.is_empty_commit(sha).await {
                        info!(
                            "Skipping review for empty commit {} (ps={} idx={})",
                            sha, patchset_id, index
                        );
                        continue;
                    }
                }

                match Self::process_patch_review(
                    &ctx,
                    patchset_id,
                    *patch_id,
                    *index,
                    &baseline_ref,
                    Some(baseline_id),
                    &input_payload,
                    commit_sha,
                    prompts_hash.as_deref(),
                    Some(&worktree.path),
                    diff,
                )
                .await
                {
                    Ok(PatchResult::Success) => {}
                    _ => {
                        review_success = false;
                        failed_patches += 1;
                    }
                }
            }

            // Cleanup worktree here since we kept it alive for reuse
            let _ = worktree.remove().await;

            let final_status = if review_success {
                ReviewStatus::Reviewed.as_str().to_string()
            } else if failed_patches == diffs.len() {
                ReviewStatus::Failed.as_str().to_string() // All failed
            } else {
                ReviewStatus::Reviewed.as_str().to_string() // Partial success
            };

            let _ = ctx
                .db
                .update_patchset_status(patchset_id, &final_status)
                .await;
        } else {
            // No baseline found
            warn!("No working baseline found for patchset {}", patchset_id);
            let _ = ctx
                .db
                .update_patchset_baseline_info(
                    patchset_id,
                    None,
                    Some(ctx.settings.ai.model.as_str()),
                    prompts_hash.as_deref(),
                    Some(logs.as_str()),
                    Some(ctx.settings.ai.provider.as_str()),
                )
                .await;

            let _ = ctx
                .db
                .update_patchset_status(patchset_id, ReviewStatus::FailedToApply.as_str())
                .await;
        }
    }

    async fn prepare_baseline_worktree(
        ctx: &ReviewContext,
        patchset_id: i64,
        candidates: &[BaselineResolution],
        diffs: &[(i64, i64, String, String, String, i64, String)],
    ) -> (
        Option<(BaselineResolution, i64, GitWorktree)>,
        HashMap<i64, String>,
        String,
    ) {
        let mut attempts: Vec<BaselineAttempt> = Vec::new();
        let repo_path = PathBuf::from(&ctx.settings.git.repository_path);

        for candidate in candidates {
            let baseline_ref = candidate.as_str();
            let mut current_log = format!("Trying baseline: {}\n", baseline_ref);
            let mut current_status = "Failed".to_string();

            // Check remote
            if let BaselineResolution::RemoteTarget { url, name, .. } = candidate {
                if let Err(e) = ensure_remote(&repo_path, name, url, false).await {
                    let msg = format!("Failed to fetch remote {}: {}\n", redact_secret(url), e);
                    current_log.push_str(&msg);
                    error!("{}", msg.trim());
                    attempts.push(BaselineAttempt {
                        baseline: baseline_ref.clone(),
                        status: current_status,
                        log: current_log,
                    });
                    continue;
                }
            }

            // Resolve SHA
            let baseline_sha = match get_commit_hash(&repo_path, &baseline_ref).await {
                Ok(sha) => sha,
                Err(e) => {
                    let msg = format!("Failed to resolve baseline ref {}: {}\n", baseline_ref, e);
                    current_log.push_str(&msg);
                    attempts.push(BaselineAttempt {
                        baseline: baseline_ref.clone(),
                        status: current_status,
                        log: current_log,
                    });
                    continue;
                }
            };

            // Worktree
            let worktree = match GitWorktree::new(
                &repo_path,
                &baseline_sha,
                Some(Path::new(&ctx.settings.review.worktree_dir)),
            )
            .await
            {
                Ok(wt) => wt,
                Err(e) => {
                    let msg = format!("Failed to create worktree: {}\n", e);
                    current_log.push_str(&msg);
                    attempts.push(BaselineAttempt {
                        baseline: baseline_ref.clone(),
                        status: current_status,
                        log: current_log,
                    });
                    continue;
                }
            };

            // Apply patches
            let mut patch_commits = HashMap::new();
            let mut application_failed = false;
            let mut apply_logs = String::new();

            for (i, (patch_id, index, diff, subject, author, date_ts, msg_id)) in
                diffs.iter().enumerate()
            {
                let date_str = std::process::Command::new("date")
                    .arg("-R")
                    .arg("-d")
                    .arg(format!("@{}", date_ts))
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                let mut applied = false;
                let mut fast_path_taken = false;

                // Optimization: If message_id is a valid SHA, just checkout it
                if msg_id.len() == 40 && msg_id.chars().all(|c| c.is_ascii_hexdigit()) {
                    let next_is_sha = diffs
                        .get(i + 1)
                        .map(|(_, _, _, _, _, _, next_msg)| {
                            next_msg.len() == 40 && next_msg.chars().all(|c| c.is_ascii_hexdigit())
                        })
                        .unwrap_or(true); // If there is no next item, we treat it as "safe to skip reset" (we are done)

                    if next_is_sha {
                        // Fast path: verify existence only, skip checkout
                        match get_commit_hash(&worktree.path, msg_id).await {
                            Ok(_) => {
                                applied = true;
                                fast_path_taken = true;
                            }
                            Err(e) => {
                                let msg = format!("Commit {} missing: {}\n", msg_id, e);
                                info!("{}", msg);
                                apply_logs.push_str(&msg);
                            }
                        }
                    } else {
                        match worktree.reset_hard(msg_id).await {
                            Ok(_) => applied = true,
                            Err(e) => {
                                let msg = format!("Failed to reset hard to {}: {}\n", msg_id, e);
                                info!("{}", msg);
                                apply_logs.push_str(&msg);
                            }
                        }
                    }
                }

                if !applied {
                    let mbox = format!(
                        "From: {}\nDate: {}\nSubject: {}\n\n{}\n",
                        author, date_str, subject, diff
                    );

                    // Try git am
                    if (worktree.apply_patch(&mbox).await).is_ok() {
                        applied = true;
                    } else {
                        // Fallback raw diff
                        if let Ok(o) = worktree.apply_raw_diff(diff).await {
                            if o.status.success() {
                                applied = true;
                                // Commit raw diff
                                let _ = Command::new("git")
                                    .current_dir(&worktree.path)
                                    .args(["add", "."])
                                    .output()
                                    .await;
                                let commit_msg = format!("{}\n\n(Applied via git apply)", subject);
                                let _ = Command::new("git")
                                    .current_dir(&worktree.path)
                                    .env("GIT_AUTHOR_NAME", author)
                                    .env("GIT_AUTHOR_EMAIL", "sashiko@localhost")
                                    .args(["commit", "-m", &commit_msg])
                                    .output()
                                    .await;
                            }
                        }
                    }
                }

                if applied {
                    if fast_path_taken {
                        patch_commits.insert(*index, msg_id.clone());
                    } else if let Ok(sha) = get_commit_hash(&worktree.path, "HEAD").await {
                        patch_commits.insert(*index, sha);
                    }
                } else {
                    let msg = format!(
                        "Patch {}/{} (ID: {}) failed to apply.\n",
                        patchset_id, index, patch_id
                    );
                    apply_logs.push_str(&msg);
                    application_failed = true;
                    break;
                }
            }

            if !application_failed {
                current_log.push_str("Application successful.\n");
                current_status = "Applied".to_string();

                attempts.push(BaselineAttempt {
                    baseline: baseline_ref.clone(),
                    status: current_status,
                    log: current_log,
                });

                // Create baseline in DB
                let baseline_id = {
                    let (repo_url, branch) = match candidate {
                        BaselineResolution::RemoteTarget { url, .. } => {
                            (Some(url.as_str()), Some(baseline_ref.as_str()))
                        }
                        _ => (None, Some(baseline_ref.as_str())),
                    };
                    ctx.db
                        .create_baseline(repo_url, branch, Some(&baseline_sha))
                        .await
                        .ok() // If fail, we just proceed. Better to have it.
                };

                // Serialize attempts to JSON
                let logs_json = serde_json::to_string(&attempts).unwrap_or_default();

                if let Some(bid) = baseline_id {
                    info!(
                        "Baseline found for patchset {}: {} ({} attempts)",
                        patchset_id,
                        candidate.as_str(),
                        attempts.len()
                    );
                    return (
                        Some((candidate.clone(), bid, worktree)),
                        patch_commits,
                        logs_json,
                    );
                } else {
                    // Fallback if DB insert fails, though unlikely
                    // We still return success but maybe log error.
                    // We do not continue loop as application succeeded.
                    // Just return success without ID.
                    // This path is tricky. Let's assume ID creation works or we fail this attempt.
                    // If we fail, we clean up.
                    // For now, let's treat it as success but maybe missing ID is fatal for `Some` return.
                    // But `create_baseline` returns Result<i64>.
                    // If it fails, we can't associate baseline.
                    // Let's count it as failure.
                    // Re-push attempt with failure.
                    // Actually we already pushed "Applied".
                    // Let's modify the last attempt status if we can't save to DB.
                    if let Some(last) = attempts.last_mut() {
                        last.status = "DB Error".to_string();
                        last.log.push_str("Failed to record baseline in DB.\n");
                    }
                }
            } else {
                current_log.push_str(&apply_logs);
                current_log.push_str("Application failed.\n");
                attempts.push(BaselineAttempt {
                    baseline: baseline_ref.clone(),
                    status: current_status,
                    log: current_log,
                });
            }

            // Clean up failed worktree
            let _ = worktree.remove().await;
        }

        let logs_json = serde_json::to_string(&attempts).unwrap_or_default();
        (None, HashMap::new(), logs_json)
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_patch_review(
        ctx: &ReviewContext,
        patchset_id: i64,
        patch_id: i64,
        index: i64,
        baseline_ref: &str,
        baseline_id: Option<i64>,
        input_payload: &Value,
        commit_sha: Option<String>,
        prompts_hash: Option<&str>,
        worktree_path: Option<&Path>,
        diff: &str,
    ) -> Result<PatchResult> {
        info!(
            "Reviewing patch {}/{} (ID: {})",
            patchset_id, index, patch_id
        );

        let successful_count = ctx
            .db
            .count_successful_reviews(patchset_id, patch_id, baseline_id)
            .await?;

        if successful_count >= ctx.target_review_count {
            info!(
                "Patch {}/{} (ID: {}) already has {} successful reviews with baseline {:?} (target: {}). Skipping.",
                patchset_id,
                index,
                patch_id,
                successful_count,
                baseline_id,
                ctx.target_review_count
            );
            return Ok(PatchResult::Success);
        }

        let files = extract_files_from_diff(diff);
        if !files.is_empty()
            && files.iter().all(|f| {
                ctx.settings
                    .review
                    .ignore_files
                    .iter()
                    .any(|ignored| f.starts_with(ignored))
            })
        {
            info!(
                "Skipping review for patch {}/{} (ID: {}) as it touches only ignored files.",
                patchset_id, index, patch_id
            );

            let review_id = ctx
                .db
                .create_review(
                    patchset_id,
                    Some(patch_id),
                    &ctx.settings.ai.provider,
                    &ctx.settings.ai.model,
                    baseline_id,
                    prompts_hash,
                )
                .await?;

            let _ = ctx
                .db
                .complete_review(
                    review_id,
                    ReviewStatus::Skipped.as_str(),
                    "Skipped: touches only ignored files",
                    None,
                    None,
                    None,
                    None,
                )
                .await;

            return Ok(PatchResult::Success);
        }

        let mut retries = 0;
        let max_retries = ctx.settings.review.max_retries;

        loop {
            let review_id = ctx
                .db
                .create_review(
                    patchset_id,
                    Some(patch_id),
                    &ctx.settings.ai.provider,
                    &ctx.settings.ai.model,
                    baseline_id,
                    prompts_hash,
                )
                .await?;

            let _ = ctx
                .db
                .update_review_status(review_id, ReviewStatus::InReview.as_str(), None)
                .await;

            let result = run_review_tool(
                patchset_id,
                input_payload,
                &ctx.settings,
                ctx.db.clone(),
                baseline_ref,
                Some(index),
                commit_sha.clone(),
                ctx.quota_manager.clone(),
                ctx.current_cache_name.as_deref(),
                ctx.cache_manager.clone(),
                ctx.active_cache_name.clone(),
                review_id,
                worktree_path,
                ctx.provider.clone(),
            )
            .await;

            match result {
                Ok(json_output) => {
                    let patches_status = json_output["patches"].as_array();
                    let target_applied = patches_status
                        .and_then(|arr| arr.iter().find(|p| p["index"] == index))
                        .map(|p| p["status"] == "applied")
                        .unwrap_or(false);

                    let history = json_output.get("history");
                    let logs_str = if let Some(h) = history {
                        serde_json::to_string_pretty(h).ok()
                    } else {
                        None
                    };

                    if let Some(h) = history.and_then(|h| h.as_array()) {
                        // Tool usage recording (same as before)
                        for item in h {
                            if let Some(role) = item.get("role").and_then(|r| r.as_str()) {
                                if role == "assistant" {
                                    if let Some(calls) =
                                        item.get("tool_calls").and_then(|c| c.as_array())
                                    {
                                        for call in calls {
                                            let name =
                                                call["function_name"].as_str().unwrap_or("unknown");
                                            let args = call["arguments"].to_string();
                                            let _ = ctx
                                                .db
                                                .create_tool_usage(ToolUsage {
                                                    review_id,
                                                    provider: ctx.settings.ai.provider.clone(),
                                                    model: ctx.settings.ai.model.clone(),
                                                    tool_name: name.to_string(),
                                                    arguments: Some(args),
                                                    output_length: 0,
                                                })
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let interaction_id = if let Some(tokens_in) = json_output["tokens_in"].as_u64()
                    {
                        let i_id = generate_id();
                        let input_ctx = json_output["input_context"].as_str().unwrap_or("");
                        let output_raw = if let Some(r) = json_output.get("review") {
                            r.to_string()
                        } else if let Some(e) = json_output.get("error") {
                            e.to_string()
                        } else {
                            String::new()
                        };

                        let _ = ctx
                            .db
                            .create_ai_interaction(AiInteractionParams {
                                id: &i_id,
                                parent_id: None,
                                workflow_id: None,
                                provider: &ctx.settings.ai.provider,
                                model: &ctx.settings.ai.model,
                                input: input_ctx,
                                output: &output_raw,
                                tokens_in: tokens_in as u32,
                                tokens_out: json_output["tokens_out"].as_u64().unwrap_or(0) as u32,
                                tokens_cached: json_output["tokens_cached"].as_u64().unwrap_or(0)
                                    as u32,
                            })
                            .await;
                        Some(i_id)
                    } else {
                        None
                    };

                    if target_applied {
                        if let Some(error_msg) = json_output["error"].as_str() {
                            if error_msg == "Patch application failed" {
                                error!(
                                    "Patch application failed for ps={} idx={}",
                                    patchset_id, index
                                );
                                let _ = ctx
                                    .db
                                    .complete_review(
                                        review_id,
                                        ReviewStatus::FailedToApply.as_str(),
                                        error_msg,
                                        None,
                                        interaction_id.as_deref(),
                                        None,
                                        logs_str.as_deref(),
                                    )
                                    .await;
                                return Ok(PatchResult::ApplyFailed);
                            }

                            error!(
                                "Review tool returned error for ps={} idx={}: {}",
                                patchset_id, index, error_msg
                            );
                            let _ = ctx
                                .db
                                .complete_review(
                                    review_id,
                                    ReviewStatus::Failed.as_str(),
                                    error_msg,
                                    None,
                                    interaction_id.as_deref(),
                                    None,
                                    logs_str.as_deref(),
                                )
                                .await;

                            if retries < max_retries {
                                retries += 1;
                                continue;
                            } else {
                                return Ok(PatchResult::ReviewFailed);
                            }
                        } else if let Some(review_content) = json_output.get("review") {
                            if !review_content.is_null() {
                                if let Some(findings_arr) =
                                    review_content.get("findings").and_then(|f| f.as_array())
                                {
                                    for f in findings_arr {
                                        let severity_str = f["severity"].as_str().unwrap_or("Low");
                                        let severity = Severity::from_str(severity_str);

                                        let problem =
                                            f["problem"].as_str().unwrap_or("").to_string();
                                        let severity_explanation = f["severity_explanation"]
                                            .as_str()
                                            .map(|s| s.to_string());
                                        let suggestion =
                                            f["suggestion"].as_str().map(|s| s.to_string());

                                        let _ = ctx
                                            .db
                                            .create_finding(Finding {
                                                review_id,
                                                severity,
                                                severity_explanation,
                                                problem,
                                                suggestion,
                                            })
                                            .await;
                                    }
                                }

                                let summary = review_content["summary"]
                                    .as_str()
                                    .unwrap_or("No summary available.")
                                    .to_string();
                                let result_desc = "Review completed successfully.";

                                let inline_review = json_output["inline_review"].as_str();

                                let _ = ctx
                                    .db
                                    .complete_review(
                                        review_id,
                                        ReviewStatus::Reviewed.as_str(),
                                        result_desc,
                                        Some(&summary),
                                        interaction_id.as_deref(),
                                        inline_review,
                                        logs_str.as_deref(),
                                    )
                                    .await;
                                return Ok(PatchResult::Success);
                            } else if ctx.settings.ai.no_ai {
                                info!(
                                    "Review skipped as requested for ps={} idx={}",
                                    patchset_id, index
                                );
                                let _ = ctx
                                    .db
                                    .complete_review(
                                        review_id,
                                        ReviewStatus::Skipped.as_str(),
                                        "Skipped AI review via --no-ai",
                                        None,
                                        interaction_id.as_deref(),
                                        None,
                                        logs_str.as_deref(),
                                    )
                                    .await;
                                return Ok(PatchResult::Success);
                            } else {
                                let _ = ctx
                                    .db
                                    .complete_review(
                                        review_id,
                                        ReviewStatus::Failed.as_str(),
                                        "AI returned null response",
                                        None,
                                        interaction_id.as_deref(),
                                        None,
                                        logs_str.as_deref(),
                                    )
                                    .await;
                                if retries < max_retries {
                                    retries += 1;
                                    continue;
                                } else {
                                    return Ok(PatchResult::ReviewFailed);
                                }
                            }
                        } else {
                            let error_msg = json_output["error"]
                                .as_str()
                                .unwrap_or("Missing review content");
                            let _ = ctx
                                .db
                                .complete_review(
                                    review_id,
                                    ReviewStatus::Failed.as_str(),
                                    error_msg,
                                    None,
                                    interaction_id.as_deref(),
                                    None,
                                    logs_str.as_deref(),
                                )
                                .await;
                            return Ok(PatchResult::ReviewFailed);
                        }
                    } else {
                        // Apply failed in tool
                        let error_msg = json_output["error"]
                            .as_str()
                            .unwrap_or("Patch application failed");
                        let _ = ctx
                            .db
                            .complete_review(
                                review_id,
                                ReviewStatus::FailedToApply.as_str(),
                                error_msg,
                                None,
                                interaction_id.as_deref(),
                                None,
                                logs_str.as_deref(),
                            )
                            .await;
                        return Ok(PatchResult::ApplyFailed);
                    }
                }
                Err(e) => {
                    error!("Review execution failed for {}: {}", patchset_id, e);
                    let _ = ctx
                        .db
                        .complete_review(
                            review_id,
                            ReviewStatus::Failed.as_str(),
                            &format!("Tool error: {}", e),
                            None,
                            None,
                            None,
                            None,
                        )
                        .await;
                    if retries < max_retries {
                        retries += 1;
                        continue;
                    }
                    return Ok(PatchResult::ReviewFailed);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_review_tool(
    patchset_id: i64,
    input_payload: &serde_json::Value,
    settings: &Settings,
    db: Arc<Database>,
    baseline: &str,
    review_index: Option<i64>,
    review_commit: Option<String>,
    quota_manager: Arc<QuotaManager>,
    cache_name: Option<&str>,
    cache_manager: Arc<CacheManager>,
    active_cache_name: Arc<Mutex<Option<String>>>,
    review_id: i64,
    worktree_path: Option<&Path>,
    provider: Arc<dyn AiProvider>,
) -> Result<serde_json::Value> {
    let exe_path = std::env::current_exe()?;
    let bin_dir = exe_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let review_bin = bin_dir.join("review");

    let mut cmd = if review_bin.exists() {
        Command::new(review_bin)
    } else {
        warn!(
            "Could not find review binary at {:?}, falling back to cargo run",
            review_bin
        );
        let mut c = Command::new("cargo");
        c.args(["run", "--bin", "review", "--"]);
        c
    };

    cmd.args([
        "--json",
        "--baseline",
        baseline,
        "--worktree-dir",
        &settings.review.worktree_dir,
        "--ai-provider",
        "stdio-gemini",
    ]);

    cmd.env("NO_COLOR", "1");
    cmd.env("SASHIKO_LOG_PLAIN", "1");

    if let Some(idx) = review_index {
        cmd.arg("--review-patch-index").arg(idx.to_string());
    }

    if let Some(commit) = review_commit {
        cmd.arg("--review-commit").arg(commit);
    }

    if let Some(name) = cache_name {
        if settings
            .ai
            .gemini
            .as_ref()
            .map(|g| g.explicit_prompt_caching)
            .unwrap_or(false)
        {
            cmd.arg("--gemini-cache").arg(name);
        }
    }

    if settings.ai.no_ai {
        cmd.arg("--no-ai");
    }

    if let Some(path) = worktree_path {
        cmd.arg("--reuse-worktree").arg(path);
    }

    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains(" ERROR ")
                    || line.starts_with("Error:")
                    || line.contains("panicked")
                {
                    error!("[review-bin] {}", line);
                } else if line.contains(" WARN ") {
                    warn!("[review-bin] {}", line);
                } else {
                    info!("[review-bin] {}", line);
                }
            }
        });
    }

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("No stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("No stdout"))?;

    // Perform interaction with timeout
    let interaction_result =
        tokio::time::timeout(std::time::Duration::from_secs(settings.review.timeout_seconds), async {
            // Send initial payload
            let mut input_str = serde_json::to_string(input_payload)?;
            input_str.push('\n');
            stdin.write_all(input_str.as_bytes()).await?;
            stdin.flush().await?;

            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut final_result: Option<Value> = None;
            let mut ai_started = false;

            while let Ok(Some(line)) = lines.next_line().await {
                // Try to parse as JSON
                if let Ok(json_msg) = serde_json::from_str::<Value>(&line) {
                    if let Some(type_str) = json_msg.get("type").and_then(|v| v.as_str()) {
                        match type_str {
                            "ai_request" | "ai_request_with_cache" => {
                                if !ai_started {
                                    let _ = db
                                        .update_review_status(
                                            review_id,
                                            ReviewStatus::InReview.as_str(),
                                            None,
                                        )
                                        .await;
                                    ai_started = true;
                                }
                                if let Some(payload_val) = json_msg.get("payload") {
                                    if let Ok(mut req) =
                                        serde_json::from_value::<AiRequest>(payload_val.clone())
                                    {
                                        // Update stale cache name if needed
                                        {
                                            let guard = active_cache_name.lock().await;
                                            if let Some(active_name) = guard.as_ref() {
                                                if req.preloaded_context.is_some()
                                                    && req.preloaded_context.as_ref()
                                                        != Some(active_name)
                                                {
                                                    info!(
                                                        "Updating stale cache name in request: {:?} -> {}",
                                                        req.preloaded_context, active_name
                                                    );
                                                    req.preloaded_context =
                                                        Some(active_name.clone());
                                                }
                                            }
                                        }

                                        let mut cache_retry_done = false;
                                        let resp_payload = loop {
                                            quota_manager.wait_for_access().await;
                                            match provider.generate_content(req.clone()).await {
                                                Ok(resp) => break Ok(resp),
                                                Err(e) => {
                                                    let err_str = e.to_string();
                                                    // Handle Gemini context cache expiration (403 Permission Denied: CachedContent not found)
                                                    if !cache_retry_done
                                                        && err_str.contains("403")
                                                        && err_str.contains("CachedContent not found")
                                                    {
                                                        warn!("AI Context Cache expired or not found. Refreshing...");
                                                        cache_retry_done = true;

                                                        let mut guard =
                                                            active_cache_name.lock().await;

                                                        let stale_cache =
                                                            req.preloaded_context.clone();

                                                        // Check if another task already refreshed it
                                                        if let Some(active_name) = guard.as_ref() {
                                                            if stale_cache.as_ref()
                                                                != Some(active_name)
                                                            {
                                                                info!(
                                                                    "Cache already refreshed by another task: {:?} -> {}",
                                                                    stale_cache, active_name
                                                                );
                                                                req.preloaded_context =
                                                                    Some(active_name.clone());
                                                                drop(guard);
                                                                continue;
                                                            }
                                                        }

                                                        // Clear the current active cache name as requested
                                                        *guard = None;

                                                        // Refresh cache via manager
                                                        match cache_manager
                                                            .ensure_cache(stale_cache.as_deref())
                                                            .await
                                                        {
                                                            Ok(new_name) => {
                                                                info!(
                                                                    "AI Context Cache refreshed: {}",
                                                                    new_name
                                                                );
                                                                *guard = Some(new_name.clone());
                                                                req.preloaded_context =
                                                                    Some(new_name);
                                                                drop(guard);
                                                                continue;
                                                            }
                                                            Err(refresh_err) => {
                                                                error!(
                                                                    "Failed to refresh AI Context Cache: {}",
                                                                    refresh_err
                                                                );
                                                                // If refresh failed, we break with original error or refresh error
                                                                break Err(refresh_err);
                                                            }
                                                        }
                                                    }

                                                    break Err(e);
                                                }
                                            }
                                        };

                                        let reply = match resp_payload {
                                            Ok(p) => json!({ "type": "ai_response", "payload": p }),
                                            Err(e) => {
                                                json!({ "type": "error", "payload": e.to_string() })
                                            }
                                        };
                                        let mut reply_str = serde_json::to_string(&reply)?;
                                        reply_str.push('\n');
                                        if let Err(e) = stdin.write_all(reply_str.as_bytes()).await {
                                            error!("Failed to write AI response to child: {}", e);
                                            break;
                                        }
                                        let _ = stdin.flush().await;
                                    }
                                }
                            }
                            "ai_create_cache" => {
                                if let Some(payload) = json_msg.get("payload") {
                                    if let Ok(req) =
                                        serde_json::from_value::<AiRequest>(payload["request"].clone())
                                    {
                                        let ttl = payload["ttl"].as_str().unwrap_or("3600s").to_string();
                                        let display_name = payload["display_name"].as_str().map(|s| s.to_string());

                                        let result = provider.create_context_cache(req, ttl, display_name).await;
                                        let reply = match result {
                                            Ok(name) => json!({
                                                "type": "ai_response",
                                                "payload": AiResponse {
                                                    content: Some(name),
                                                    thought: None,
            tool_calls: None,
                                                    usage: None,
                                                }
                                            }),
                                            Err(e) => json!({ "type": "error", "payload": e.to_string() }),
                                        };
                                        let mut reply_str = serde_json::to_string(&reply)?;
                                        reply_str.push('\n');
                                        stdin.write_all(reply_str.as_bytes()).await?;
                                        stdin.flush().await?;
                                    }
                                }
                            }
                            "ai_delete_cache" => {
                                if let Some(name) = json_msg.get("payload").and_then(|v| v.as_str()) {
                                    let result = provider.delete_context_cache(name).await;
                                    let reply = match result {
                                        Ok(_) => json!({
                                            "type": "ai_response",
                                            "payload": AiResponse {
                                                content: None,
                                                thought: None,
            tool_calls: None,
                                                usage: None,
                                            }
                                        }),
                                        Err(e) => json!({ "type": "error", "payload": e.to_string() }),
                                    };
                                    let mut reply_str = serde_json::to_string(&reply)?;
                                    reply_str.push('\n');
                                    stdin.write_all(reply_str.as_bytes()).await?;
                                    stdin.flush().await?;
                                }
                            }
                            _ => {
                                // Unknown type. Assume it's result if it matches result structure.
                                if json_msg.get("patchset_id").is_some() {
                                    final_result = Some(json_msg);
                                    break;
                                }
                            }
                        }
                    } else {
                        // No type. Result?
                        if json_msg.get("patchset_id").is_some() {
                            final_result = Some(json_msg);
                            break;
                        }
                    }
                } else {
                    // Non-JSON line. Log it.
                    warn!("Review tool stdout: {}", line);
                }
            }

            // Return result
            if let Some(res) = final_result {
                Ok(res)
            } else {
                Err(anyhow::anyhow!("Review tool finished without valid result"))
            }
        })
        .await;

    // Handle timeout and child process cleanup
    match interaction_result {
        Ok(res) => {
            // Interaction finished (Success or Error inside interaction)
            drop(stdin); // Close stdin to signal EOF/finish to child if it's still running
            let _ = child.wait().await; // Reap zombie

            // Now process the result if it was Ok
            match res {
                Ok(json) => {
                    // Update DB with patch statuses if final_result available
                    if let Some(patches) = json["patches"].as_array() {
                        for p in patches {
                            let idx = p["index"].as_i64().unwrap_or(0);
                            let status = p["status"].as_str().unwrap_or("error");

                            let stderr_str = p["stderr"].as_str().unwrap_or("");
                            let stdout_str = p["stdout"].as_str().unwrap_or("");
                            let am_error = p["am_error"].as_str().unwrap_or("");

                            let mut full_log = String::new();
                            if !am_error.is_empty() {
                                full_log.push_str("git am error:\n");
                                full_log.push_str(am_error);
                                full_log.push_str("\n\n");
                            }
                            if !stdout_str.is_empty() {
                                full_log.push_str("stdout:\n");
                                full_log.push_str(stdout_str);
                                full_log.push('\n');
                            }
                            if !stderr_str.is_empty() {
                                full_log.push_str("stderr:\n");
                                full_log.push_str(stderr_str);
                            }

                            let error_msg = if full_log.trim().is_empty() {
                                None
                            } else {
                                Some(full_log.as_str())
                            };

                            if let Err(e) = db
                                .update_patch_application_status(
                                    patchset_id,
                                    idx,
                                    status,
                                    error_msg,
                                )
                                .await
                            {
                                error!(
                                    "Failed to update patch status for ps={} idx={}: {}",
                                    patchset_id, idx, e
                                );
                            }
                        }
                    }
                    Ok(json)
                }
                Err(e) => {
                    if let Some(idx) = review_index {
                        let _ = db
                            .update_patch_application_status(
                                patchset_id,
                                idx,
                                "error",
                                Some(&e.to_string()),
                            )
                            .await;
                    }
                    Err(e)
                }
            }
        }
        Err(_) => {
            // Timeout occurred
            error!(
                "Review tool timed out after {} seconds. Killing process.",
                settings.review.timeout_seconds
            );
            let _ = child.kill().await;

            if let Some(idx) = review_index {
                if let Err(e) = db
                    .update_patch_application_status(
                        patchset_id,
                        idx,
                        "error",
                        Some("Review tool timed out"),
                    )
                    .await
                {
                    error!(
                        "Failed to update patch status for ps={} idx={}: {}",
                        patchset_id, idx, e
                    );
                }
            }

            Err(anyhow::anyhow!("Review tool timed out"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::proxy::QuotaManager;
    use crate::ai::{AiRequest, AiResponse, ProviderCapabilities};
    use crate::db::Database;
    use crate::settings::Settings;
    use async_trait::async_trait;
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tempfile::tempdir;

    struct MockProvider;
    #[async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            // Simulate a slow AI response to allow logs to accumulate
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            Ok(AiResponse {
                content: Some("Mocked AI response".to_string()),
                thought: None,
                tool_calls: None,
                usage: None,
            })
        }
        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_run_review_tool_concurrency() -> Result<()> {
        let temp_dir = tempdir()?;
        let bin_path = temp_dir.path().join("mock_review");

        // Create a mock "review" binary that:
        // 1. Reads initial JSON from stdin.
        // 2. Spams 1000 lines of logs to STDOUT.
        // 3. Sends an 'ai_request' JSON to STDOUT.
        // 4. Reads 'ai_response' from stdin.
        // 5. Prints final result JSON to STDOUT.
        let mock_script = r#"#!/bin/bash
# 1. Read input
read -r input

# 2. Spam logs
for i in {1..1000}; do
    echo "LOG LINE $i - This is a long log line to fill up buffers and test if the parent drains stdout correctly while waiting for AI response."
done

# 3. Send AI request
echo '{"type": "ai_request", "payload": {"messages": [{"role": "user", "content": "hello"}], "temperature": 0.5}}'

# 4. Wait for AI response
read -r ai_response

# 5. Send final result
echo '{"patchset_id": 1, "patches": [{"index": 1, "status": "applied"}]}'
"#;

        std::fs::write(&bin_path, mock_script)?;
        std::fs::set_permissions(&bin_path, Permissions::from_mode(0o755))?;

        // Setup Sashiko dependencies
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();

        let _db = Arc::new(Database::new(&settings.database).await?);
        let _quota_manager = Arc::new(QuotaManager::new());
        let _cache_manager = Arc::new(CacheManager::new(
            PathBuf::from("."),
            Arc::new(MockProvider),
            "mock".to_string(),
            "1h".to_string(),
            None,
        ));
        let _active_cache_name: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let provider = Arc::new(MockProvider);
        // We manually spawn the mock and run the same loop as run_review_tool
        let mut cmd = Command::new(&bin_path);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let interaction = async {
            // 1. Send input
            stdin.write_all(b"{}\n").await?;
            stdin.flush().await?;

            let mut reader = BufReader::new(stdout).lines();
            let mut final_result = None;

            while let Ok(Some(line)) = reader.next_line().await {
                if let Ok(json_msg) = serde_json::from_str::<Value>(&line) {
                    if json_msg["type"] == "ai_request" {
                        // Concurrency check: We are here, the child already sent 1000 log lines.
                        // If the parent didn't drain them, the child would be blocked on write
                        // and we would never receive this ai_request.

                        let resp = provider
                            .generate_content(AiRequest {
                                system: None,
                                messages: vec![],
                                tools: None,
                                temperature: None,
                                preloaded_context: None,
                                response_format: None,
                            })
                            .await?;

                        let reply = json!({ "type": "ai_response", "payload": resp });
                        let mut reply_str = serde_json::to_string(&reply)?;
                        reply_str.push('\n');
                        stdin.write_all(reply_str.as_bytes()).await?;
                        stdin.flush().await?;
                    } else if json_msg.get("patchset_id").is_some() {
                        final_result = Some(json_msg);
                        break;
                    }
                } else {
                    // This is where logs are drained
                    // println!("Log: {}", line);
                }
            }
            Ok::<Option<Value>, anyhow::Error>(final_result)
        };

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), interaction).await??;

        assert!(result.is_some());
        assert_eq!(result.unwrap()["patchset_id"], 1);

        child.wait().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_reactive_cache_refresh() -> Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};

        let call_count = Arc::new(AtomicU32::new(0));
        let refresh_count = Arc::new(AtomicU32::new(0));

        struct ReactiveMockProvider {
            call_count: Arc<AtomicU32>,
            refresh_count: Arc<AtomicU32>,
        }
        #[async_trait]
        impl AiProvider for ReactiveMockProvider {
            async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
                let count = self.call_count.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    // First call fails with 403
                    assert_eq!(request.preloaded_context.as_deref(), Some("stale-cache"));
                    anyhow::bail!("403 Permission Denied: CachedContent not found");
                } else {
                    // Second call succeeds
                    assert_eq!(request.preloaded_context.as_deref(), Some("fresh-cache"));
                    Ok(AiResponse {
                        content: Some("Success after refresh".to_string()),
                        thought: None,
                        tool_calls: None,
                        usage: None,
                    })
                }
            }
            async fn create_context_cache(
                &self,
                _request: AiRequest,
                _ttl: String,
                _display_name: Option<String>,
            ) -> Result<String> {
                self.refresh_count.fetch_add(1, Ordering::SeqCst);
                Ok("fresh-cache".to_string())
            }
            fn estimate_tokens(&self, _request: &AiRequest) -> usize {
                0
            }
            fn get_capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    model_name: "mock".to_string(),
                    context_window_size: 1000,
                }
            }
        }

        let temp_dir = tempdir()?;
        let bin_path = temp_dir.path().join("mock_review_cache");

        let mock_script = r#"#!/bin/bash
read -r input
# Child sends request with stale cache
echo '{"type": "ai_request", "payload": {"messages": [], "preloaded_context": "stale-cache"}}'
read -r response
# Child receives response and prints it
echo "$response"
echo '{"patchset_id": 1, "patches": []}'
"#;
        std::fs::write(&bin_path, mock_script)?;
        std::fs::set_permissions(&bin_path, Permissions::from_mode(0o755))?;

        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        let _db = Arc::new(Database::new(&settings.database).await?);
        let _quota_manager = Arc::new(QuotaManager::new());
        let provider = Arc::new(ReactiveMockProvider {
            call_count: call_count.clone(),
            refresh_count: refresh_count.clone(),
        });
        let cache_manager = Arc::new(CacheManager::new(
            PathBuf::from("."),
            provider.clone(),
            "mock".to_string(),
            "1h".to_string(),
            None,
        ));
        let active_cache_name = Arc::new(Mutex::new(Some("stale-cache".to_string())));

        let mut cmd = Command::new(&bin_path);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = cmd.spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // This block effectively runs the interaction loop from run_review_tool
        let mut reader = BufReader::new(stdout).lines();
        stdin.write_all(b"{}\n").await?;
        stdin.flush().await?;

        while let Ok(Some(line)) = reader.next_line().await {
            if let Ok(json_msg) = serde_json::from_str::<Value>(&line) {
                if json_msg["type"] == "ai_request" {
                    let mut req: AiRequest = serde_json::from_value(json_msg["payload"].clone())?;
                    let mut cache_retry_done = false;
                    let resp = loop {
                        match provider.generate_content(req.clone()).await {
                            Ok(r) => break Ok(r),
                            Err(e) => {
                                if !cache_retry_done && e.to_string().contains("403") {
                                    cache_retry_done = true;
                                    let mut guard = active_cache_name.lock().await;
                                    *guard = None;
                                    let new_name =
                                        cache_manager.ensure_cache(Some("stale-cache")).await?;
                                    *guard = Some(new_name.clone());
                                    req.preloaded_context = Some(new_name);
                                    continue;
                                }
                                break Err(e);
                            }
                        }
                    };
                    let reply = json!({ "type": "ai_response", "payload": resp? });
                    stdin
                        .write_all(serde_json::to_string(&reply)?.as_bytes())
                        .await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                } else if json_msg.get("patchset_id").is_some() {
                    break;
                }
            }
        }

        assert_eq!(call_count.load(Ordering::SeqCst), 2);
        assert_eq!(refresh_count.load(Ordering::SeqCst), 1);
        child.wait().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_cross_task_cache_refresh_coordination() -> Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};

        let refresh_count = Arc::new(AtomicU32::new(0));

        struct MultiMockProvider {
            refresh_count: Arc<AtomicU32>,
        }
        #[async_trait]
        impl AiProvider for MultiMockProvider {
            async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
                if request.preloaded_context.as_deref() == Some("stale") {
                    anyhow::bail!("403 Permission Denied: CachedContent not found");
                }
                Ok(AiResponse {
                    content: Some("Ok".to_string()),
                    thought: None,
                    tool_calls: None,
                    usage: None,
                })
            }
            async fn create_context_cache(
                &self,
                _request: AiRequest,
                _ttl: String,
                _display_name: Option<String>,
            ) -> Result<String> {
                // Simulate slow refresh to increase race probability
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                self.refresh_count.fetch_add(1, Ordering::SeqCst);
                Ok("fresh".to_string())
            }
            fn estimate_tokens(&self, _request: &AiRequest) -> usize {
                0
            }
            fn get_capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    model_name: "m".to_string(),
                    context_window_size: 10,
                }
            }
        }

        let provider = Arc::new(MultiMockProvider {
            refresh_count: refresh_count.clone(),
        });
        let cache_manager = Arc::new(CacheManager::new(
            PathBuf::from("."),
            provider.clone(),
            "m".to_string(),
            "1h".to_string(),
            None,
        ));
        let active_cache_name = Arc::new(Mutex::new(Some("stale".to_string())));

        // Simulate two concurrent tasks both hitting 403
        let mut handles = vec![];
        for _ in 0..2 {
            let p = provider.clone();
            let cm = cache_manager.clone();
            let acn = active_cache_name.clone();
            handles.push(tokio::spawn(async move {
                let mut req = AiRequest {
                    system: None,
                    messages: vec![],
                    tools: None,
                    temperature: None,
                    preloaded_context: Some("stale".to_string()),
                    response_format: None,
                };

                let mut retry = false;
                loop {
                    match p.generate_content(req.clone()).await {
                        Ok(_) => break Ok::<(), anyhow::Error>(()),
                        Err(e) => {
                            if !retry && e.to_string().contains("403") {
                                retry = true;
                                let mut guard = acn.lock().await;
                                let current = req.preloaded_context.clone();
                                if let Some(active) = guard.as_ref() {
                                    if current.as_ref() != Some(active) {
                                        // Already refreshed by another task!
                                        req.preloaded_context = Some(active.clone());
                                        drop(guard);
                                        continue;
                                    }
                                }
                                *guard = None;
                                let new = cm.ensure_cache(current.as_deref()).await?;
                                *guard = Some(new.clone());
                                req.preloaded_context = Some(new);
                                drop(guard);
                                continue;
                            }
                            break Err(e);
                        }
                    }
                }
            }));
        }

        for h in handles {
            h.await??;
        }

        // Critical assertion: Only ONE refresh call happened despite two failures
        assert_eq!(refresh_count.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_skip_ignored_files() -> Result<()> {
        let mut settings = Settings::new()?;
        settings.database.url = ":memory:".to_string();
        settings.review.ignore_files = vec!["ignored.txt".to_string(), "ignore_dir/".to_string()];

        let db = Arc::new(Database::new(&settings.database).await?);
        db.migrate().await?;
        let quota_manager = Arc::new(QuotaManager::new());
        let provider = Arc::new(MockProvider);
        let cache_manager = Arc::new(CacheManager::new(
            PathBuf::from("."),
            provider.clone(),
            "mock".to_string(),
            "1h".to_string(),
            None,
        ));
        let active_cache_name = Arc::new(Mutex::new(None));

        let ctx = ReviewContext {
            db: db.clone(),
            settings: settings.clone(),
            baseline_registry: Arc::new(BaselineRegistry::new(Path::new(".")).unwrap()),
            quota_manager,
            cache_manager,
            active_cache_name,
            current_cache_name: None,
            target_review_count: 1,
            provider,
        };

        // Create dummy patchset and patch in DB
        let thread_id = db.create_thread("msg_id_1", "Subject", 1000).await?;

        // Create messages for patches
        db.create_message(
            "msg_id_p1",
            thread_id,
            None,
            "Author",
            "Subject",
            1000,
            "Body",
            "",
            "",
            None,
            None,
        )
        .await?;
        db.create_message(
            "msg_id_p2",
            thread_id,
            None,
            "Author",
            "Subject",
            1000,
            "Body",
            "",
            "",
            None,
            None,
        )
        .await?;
        db.create_message(
            "msg_id_p3",
            thread_id,
            None,
            "Author",
            "Subject",
            1000,
            "Body",
            "",
            "",
            None,
            None,
        )
        .await?;

        let ps_id = db
            .create_patchset(
                thread_id, None, "msg_id_1", "Subject", "Author", 1000, 1, 1, "", "", None, 1,
                None, false, None, None,
            )
            .await?
            .expect("Failed to create patchset");

        // Case 1: Ignored file
        let diff_ignored = "diff --git a/ignored.txt b/ignored.txt\nindex ...";
        let p_id = db.create_patch(ps_id, "msg_id_p1", 1, diff_ignored).await?;

        let result = Reviewer::process_patch_review(
            &ctx,
            ps_id,
            p_id,
            1,
            "HEAD",
            None,
            &json!({}),
            None,
            None,
            None,
            diff_ignored,
        )
        .await?;

        // Should return Success (because it's skipped gracefully)
        match result {
            PatchResult::Success => {}
            _ => panic!("Expected Success for skipped review"),
        }

        // Verify DB status
        let mut rows = db
            .conn
            .query(
                "SELECT status, result_description FROM reviews WHERE patch_id = ?",
                libsql::params![p_id],
            )
            .await?;
        let row = rows.next().await?.expect("No review found");
        let status: String = row.get(0)?;
        let description: String = row.get(1).unwrap_or_default();

        assert_eq!(status, "Skipped");
        assert!(description.contains("touches only ignored files"));

        // Case 2: Ignored directory prefix
        let diff_dir = "diff --git a/ignore_dir/subfile.c b/ignore_dir/subfile.c\n...";
        let p_id_2 = db.create_patch(ps_id, "msg_id_p2", 2, diff_dir).await?;

        let result = Reviewer::process_patch_review(
            &ctx,
            ps_id,
            p_id_2,
            2,
            "HEAD",
            None,
            &json!({}),
            None,
            None,
            None,
            diff_dir,
        )
        .await?;

        match result {
            PatchResult::Success => {}
            _ => panic!("Expected Success for skipped review"),
        }

        let mut rows = db
            .conn
            .query(
                "SELECT status FROM reviews WHERE patch_id = ?",
                libsql::params![p_id_2],
            )
            .await?;
        let row = rows.next().await?.expect("No review found");
        let status: String = row.get(0)?;
        assert_eq!(status, "Skipped");

        // Case 3: Mixed (Ignored + Not Ignored) -> Should NOT skip
        let diff_mixed = "diff --git a/ignored.txt b/ignored.txt\n...\ndiff --git a/src/main.rs b/src/main.rs\n...";
        let p_id_3 = db.create_patch(ps_id, "msg_id_p3", 3, diff_mixed).await?;

        let _result = Reviewer::process_patch_review(
            &ctx,
            ps_id,
            p_id_3,
            3,
            "HEAD",
            None,
            &json!({}),
            None,
            None,
            None,
            diff_mixed,
        )
        .await;

        // Even if it fails to run tool, it shouldn't be "Skipped".
        let mut rows = db
            .conn
            .query(
                "SELECT status FROM reviews WHERE patch_id = ?",
                libsql::params![p_id_3],
            )
            .await?;
        // Review might be created (Pending/InReview) or not if process failed early (but create_review is called early in loop)
        // Wait, loop calls create_review at start of loop.
        // If run_review_tool fails (which it will), we get ReviewFailed.

        if let Ok(Some(row)) = rows.next().await {
            let status: String = row.get(0)?;
            assert_ne!(status, "Skipped");
        } else {
            // It might fail before creating review?
            // create_review is inside the loop.
            // If run_review_tool fails to spawn (binary not found), it returns Err.
            // process_patch_review handles Err by logging and retrying.
            // If retries exhausted, it returns ReviewFailed.
            // But it DOES create a review entry in each iteration.
            // So we should find at least one review.
        }

        Ok(())
    }
}
