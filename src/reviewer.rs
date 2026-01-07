use crate::ReviewStatus;
use crate::ai::cache::CacheManager;
use crate::ai::gemini::{
    GeminiClient, GenerateContentRequest, GenerateContentWithCacheRequest, GeminiError,
};
use crate::ai::proxy::QuotaManager;
use crate::baseline::{BaselineRegistry, BaselineResolution, extract_files_from_diff};
use crate::db::{AiInteractionParams, Database};
use crate::git_ops::{ensure_remote, get_commit_hash};
use crate::settings::Settings;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore};
use tracing::{error, info, warn};

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

use crate::agent::tools::ToolBox;

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
        // Assuming prompts are in "review-prompts" in CWD.
        let prompts_dir = PathBuf::from("review-prompts");
        let client = Box::new(GeminiClient::new(
            settings.ai.model.clone(),
            settings.ai.rate_limit_tokens_per_minute,
        ));

        // We need tool definitions for the cache.
        // We use dummy paths for ToolBox here because we only need declarations,
        // not execution capability.
        let tools_def = ToolBox::new(PathBuf::from(".")).get_declarations();

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

        // Ensure Gemini Cache
        match self.cache_manager.ensure_cache().await {
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
            let db = self.db.clone();
            let settings = self.settings.clone();
            let baseline_registry = self.baseline_registry.clone();
            let quota_manager = self.quota_manager.clone();
            let patchset_id = patchset.id;
            let subject = patchset.subject.clone().unwrap_or("Unknown".to_string());
            let cache_name = current_cache_name.clone();
            let cache_manager = self.cache_manager.clone();
            let active_cache_name = self.active_cache_name.clone();

            tokio::spawn(async move {
                let _permit = permit;

                info!("Starting review for patchset {}", patchset_id);

                if let Err(e) = db
                    .update_patchset_status(patchset_id, ReviewStatus::Applying.as_str())
                    .await
                {
                    error!(
                        "Failed to update status to Applying for {}: {}",
                        patchset_id, e
                    );
                    return;
                }

                let diffs = match db.get_patch_diffs(patchset_id).await {
                    Ok(d) => d,
                    Err(e) => {
                        error!("Failed to fetch diffs for {}: {}", patchset_id, e);
                        let _ = db.update_patchset_status(patchset_id, "Failed").await;
                        return;
                    }
                };

                // patches_json for input payload (contains all patches)
                let patches_json: Vec<_> = diffs
                    .iter()
                    .map(|(_id, idx, diff, subj, auth, date)| {
                        json!({
                            "index": idx,
                            "diff": diff,
                            "subject": subj,
                            "author": auth,
                            "date": date
                        })
                    })
                    .collect();

                let input_payload = json!({
                    "id": patchset_id,
                    "subject": subject,
                    "patches": patches_json
                });

                // Determine Baseline
                let mut all_files = Vec::new();
                for p in patches_json.iter() {
                    if let Some(diff_str) = p["diff"].as_str() {
                        let files = extract_files_from_diff(diff_str);
                        all_files.extend(files);
                    }
                }

                // Fetch body for base-commit detection
                let body = if let Some(mid) = &patchset.message_id {
                    db.get_message_body(mid).await.unwrap_or(None)
                } else if let Some(first_patch_msg_id) =
                    patches_json.first().and_then(|p| p["message_id"].as_str())
                {
                    db.get_message_body(first_patch_msg_id)
                        .await
                        .unwrap_or(None)
                } else {
                    None
                };

                let candidates =
                    baseline_registry.resolve_candidates(&all_files, &subject, body.as_deref());

                let mut final_status = ReviewStatus::Reviewed.as_str().to_string(); // Assume success unless failure
                let repo_path = PathBuf::from(&settings.git.repository_path);

                // We only use the FIRST candidate for now (simplification) or loop?
                // The original code looped candidates. But we now loop patches.
                // If we loop patches inside candidates loop, we might re-review patches for each candidate?
                // Usually there is only 1 valid candidate.
                // Let's stick to the outer loop being candidates, but we should probably stop after one successful candidate?
                // The original code tried candidates until one worked.

                let mut review_success = false;

                for candidate in candidates {
                    let baseline_ref = candidate.as_str();
                    let _fetch_warning = match &candidate {
                        BaselineResolution::Commit(h) => {
                            info!("Using base-commit for {}: {}", patchset_id, h);
                            Option::<String>::None
                        }
                        BaselineResolution::LocalRef(r) => {
                            info!("Using local baseline for {}: {}", patchset_id, r);
                            Option::<String>::None
                        }
                        BaselineResolution::RemoteTarget {
                            url,
                            name,
                            branch: _,
                        } => {
                            info!(
                                "Fetching remote baseline for {}: {} ({})",
                                patchset_id, name, url
                            );
                            match ensure_remote(&repo_path, name, url, false).await {
                                Ok(_) => None,
                                Err(e) => {
                                    let msg = format!(
                                        "Failed to fetch remote {}: {}. Skipping candidate.",
                                        url, e
                                    );
                                    error!("{}", msg);
                                    continue;
                                }
                            }
                        }
                    };

                    // Baseline preparation
                    let mut retries = 0;
                    const MAX_RETRIES: i32 = 3;

                    // Now loop through patches
                    let mut candidate_success = true;

                    for (patch_id, index, _diff, _subj, _auth, _date) in &diffs {
                        info!(
                            "Reviewing patch {}/{} (ID: {})",
                            patchset_id, index, patch_id
                        );

                        loop {
                            let prompts_hash = get_commit_hash(Path::new("review-prompts"), "HEAD")
                                .await
                                .ok();
                            let baseline_commit =
                                get_commit_hash(&repo_path, &baseline_ref).await.ok();

                            let baseline_id = if let Some(commit) = &baseline_commit {
                                let (repo_url, branch) = match &candidate {
                                    BaselineResolution::RemoteTarget { url, .. } => {
                                        (Some(url.as_str()), Some(baseline_ref.as_str()))
                                    }
                                    _ => (None, Some(baseline_ref.as_str())),
                                };
                                db.create_baseline(repo_url, branch, Some(commit))
                                    .await
                                    .ok()
                            } else {
                                None
                            };

                            let review_id = match db
                                .create_review(
                                    patchset_id,
                                    Some(*patch_id),
                                    &settings.ai.provider,
                                    &settings.ai.model,
                                    baseline_id,
                                    prompts_hash.as_deref(),
                                )
                                .await
                            {
                                Ok(id) => id,
                                Err(e) => {
                                    error!("Failed to create review entry: {}", e);
                                    candidate_success = false;
                                    break;
                                }
                            };

                            let _ = db
                                .update_review_status(
                                    review_id,
                                    ReviewStatus::Applying.as_str(),
                                    None,
                                )
                                .await;

                            // Run tool for SPECIFIC patch index
                            match run_review_tool(
                                patchset_id,
                                &input_payload,
                                &settings,
                                db.clone(),
                                &baseline_ref,
                                Some(*index),
                                quota_manager.clone(),
                                cache_name.as_deref(),
                                cache_manager.clone(),
                                active_cache_name.clone(),
                                review_id,
                            )
                            .await
                            {
                                Ok(json_output) => {
                                    // Check patches status
                                    let patches_status = json_output["patches"].as_array();
                                    let target_applied = patches_status
                                        .and_then(|arr| arr.iter().find(|p| p["index"] == *index))
                                        .map(|p| p["status"] == "applied")
                                        .unwrap_or(false);

                                    let history = json_output.get("history");
                                    let logs_str = if let Some(h) = history {
                                        serde_json::to_string_pretty(h).ok()
                                    } else {
                                        None
                                    };

                                    if target_applied {
                                        if let Some(error_msg) = json_output["error"].as_str() {
                                            error!(
                                                "Review tool returned error for ps={} idx={}: {}",
                                                patchset_id, index, error_msg
                                            );
                                            let _ = db
                                                .complete_review(
                                                    review_id,
                                                    ReviewStatus::Failed.as_str(),
                                                    error_msg,
                                                    None,
                                                    None,
                                                    None,
                                                    logs_str.as_deref(),
                                                )
                                                .await;

                                            if retries < MAX_RETRIES {
                                                retries += 1;
                                                warn!(
                                                    "AI failed for ps={} idx={}. Retrying (attempt {}/{})...",
                                                    patchset_id, index, retries, MAX_RETRIES
                                                );
                                                continue;
                                            } else {
                                                final_status =
                                                    ReviewStatus::Failed.as_str().to_string();
                                                break;
                                            }
                                        } else if let Some(review_content) =
                                            json_output.get("review")
                                        {
                                            if !review_content.is_null() {
                                                // Record Interaction
                                                let interaction_id = generate_id();
                                                let input_ctx = json_output["input_context"]
                                                    .as_str()
                                                    .unwrap_or("");
                                                let output_raw = review_content.to_string();

                                                let _ = db
                                                    .create_ai_interaction(AiInteractionParams {
                                                        id: &interaction_id,
                                                        parent_id: None,
                                                        workflow_id: None,
                                                        provider: &settings.ai.provider,
                                                        model: &settings.ai.model,
                                                        input: input_ctx,
                                                        output: &output_raw,
                                                        tokens_in: json_output["tokens_in"]
                                                            .as_u64()
                                                            .unwrap_or(0)
                                                            as u32,
                                                        tokens_out: json_output["tokens_out"]
                                                            .as_u64()
                                                            .unwrap_or(0)
                                                            as u32,
                                                    })
                                                    .await;

                                                let summary = review_content["summary"]
                                                    .as_str()
                                                    .unwrap_or("No summary available.")
                                                    .to_string();
                                                let result_desc = "Review completed successfully.";

                                                let inline_review =
                                                    json_output["inline_review"].as_str();

                                                let _ = db
                                                    .complete_review(
                                                        review_id,
                                                        ReviewStatus::Reviewed.as_str(),
                                                        result_desc,
                                                        Some(&summary),
                                                        Some(&interaction_id),
                                                        inline_review,
                                                        logs_str.as_deref(),
                                                    )
                                                    .await;
                                                break; // Success for this patch
                                            } else {
                                                let _ = db
                                                    .complete_review(
                                                        review_id,
                                                        ReviewStatus::Failed.as_str(),
                                                        "AI returned null response",
                                                        None,
                                                        None,
                                                        None,
                                                        logs_str.as_deref(),
                                                    )
                                                    .await;
                                                if retries < MAX_RETRIES {
                                                    retries += 1;
                                                    warn!(
                                                        "AI failed for ps={} idx={}. Retrying (attempt {}/{})...",
                                                        patchset_id, index, retries, MAX_RETRIES
                                                    );
                                                    continue;
                                                } else {
                                                    final_status =
                                                        ReviewStatus::Failed.as_str().to_string();
                                                    break;
                                                }
                                            }
                                        } else {
                                            // Patches applied but no review content found
                                            let error_msg = json_output["error"]
                                                .as_str()
                                                .unwrap_or("Missing review content");

                                            error!(
                                                "Review tool returned no content for ps={} idx={}. Error: {}",
                                                patchset_id, index, error_msg
                                            );

                                            let _ = db
                                                .complete_review(
                                                    review_id,
                                                    ReviewStatus::Failed.as_str(),
                                                    error_msg,
                                                    None,
                                                    None,
                                                    None,
                                                    logs_str.as_deref(),
                                                )
                                                .await;
                                            if retries < MAX_RETRIES {
                                                retries += 1;
                                                continue;
                                            }
                                            final_status =
                                                ReviewStatus::Failed.as_str().to_string();
                                            break;
                                        }
                                    } else {
                                        // Patch application failed
                                        let patches_debug =
                                            serde_json::to_string_pretty(&json_output["patches"])
                                                .unwrap_or_default();
                                        let error_msg = json_output["error"]
                                            .as_str()
                                            .unwrap_or("Patch application failed");
                                        let _ = db
                                            .update_review_status(
                                                review_id,
                                                ReviewStatus::Failed.as_str(),
                                                Some(&patches_debug),
                                            )
                                            .await;
                                        let _ = db
                                            .complete_review(
                                                review_id,
                                                ReviewStatus::Failed.as_str(),
                                                error_msg,
                                                None,
                                                None,
                                                None,
                                                logs_str.as_deref(),
                                            )
                                            .await;

                                        candidate_success = false;
                                        final_status = ReviewStatus::Failed.as_str().to_string();
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!("Review execution failed for {}: {}", patchset_id, e);
                                    let _ = db
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
                                    // Tool failure (e.g. binary crash). Retry?
                                    if retries < MAX_RETRIES {
                                        retries += 1;
                                        continue;
                                    }
                                    candidate_success = false;
                                    final_status = ReviewStatus::Failed.as_str().to_string();
                                    break;
                                }
                            }
                        }

                        if !candidate_success {
                            break; // Stop processing patches for this candidate
                        }
                    }

                    if candidate_success {
                        review_success = true;
                        break; // Stop processing candidates if one worked
                    }
                }

                if !review_success && final_status == ReviewStatus::Reviewed.as_str() {
                    // If we didn't succeed with any candidate, set to Failed
                    final_status = ReviewStatus::Failed.as_str().to_string();
                }

                info!(
                    "Review process finished for {}: {}",
                    patchset_id, final_status
                );
                if let Err(e) = db.update_patchset_status(patchset_id, &final_status).await {
                    error!("Failed to update status for {}: {}", patchset_id, e);
                }
            });
        }

        Ok(())
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
    quota_manager: Arc<QuotaManager>,
    cache_name: Option<&str>,
    cache_manager: Arc<CacheManager>,
    active_cache_name: Arc<Mutex<Option<String>>>,
    review_id: i64,
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

    if let Some(idx) = review_index {
        cmd.arg("--review-patch-index").arg(idx.to_string());
    }

    if let Some(name) = cache_name {
        cmd.arg("--gemini-cache").arg(name);
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
            while let Ok(Some(_line)) = lines.next_line().await {
                // Forward stderr to our log (or warn)
                // warn!("[review-bin] {}", line); // Too noisy?
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
        tokio::time::timeout(std::time::Duration::from_secs(1800), async {
            // Send initial payload
            let mut input_str = serde_json::to_string(input_payload)?;
            input_str.push('\n');
            stdin.write_all(input_str.as_bytes()).await?;
            stdin.flush().await?;

            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let client = GeminiClient::new(
                settings.ai.model.clone(),
                settings.ai.rate_limit_tokens_per_minute,
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
                                                if let Some(gemini_err) = e.downcast_ref::<GeminiError>() {
                                                    if let GeminiError::QuotaExceeded(d) = gemini_err {
                                                        quota_manager.report_quota_error(*d).await;
                                                        continue;
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
                                    let resp_payload = loop {
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
                                                            match cache_manager.ensure_cache().await {
                                                                Ok(new_name) => {
                                                                    info!("Refreshed cache: {}", new_name);
                                                                    current_req.cached_content = new_name.clone();
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
                            let stderr = p["stderr"].as_str();

                            if let Err(e) = db
                                .update_patch_application_status(patchset_id, idx, status, stderr)
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
                Err(e) => Err(e),
            }
        }
        Err(_) => {
            // Timeout occurred
            error!("Review tool timed out after 30 minutes. Killing process.");
            let _ = child.kill().await;
            Err(anyhow::anyhow!("Review tool timed out"))
        }
    }
}
