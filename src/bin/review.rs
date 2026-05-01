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

use anyhow::Result;
use clap::Parser;
use sashiko::{
    git_ops::GitWorktree,
    settings::Settings,
    worker::{
        PatchInput, Worker, WorkerConfig, calculate_series_range, prompts::PromptRegistry,
        tools::ToolBox,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::IsTerminal;
use std::path::PathBuf;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Read patchset data from JSON via Stdin (Deprecated: Always true).
    #[arg(long)]
    json: bool,

    /// Git revision to use as baseline (e.g. "HEAD", "v6.12", or commit hash).
    /// Defaults to "HEAD" if not specified.
    #[arg(long)]
    baseline: Option<String>,

    /// Parent directory for creating worktrees.
    #[arg(long)]
    worktree_dir: Option<PathBuf>,

    #[arg(long)]
    prompts: Option<PathBuf>,

    /// If set, only review the patch with this index (1-based usually).
    /// Previous patches (with lower index) will be applied but not reviewed.
    #[arg(long)]
    review_patch_index: Option<i64>,

    /// If set, checks out this specific commit hash and reviews it.
    /// Skips patch application logic.
    #[arg(long)]
    review_commit: Option<String>,

    /// If set, skip AI review but still apply patches for verification.
    #[arg(long)]
    no_ai: bool,

    /// If set, use this existing worktree path instead of creating a new one.
    /// The caller is responsible for cleanup.
    #[arg(long)]
    reuse_worktree: Option<PathBuf>,

    /// AI provider to use. Overrides settings.
    #[arg(long)]
    ai_provider: Option<String>,

    /// Custom prompt string to append to the user task prompt.
    #[arg(long)]
    custom_prompt: Option<String>,

    /// Select which stages from 1-7 to run.
    #[arg(long, hide = true, value_delimiter = ',')]
    stages: Option<Vec<u8>>,
}

#[derive(Deserialize, Serialize, Debug)]
struct ReviewInput {
    id: i64,
    subject: String,
    patches: Vec<PatchInput>,
}

#[tokio::main]
async fn main() -> Result<()> {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("CRITICAL ERROR: Panic detected: {}", info);
    }));

    let no_color = std::env::var("NO_COLOR").is_ok();
    let plain_logs = std::env::var("SASHIKO_LOG_PLAIN").is_ok();
    let use_ansi = !no_color && std::io::stderr().is_terminal();

    let builder = tracing_subscriber::fmt()
        .with_writer(sashiko::logging::IgnoreBrokenPipe(std::io::stderr))
        .with_ansi(use_ansi);

    if plain_logs {
        builder
            .with_level(false)
            .with_target(false)
            .without_time()
            .init();
    } else {
        builder.init();
    }

    let args = Args::parse();
    let mut settings = Settings::new().expect("Failed to load settings");

    if let Some(p) = &args.ai_provider {
        settings.ai.provider = p.clone();
    }

    // Data Loading: Always from Stdin (JSON)
    let mut buffer = String::new();
    if std::io::stdin().read_line(&mut buffer)? == 0 {
        anyhow::bail!("No input provided on stdin");
    }
    let input: ReviewInput = serde_json::from_str(&buffer)?;

    info!(
        "Loaded patchset via JSON: {} (ID: {})",
        input.subject, input.id
    );
    let (patchset_id, subject, patches) = (input.id, input.subject, input.patches);
    let baseline_arg = args.baseline.clone().unwrap_or_else(|| "HEAD".to_string());
    let repo_path = PathBuf::from(&settings.git.repository_path);

    let run_logic = async {
        info!("Resolving baseline: {}", baseline_arg);
        let baseline_sha = sashiko::git_ops::get_commit_hash(&repo_path, &baseline_arg).await?;
        info!("Using baseline: {} ({})", baseline_arg, baseline_sha);

        // Use provided or default baseline
        let worktree = if let Some(path) = &args.reuse_worktree {
            info!("Reusing existing worktree at {:?}", path);
            GitWorktree::from_path(path.clone(), repo_path.clone())
        } else {
            GitWorktree::new(&repo_path, &baseline_sha, args.worktree_dir.as_deref()).await?
        };

        let result = async {
            info!("Worktree at {:?}", worktree.path);
            info!("Found {} patches total", patches.len());

            let mut patch_results = Vec::new();
            let mut patch_shas = std::collections::HashMap::new();
            let mut patch_shows = std::collections::HashMap::new();
            let mut patch_messages = std::collections::HashMap::new();

            let all_applied;

            if let Some(commit_hash) = &args.review_commit {
                info!("Directly reviewing commit {}", commit_hash);
                // We assume the caller has already validated the series and applied it.
                // We just checkout the specific commit.
                // Note: The commit must exist in the repo (shared object store).

                // Fetch/Checkout logic for worktree
                // GitWorktree::new checks out 'baseline' initially.
                // We need to fetch/reset to the specific commit.
                // Since it's a shared repo, we can just reset --hard to the hash.
                if let Err(e) = worktree.reset_hard(commit_hash).await {
                     error!("Failed to checkout target commit {}: {}", commit_hash, e);
                     let result_json = json!({
                        "patchset_id": patchset_id,
                        "baseline": baseline_arg,
                        "patches": patch_results,
                        "error": format!("Failed to checkout target commit: {}", e)
                    });
                    println!("{}", serde_json::to_string(&result_json)?);
                    return Ok(());
                }

                all_applied = true;

                // Populate metadata for the single patch we are reviewing
                if let Some(idx) = args.review_patch_index {
                     patch_shas.insert(idx, commit_hash.clone());
                     if let Ok(show) = worktree.get_commit_show(commit_hash).await {
                        patch_shows.insert(idx, show);
                    }
                    // Fake a success result for this patch so the report looks good
                    patch_results.push(json!({
                        "index": idx,
                        "status": "applied",
                        "method": "pre-applied"
                    }));
                }
            } else {
                // 1. Apply ALL patches to validate the series
                info!(
                    "Applying all {} patches to validate series...",
                    patches.len()
                );
                let mut applied_flag = true;

                for p in &patches {
                    info!("Applying patch part {}", p.index);

                    let success = apply_single_patch(
                        &worktree,
                        p,
                        &mut patch_shas,
                        &mut patch_shows,
                        &mut patch_messages,
                        &mut patch_results,
                    )
                    .await;

                    if !success {
                        applied_flag = false;
                    }
                }
                all_applied = applied_flag;
            }

            // Determine patches to review
            let mut patches_to_review: Vec<PatchInput> = if let Some(target_idx) = args.review_patch_index {
                patches
                    .iter()
                    .filter(|p| p.index == target_idx)
                    .cloned()
                    .collect()
            } else {
                patches.clone() // Review all
            };

            if args.no_ai {
                info!("Skipping AI review due to --no-ai flag.");
                patches_to_review.clear();
            }

            if all_applied {
                // 2. Prepare worktree context if reviewing a specific patch
                // Only needed if we didn't use review_commit (which already sets context)
                if args.review_commit.is_none()
                    && let Some(target_idx) = args.review_patch_index {

                    // Optimization: Only reset if target_idx < max_index
                    let max_index = patches.iter().map(|p| p.index).max().unwrap_or(0);

                    if target_idx < max_index {
                        info!(
                            "Resetting worktree to baseline to prepare context for patch {}...",
                            target_idx
                        );
                        if let Err(e) = worktree.reset_hard(&baseline_sha).await {
                            error!("Failed to reset worktree: {}", e);
                            // If reset fails, we can't proceed safely.
                            // Report error.
                            let result_json = json!({
                                "patchset_id": patchset_id,
                                "baseline": baseline_arg,
                                "patches": patch_results,
                                "error": format!("Failed to reset worktree: {}", e)
                            });
                            println!("{}", serde_json::to_string(&result_json)?);
                            return Ok(());
                        }

                        info!("Re-applying patches up to index {}...", target_idx);
                        // We use dummy containers because we already have results/shas from validation pass
                        let mut dummy_results = Vec::new();
                        let mut dummy_shas = std::collections::HashMap::new();
                        let mut dummy_shows = std::collections::HashMap::new();

                        let mut dummy_msgs = std::collections::HashMap::new();

                        let patches_subset: Vec<&PatchInput> =
                            patches.iter().filter(|p| p.index <= target_idx).collect();
                        for p in patches_subset {
                            let success = apply_single_patch(
                                &worktree,
                                p,
                                &mut dummy_shas,
                                &mut dummy_shows,
                                &mut dummy_msgs,
                                &mut dummy_results,
                            )
                            .await;

                            if !success {
                                // Inconsistent state: patch applied successfully on first pass but failed on second.
                                error!("Patch {} failed to apply on second pass!", p.index);
                                let result_json = json!({
                                    "patchset_id": patchset_id,
                                    "baseline": baseline_arg,
                                    "patches": patch_results,
                                    "error": "Inconsistent patch application (failed on re-apply)"
                                });
                                println!("{}", serde_json::to_string(&result_json)?);
                                return Ok(());
                            }
                        }
                    }
                }

                if patches_to_review.is_empty() {
                    info!("No patches matched review index or list empty. Skipping AI review.");
                    // Return success with patches status (even if we didn't review anything)
                    let result_json = json!({
                        "patchset_id": patchset_id,
                        "baseline": baseline_arg,
                        "patches": patch_results,
                        "review": null, // Indicate no review
                        "input_context": "",
                        "tokens_in": 0,
                        "tokens_out": 0,
                        "tokens_cached": 0
                    });
                    println!("{}", serde_json::to_string(&result_json)?);
                } else {
                    info!(
                        "Patches applied. Starting AI review for {} patches...",
                        patches_to_review.len()
                    );

                    let rich_patches: Vec<serde_json::Value> = patches_to_review
                        .iter()
                        .map(|p| {
                            let date_str = if let Some(ts) = p.date {
                                std::process::Command::new("date")
                                    .arg("-R")
                                    .arg("-d")
                                    .arg(format!("@{}", ts))
                                    .output()
                                    .ok()
                                    .and_then(|o| {
                                        if o.status.success() {
                                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or_default()
                                } else {
                                    String::new()
                                };

                            json!({
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

                    let patchset_val = json!({
                        "id": patchset_id,
                        "subject": subject,
                        "patches": rich_patches,
                        "patch_index": args.review_patch_index
                    });

                    let mut review_result_to_print = None;

                    for attempt in 1..=3 {
                        if attempt > 1 {
                            info!("Restarting AI review (attempt {}/3)...", attempt);
                        }

                        // Use stdio-gemini for the binary as it expects to communicate with parent
                        let provider = sashiko::ai::create_provider(&settings).expect("Failed to create AI provider");

                        // Enable read_prompt tool only if explicit caching is NOT used.
                        let prompts_dir = args
                            .prompts
                            .clone()
                            .unwrap_or_else(|| PathBuf::from(settings.get_prompts_dir()));
                        let prompts_tool_path = Some(prompts_dir.join("tool.md"));

                        let tools = ToolBox::with_config(
                            worktree.path.clone(),
                            prompts_tool_path,
                            settings.tools.as_ref(),
                        );

                        // Use settings if available, otherwise fall back to args.prompts
                        let prompts = if settings.prompts.is_some() {
                            PromptRegistry::with_settings(settings.prompts.as_ref())
                                .await
                                .expect("Failed to load prompts with settings")
                        } else {
                            PromptRegistry::new(prompts_dir)
                        };

                        // Calculate series range (baseline..last_patch)
                        let series_range = calculate_series_range(
                            &patches,
                            &patches_to_review,
                            &patch_shas,
                            &baseline_sha,
                        );

                        let mut worker = Worker::new(
                            provider,
                            tools,
                            prompts,
                            WorkerConfig {
                                max_input_tokens: settings.ai.max_input_tokens,
                                max_interactions: settings.ai.max_interactions,
                                temperature: settings.ai.temperature,
                                custom_prompt: args.custom_prompt.clone(),
                                series_range,
                                stages: args.stages.clone(),
                            },
                        );

                        match worker.run(patchset_val.clone()).await {
                            Ok(result) => {
                                info!("AI review completed (or stopped).");

                                // Extract review_inline from JSON
                                let mut inline_content = None;
                                if let Some(output) = &result.output
                                    && let Some(content) = output.get("review_inline").and_then(|v| v.as_str()) {
                                        inline_content = Some(content.to_string());
                                    }

                                // Check for missing inline review with findings
                                let mut has_findings = false;
                                if let Some(output) = &result.output
                                    && let Some(findings) = output.get("findings").and_then(|f| f.as_array())
                                        && !findings.is_empty() {
                                            has_findings = true;
                                        }

                                if has_findings && inline_content.is_none() {
                                    error!("Review failure: Findings detected but review_inline field was missing or empty.");
                                    if attempt < 3 {
                                        continue;
                                    }
                                }

                                review_result_to_print = Some(json!({
                                    "patchset_id": patchset_id,
                                    "baseline": baseline_arg,
                                    "patches": patch_results,
                                    "review": result.output,
                                    "error": result.error,
                                    "inline_review": inline_content,
                                    "input_context": result.input_context,
                                    "history": result.history,
                                    "tokens_in": result.tokens_in,
                                    "tokens_out": result.tokens_out,
                                    "tokens_cached": result.tokens_cached
                                }));
                                break;
                            }
                            Err(e) => {
                                error!("AI review failed with exception: {}", e);
                                if attempt < 3 {
                                    continue;
                                }
                                // Even on failure, we print what we have (patches status)
                                review_result_to_print = Some(json!({
                                    "patchset_id": patchset_id,
                                    "baseline": baseline_arg,
                                    "patches": patch_results,
                                    "error": e.to_string(),
                                    "tokens_in": 0,
                                    "tokens_out": 0,
                                    "tokens_cached": 0
                                }));
                                break;
                            }
                        }
                    }

                    if let Some(json) = review_result_to_print {
                        println!("{}", serde_json::to_string(&json)?);
                    } else {
                        let result_json = json!({
                            "patchset_id": patchset_id,
                            "baseline": baseline_arg,
                            "patches": patch_results,
                            "error": "Internal error: Review loop finished without result"
                        });
                        println!("{}", serde_json::to_string(&result_json)?);
                    }
                }
            } else {
                info!("Not all patches applied successfully. Skipping AI review.");
                let result_json = json!({
                    "patchset_id": patchset_id,
                    "baseline": baseline_arg,
                    "patches": patch_results,
                    "error": "Patch application failed"
                });
                println!("{}", serde_json::to_string(&result_json)?);
            }
            Ok::<(), anyhow::Error>(())
        }.await;

        if let Err(e) = worktree.remove().await {
            error!("Failed to remove worktree: {}", e);
        }

        result
    };

    if let Err(e) = run_logic.await {
        error!("Critical error in review tool: {}", e);
        let error_json = json!({
            "patchset_id": patchset_id,
            "baseline": baseline_arg,
            "error": e.to_string()
        });
        println!("{}", serde_json::to_string(&error_json)?);
    }
    Ok(())
}

async fn apply_single_patch(
    worktree: &GitWorktree,
    p: &PatchInput,
    patch_shas: &mut std::collections::HashMap<i64, String>,
    patch_shows: &mut std::collections::HashMap<i64, String>,
    patch_messages: &mut std::collections::HashMap<i64, String>,
    patch_results: &mut Vec<serde_json::Value>,
) -> bool {
    // Check if commit_id is present (preferred over message_id guessing)
    if let Some(sha) = &p.commit_id {
        info!(
            "Patch {} is identified by commit ID {}, attempting direct checkout...",
            p.index, sha
        );
        match worktree.reset_hard(sha).await {
            Ok(_) => {
                if let Ok(show) = worktree.get_commit_show(sha).await {
                    patch_shows.insert(p.index, show);
                }
                if let Ok(msg) = worktree.get_commit_message(sha).await {
                    patch_messages.insert(p.index, msg);
                }
                patch_shas.insert(p.index, sha.clone());
                patch_results.push(json!({
                    "index": p.index,
                    "status": "applied",
                    "method": "checkout"
                }));
                return true;
            }
            Err(e) => {
                error!("Failed to checkout commit {}: {}", sha, e);
                patch_results.push(json!({
                    "index": p.index,
                    "status": "error",
                    "method": "checkout",
                    "error": e.to_string()
                }));
                return false;
            }
        }
    }

    // Legacy fallback: Check if message_id looks like a SHA
    if let Some(sha) = &p.message_id
        && sha.len() == 40
        && sha.chars().all(|c| c.is_ascii_hexdigit())
    {
        info!(
            "Patch {} message_id looks like a SHA {}, checking out...",
            p.index, sha
        );
        match worktree.reset_hard(sha).await {
            Ok(_) => {
                if let Ok(show) = worktree.get_commit_show(sha).await {
                    patch_shows.insert(p.index, show);
                }
                if let Ok(msg) = worktree.get_commit_message(sha).await {
                    patch_messages.insert(p.index, msg);
                }
                patch_shas.insert(p.index, sha.clone());
                patch_results.push(json!({
                    "index": p.index,
                    "status": "applied",
                    "method": "checkout"
                }));
                return true;
            }
            Err(e) => {
                error!("Failed to checkout commit {}: {}", sha, e);
                patch_results.push(json!({
                    "index": p.index,
                    "status": "error",
                    "method": "checkout",
                    "error": e.to_string()
                }));
                return false;
            }
        }
    }

    let mut applied_via_am = false;
    let mut am_error = String::new();
    let mut success = false;

    if let (Some(author), Some(subject)) = (&p.author, &p.subject) {
        // Try to construct mbox
        let date_str = if let Some(ts) = p.date {
            // Try format date using system date command
            let output = std::process::Command::new("date")
                .arg("-R")
                .arg("-d")
                .arg(format!("@{}", ts))
                .output();
            match output {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                _ => String::new(), // Fallback to no date (git am uses current)
            }
        } else {
            String::new()
        };

        let mbox = format!(
            "From: {}\nDate: {}\nSubject: {}\n\n{}\n",
            author, date_str, subject, p.diff
        );

        match worktree.apply_patch(&mbox).await {
            Ok(_) => {
                applied_via_am = true;
                success = true;
                if let Ok(sha) = sashiko::git_ops::get_commit_hash(&worktree.path, "HEAD").await {
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
                    "method": "git-am"
                }));
            }
            Err(e) => {
                info!("git am failed, falling back to git apply: {}", e);
                am_error = e.to_string();
            }
        }
    }

    if !applied_via_am {
        match worktree.apply_raw_diff(&p.diff).await {
            Ok(output) => {
                let status = if output.status.success() {
                    success = true;
                    "applied"
                } else {
                    "failed"
                };
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

                if status == "failed" {
                    info!(
                        "Failed to apply patch {}. stdout: {}\nstderr: {}",
                        p.index, stdout, stderr
                    );
                }

                patch_results.push(json!({
                    "index": p.index,
                    "status": status,
                    "method": "git-apply",
                    "stdout": stdout,
                    "stderr": stderr,
                    "exit_code": output.status.code(),
                    "am_error": if !am_error.is_empty() { Some(am_error) } else { None }
                }));
            }
            Err(e) => {
                info!("Error applying patch {}: {}", p.index, e);
                patch_results.push(json!({
                    "index": p.index,
                    "status": "error",
                    "method": "git-apply",
                    "error": e.to_string(),
                    "am_error": if !am_error.is_empty() { Some(am_error) } else { None }
                }));
            }
        }
    }

    success
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::process::Command;

    #[tokio::test]
    async fn test_apply_single_patch_remote_checkout() -> Result<()> {
        // 1. Setup a dummy repo
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()?;

        // Initial commit
        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Initial")?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial"])
            .output()?;

        let initial_sha = sashiko::git_ops::get_commit_hash(&repo_path, "HEAD").await?;

        // Second commit (The one we want to checkout)
        writeln!(file, "Change")?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Feature"])
            .output()?;

        let feature_sha = sashiko::git_ops::get_commit_hash(&repo_path, "HEAD").await?;

        // 2. Setup Worktree on Initial commit (Baseline)
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        // 3. Prepare PatchInput with feature_sha as commit_id and BROKEN diff
        let patch = PatchInput {
            index: 1,
            diff: "INVALID DIFF content that would fail git apply".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some("some-msg-id".to_string()),
            commit_id: Some(feature_sha.clone()),
        };

        let mut patch_shas = std::collections::HashMap::new();
        let mut patch_shows = std::collections::HashMap::new();
        let mut patch_messages = std::collections::HashMap::new();
        let mut patch_results = Vec::new();

        // 4. Run apply_single_patch
        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        // 5. Verify
        assert!(success, "Should succeed via checkout despite invalid diff");

        // Check result JSON
        let result = &patch_results[0];
        assert_eq!(result["status"], "applied");
        assert_eq!(result["method"], "checkout");

        // Verify worktree content matches feature commit
        let content = std::fs::read_to_string(worktree.path.join("file.txt"))?;
        assert!(content.contains("Change"));

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_checkout_failure() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // 1. Setup a dummy repo
        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()?;

        // Initial commit
        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Initial")?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial"])
            .output()?;

        let initial_sha = sashiko::git_ops::get_commit_hash(&repo_path, "HEAD").await?;

        // 2. Setup Worktree
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        // 3. Prepare PatchInput with NON-EXISTENT SHA
        // Use a valid SHA format but non-existent
        let missing_sha = "0000000000000000000000000000000000000000".to_string();
        let patch = PatchInput {
            index: 1,
            diff: "Valid Diff content that would apply if we fell back".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some("some-msg-id".to_string()),
            commit_id: Some(missing_sha.clone()),
        };

        let mut patch_shas = std::collections::HashMap::new();
        let mut patch_shows = std::collections::HashMap::new();
        let mut patch_messages = std::collections::HashMap::new();
        let mut patch_results = Vec::new();

        // 4. Run apply_single_patch
        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        // 5. Verify Failure
        assert!(!success, "Should fail because checkout failed");

        // Check result JSON
        let result = &patch_results[0];
        assert_eq!(result["status"], "error");
        assert_eq!(result["method"], "checkout"); // Failed at checkout stage

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_legacy_message_id_sha() -> Result<()> {
        // Test backward compatibility where message_id is a SHA and commit_id is None
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Initial")?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial"])
            .output()?;
        let initial_sha = sashiko::git_ops::get_commit_hash(&repo_path, "HEAD").await?;

        writeln!(file, "Change")?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Feature"])
            .output()?;
        let feature_sha = sashiko::git_ops::get_commit_hash(&repo_path, "HEAD").await?;

        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "INVALID".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some(feature_sha.clone()),
            commit_id: None,
        };

        let mut patch_shas = std::collections::HashMap::new();
        let mut patch_shows = std::collections::HashMap::new();
        let mut patch_messages = std::collections::HashMap::new();
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
