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
use crate::ai::gemini::{
    GeminiClient, GeminiError, GenerateContentRequest, GenerateContentWithCacheRequest,
};
use crate::ai::proxy::QuotaManager;
use crate::baseline::{BaselineRegistry, BaselineResolution, extract_files_from_diff};
use crate::db::{AiInteractionParams, Database, Finding, PatchsetRow, Severity, ToolUsage};
use crate::git_ops::{ensure_remote, get_commit_hash, GitWorktree};
use crate::settings::Settings;
use anyhow::Result;
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
}

enum PatchResult {
    Success,
    ApplyFailed,
    ReviewFailed,
}

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");
    format!("{:x}-{:x}", since_the_epoch.as_micros(), std::process::id())
}

pub struct Reviewer {
    db: Arc<Database>,
    settings: Settings,
    semaphore: Arc<Semaphore>,
    baseline_registry: Arc<BaselineRegistry>,
    quota_manager: Arc<QuotaManager>,
    cache_manager: Arc<CacheManager>,
    active_cache_name: Arc<Mutex<Option<String>>>,
}

use crate::worker::tools::ToolBox;

impl Reviewer {
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

        // Initialize CacheManager
        // Assuming prompts are in "third_party/review-prompts/kernel" in CWD.
        let prompts_dir = PathBuf::from("third_party/review-prompts/kernel");
        let client = Box::new(GeminiClient::new(settings.ai.model.clone()));

        // We need tool definitions for the cache.
        // We use dummy paths for ToolBox here because we only need declarations,
        // not execution capability.
        let tools_def = ToolBox::new(PathBuf::from("."), None).get_declarations();

        let cache_manager = Arc::new(CacheManager::new(
            prompts_dir,
            client,
            settings.ai.model.clone(),
            "3600s".to_string(),
            Some(vec![tools_def]),
        ));

