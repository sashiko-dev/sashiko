use crate::baseline::{BaselineRegistry, BaselineResolution, extract_files_from_diff};
use crate::db::{AiInteractionParams, Database};
use crate::git_ops::{ensure_remote, get_commit_hash};
use crate::settings::Settings;
use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Semaphore;
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
}

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

        Self {
            db,
            settings,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            baseline_registry,
        }
    }

    pub async fn start(&self) {
        info!(
            "Starting Reviewer service with concurrency limit: {}",
            self.settings.review.concurrency
        );

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

        for patchset in patchsets {
            let permit = self.semaphore.clone().acquire_owned().await?;
            let db = self.db.clone();
            let settings = self.settings.clone();
            let baseline_registry = self.baseline_registry.clone();
            let patchset_id = patchset.id;
            let subject = patchset.subject.clone().unwrap_or("Unknown".to_string());

            tokio::spawn(async move {
                let _permit = permit;

                info!("Starting review for patchset {}", patchset_id);

                if let Err(e) = db.update_patchset_status(patchset_id, "Reviewing").await {
                    error!(
                        "Failed to update status to Reviewing for {}: {}",
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
                    .map(|(_id, idx, diff)| json!({ "index": idx, "diff": diff }))
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

                let mut final_status = "Applied".to_string(); // Assume success unless failure
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
                    let fetch_warning = match &candidate {
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

                    for (patch_id, index, _diff) in &diffs {
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

                            if let Some(warning) = &fetch_warning {
                                let _ = db
                                    .update_review_status(
                                        review_id,
                                        "Applying Patches",
                                        Some(warning.as_str()),
                                    )
                                    .await;
                            } else {
                                let _ = db
                                    .update_review_status(review_id, "Applying Patches", None)
                                    .await;
                            }

                            // Run tool for SPECIFIC patch index
                            match run_review_tool(
                                patchset_id,
                                &input_payload,
                                &settings,
                                db.clone(),
                                &baseline_ref,
                                Some(*index),
                            )
                            .await
                            {
                                Ok(json_output) => {
                                    // Check patches status
                                    // The tool returns status for ALL applied patches (up to index).
                                    // We need to check if the TARGET patch (index) was applied and reviewed.
                                    let patches_status = json_output["patches"].as_array();
                                    let target_applied = patches_status
                                        .and_then(|arr| arr.iter().find(|p| p["index"] == *index))
                                        .map(|p| p["status"] == "applied")
                                        .unwrap_or(false);

                                    // Also check if ANY previous patch failed, which would prevent this one?
                                    // If target applied, we are good.

                                    if target_applied {
                                        if let Some(review_content) = json_output.get("review") {
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
                                                        "Finished",
                                                        result_desc,
                                                        Some(&summary),
                                                        Some(&interaction_id),
                                                        inline_review,
                                                    )
                                                    .await;
                                                break; // Success for this patch
                                            } else {
                                                let _ = db
                                                    .complete_review(
                                                        review_id,
                                                        "Failed",
                                                        "AI returned null response",
                                                        None,
                                                        None,
                                                        None,
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
                                                    // If max retries reached, we mark as failed but continue to next patch?
                                                    // Or fail the whole set?
                                                    // Prompt says "reviews all patches... Each review should be independent".
                                                    // So we continue.
                                                    final_status =
                                                        "Review Failed (Partial)".to_string();
                                                    break;
                                                }
                                            }
                                        } else {
                                            // Patches applied but no review (maybe list empty logic in review.rs?)
                                            let _ = db
                                                .complete_review(
                                                    review_id,
                                                    "Failed",
                                                    "Missing review content",
                                                    None,
                                                    None,
                                                    None,
                                                )
                                                .await;
                                            if retries < MAX_RETRIES {
                                                retries += 1;
                                                continue;
                                            }
                                            final_status = "Review Failed (Partial)".to_string();
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
                                                "Failed",
                                                Some(&patches_debug),
                                            )
                                            .await;
                                        let _ = db
                                            .complete_review(
                                                review_id, "Failed", error_msg, None, None, None,
                                            )
                                            .await;

                                        candidate_success = false;
                                        final_status = "Failed".to_string();
                                        break; // If application fails, we probably can't apply subsequent patches?
                                        // Actually yes, if patch 1 fails, patch 2 (which depends on 1) will fail.
                                        // So we should break the loop for this candidate.
                                    }
                                }
                                Err(e) => {
                                    error!("Review execution failed for {}: {}", patchset_id, e);
                                    let _ = db
                                        .complete_review(
                                            review_id,
                                            "Failed",
                                            &format!("Tool error: {}", e),
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
                                    final_status = "Failed".to_string();
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

                if !review_success && final_status == "Applied" {
                    // If we didn't succeed with any candidate, set to Failed
                    final_status = "Failed".to_string();
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

async fn run_review_tool(
    patchset_id: i64,
    input_payload: &serde_json::Value,
    settings: &Settings,
    db: Arc<Database>,
    baseline: &str,
    review_index: Option<i64>,
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

    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        let input_str = serde_json::to_string(input_payload)?;
        stdin.write_all(input_str.as_bytes()).await?;
    }

    let output = child.wait_with_output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            "Review tool failed (exit code {:?}): {}",
            output.status.code(),
            stderr
        );
        return Err(anyhow::anyhow!("Tool failure: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    let json: serde_json::Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(e) => {
            error!(
                "Failed to parse JSON output from review tool. Raw stdout: '{}'",
                stdout
            );
            return Err(e.into());
        }
    };

    // Update DB with patch statuses
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