        Self {
            db,
            settings,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            baseline_registry,
            quota_manager: Arc::new(QuotaManager::new()),
            cache_manager,
            active_cache_name: Arc::new(Mutex::new(None)),
        }
    }

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

        // Ensure Gemini Cache
        if self.settings.ai.explicit_prompts_caching {
            match self.cache_manager.ensure_cache(None).await {
                Ok(name) => {
                    info!("Gemini Context Cache initialized: {}", name);
                    let mut guard = self.active_cache_name.lock().await;
                    *guard = Some(name);
                }
                Err(e) => {
                    error!(
                        "Failed to initialize Gemini Context Cache: {}. Proceeding without cache (higher cost/latency).",
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
            .update_patchset_status(patchset_id, ReviewStatus::Applying.as_str())
            .await
        {
            error!(
                "Failed to update status to Applying for {}: {}",
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
                json!({
                    "index": idx,
                    "diff": diff,
                    "subject": subj,
                    "author": auth,
                    "date": date,
                    "message_id": msg_id
                })
            })
            .collect();

        let input_payload = json!({
            "id": patchset_id,
            "subject": patchset.subject.clone().unwrap_or("Unknown".to_string()),
            "patches": patches_json
        });

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

        let prompts_hash = get_commit_hash(Path::new("third_party/review-prompts"), "HEAD")
            .await
            .ok();

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

            // 2. Run Reviews
            let mut review_success = true; // Optimistic
            let mut failed_patches = 0;

            for (patch_id, index, _diff, _subj, _auth, _date, _msg_id) in &diffs {
                let commit_sha = patch_commits.get(index).cloned();
                let baseline_ref = resolution.as_str();

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
                )
                .await;

            let _ = ctx
                .db
                .update_patchset_status(patchset_id, ReviewStatus::FailedToApply.as_str())
                .await;
        }
    }
                let commit_sha = patch_commits.get(index).cloned();
                let baseline_ref = resolution.as_str();

                match Self::process_patch_review(
                    &ctx,
                    patchset_id,
                    *patch_id,
                    *index,
                    &baseline_ref,
                    Some(baseline_id),
                    &input_payload,
                    commit_sha,
                    Some(&worktree.path),
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
        let mut full_logs = String::new();
        let repo_path = PathBuf::from(&ctx.settings.git.repository_path);

        for candidate in candidates {
            let baseline_ref = candidate.as_str();
            full_logs.push_str(&format!("Trying baseline: {}\n", baseline_ref));
            
            // Check remote
            if let BaselineResolution::RemoteTarget { url, name, .. } = candidate {
                if let Err(e) = ensure_remote(&repo_path, name, url, false).await {
                    let msg = format!("Failed to fetch remote {}: {}\n", url, e);
                    full_logs.push_str(&msg);
                    error!("{}", msg.trim());
                    continue;
                }
            }

            // Resolve SHA
            let baseline_sha = match get_commit_hash(&repo_path, &baseline_ref).await {
                Ok(sha) => sha,
                Err(e) => {
                    let msg = format!("Failed to resolve baseline ref {}: {}\n", baseline_ref, e);
                    full_logs.push_str(&msg);
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
                    full_logs.push_str(&msg);
                    continue;
                }
            };

            // Apply patches
            let mut patch_commits = HashMap::new();
            let mut application_failed = false;
            let mut apply_logs = String::new();

            for (patch_id, index, diff, subject, author, date_ts, _msg_id) in diffs {
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

                let mbox = format!(
                    "From: {}\nDate: {}\nSubject: {}\n\n{}\n",
                    author, date_str, subject, diff
                );

                // Try git am
                let mut applied = false;
                if let Ok(_) = worktree.apply_patch(&mbox).await {
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

                if applied {
                    if let Ok(sha) = get_commit_hash(&worktree.path, "HEAD").await {
                        patch_commits.insert(*index, sha);
                    }
                } else {
                    let msg = format!("Patch {}/{} (ID: {}) failed to apply.\n", patchset_id, index, patch_id);
                    apply_logs.push_str(&msg);
                    application_failed = true;
                    break;
                }
            }

            if !application_failed {
                full_logs.push_str("Application successful.\n");
                
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
                        .ok() // If fail, we just proceed? Better to have it.
                };

                if let Some(bid) = baseline_id {
                    return (Some((candidate.clone(), bid, worktree)), patch_commits, full_logs);
                } else {
                    full_logs.push_str("Failed to record baseline in DB.\n");
                }
            } else {
                full_logs.push_str(&apply_logs);
                full_logs.push_str("Application failed.\n");
            }
            
            // Clean up failed worktree
            let _ = worktree.remove().await;
        }

        (None, HashMap::new(), full_logs)
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
        worktree_path: Option<&Path>,
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
                patchset_id, index, patch_id, successful_count, baseline_id, ctx.target_review_count
            );
            return Ok(PatchResult::Success);
        }

        let mut retries = 0;
        let max_retries = 0;

        loop {
            let review_id = ctx
                .db
                .create_review(
                    patchset_id,
                    Some(patch_id),
                )
                .await?;

            let _ = ctx
                .db
                .update_review_status(review_id, ReviewStatus::Applying.as_str(), None)
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
                            if let Some(parts) = item.get("parts").and_then(|p| p.as_array()) {
                                for part in parts {
                                    if let Some(call) = part.get("functionCall") {
                                        let name = call["name"].as_str().unwrap_or("unknown");
                                        let args = call["args"].to_string();
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
                                        let file_path =
                                            f["file"].as_str().unwrap_or("unknown").to_string();
                                        let line_number = f["line"].as_i64().unwrap_or(0);
                                        let severity_str = f["severity"].as_str().unwrap_or("Low");
                                        let message =
                                            f["message"].as_str().unwrap_or("").to_string();
                                        let suggestion =
                                            f["suggestion"].as_str().map(|s| s.to_string());

                                        let severity = Severity::from_str(severity_str);

                                        let _ = ctx
                                            .db
                                            .create_finding(Finding {
                                                review_id,
                                                file_path,
                                                line_number,
                                                severity,
                                                message,
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
        if settings.ai.explicit_prompts_caching {
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

    // We need to hold stdin to write responses
    // But `child.stdin` is Option. We took it. We need to pass it to loop.
    // However, `child.spawn()` returns child.
    // The previous block took stdin.
    // I need to restructure to keep `stdin`.

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
            let client = GeminiClient::new(
                settings.ai.model.clone(),
            );
            let mut final_result: Option<Value> = None;
            let mut ai_started = false;

            while let Ok(Some(line)) = lines.next_line().await {
                // Try to parse as JSON
                if let Ok(json_msg) = serde_json::from_str::<Value>(&line) {
                    if let Some(type_str) = json_msg.get("type").and_then(|v| v.as_str()) {
                        if type_str == "ai_request" {
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
                                if let Ok(req) = serde_json::from_value::<GenerateContentRequest>(
                                    payload_val.clone(),
                                ) {
                                    // Handle Standard AI Request
                                    let resp_payload = loop {
                                        quota_manager.wait_for_access().await;
                                        match client.generate_content_single(&req).await {
                                            Ok(resp) => break Ok(resp),
                                            Err(e) => {
                                                if let Some(GeminiError::QuotaExceeded(d)) = e.downcast_ref::<GeminiError>() {
                                                    quota_manager.report_quota_error(*d).await;
                                                    continue;
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
                        } else if type_str == "ai_request_with_cache" {
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
                                if let Ok(req) =
                                    serde_json::from_value::<GenerateContentWithCacheRequest>(
                                        payload_val.clone(),
                                    )
                                {
                                    // Handle Cached AI Request
                                    let mut current_req = req;
                                    let mut updated_cache_name: Option<String> = None;

                                    // Check if we have a newer cache name and update the request if needed
                                    {
                                        let guard = active_cache_name.lock().await;
                                        if let Some(active_name) = guard.as_ref() {
                                            if &current_req.cached_content != active_name {
                                                info!(
                                                    "Updating stale cache name in request: {} -> {}",
                                                    current_req.cached_content, active_name
                                                );
                                                current_req.cached_content = active_name.clone();
                                                updated_cache_name = Some(active_name.clone());
                                            }
                                        }
                                    }

                                    let mut resp_payload = loop {
                                        quota_manager.wait_for_access().await;
                                        match client.generate_content_with_cache_single(&current_req).await
                                        {
                                            Ok(resp) => break Ok(resp),
                                            Err(e) => {
                                                if let Some(gemini_err) = e.downcast_ref::<GeminiError>() {
                                                    match gemini_err {
                                                        GeminiError::QuotaExceeded(d) => {
                                                            quota_manager.report_quota_error(*d).await;
                                                            continue;
                                                        }
                                                        GeminiError::PermissionDenied(_) => {
                                                            warn!("Permission denied for cache. Refreshing cache...");
                                                            match cache_manager
                                                                .ensure_cache(Some(&current_req.cached_content))
                                                                .await
                                                            {
                                                                Ok(new_name) => {
                                                                    info!("Refreshed cache: {}", new_name);
                                                                    current_req.cached_content = new_name.clone();
                                                                    updated_cache_name = Some(new_name.clone());
                                                                    let mut guard = active_cache_name.lock().await;
                                                                    *guard = Some(new_name);
                                                                    continue;
                                                                }
                                                                Err(create_err) => {
                                                                    error!("Failed to recreate cache: {}", create_err);
                                                                    break Err(e);
                                                                }
                                                            }
                                                        }
                                                        _ => break Err(e),
                                                    }
                                                }
                                                break Err(e);
                                            }
                                        }
                                    };

                                    // Inject new cache name into response metadata if updated
                                    if let Some(new_name) = updated_cache_name {
                                        if let Ok(resp) = resp_payload.as_mut() {
                                            let usage = resp.usage_metadata.get_or_insert_with(|| crate::ai::gemini::UsageMetadata {
                                                prompt_token_count: 0,
                                                candidates_token_count: None,
                                                total_token_count: 0,
                                                cached_content_token_count: None,
                                                extra: Some(std::collections::HashMap::new()),
                                            });
                                            let extra = usage.extra.get_or_insert_with(std::collections::HashMap::new);
                                            extra.insert("new_cache_name".to_string(), serde_json::Value::String(new_name));
                                        }
                                    }

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
                        } else {
                            // Unknown type? Assume it's result if it matches result structure?
                            if json_msg.get("patchset_id").is_some() {
                                final_result = Some(json_msg);
                                break;
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
                    // Non-JSON line? Log it
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
