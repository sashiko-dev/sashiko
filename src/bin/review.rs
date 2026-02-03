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
    worker::{Worker, prompts::PromptRegistry, tools::ToolBox},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
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

    #[arg(long, default_value = "review-prompts/kernel")]
    prompts: PathBuf,

    /// If set, only review the patch with this index (1-based usually).
    /// Previous patches (with lower index) will be applied but not reviewed.
    #[arg(long)]
    review_patch_index: Option<i64>,

    /// Resource name of the Gemini Context Cache to use (e.g. cachedContents/...).
    #[arg(long)]
    gemini_cache: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct PatchInput {
    index: i64,
    diff: String,
    subject: Option<String>,
    author: Option<String>,
    date: Option<i64>,
}

#[derive(Deserialize, Serialize, Debug)]
struct ReviewInput {
    id: i64,
    subject: String,
    patches: Vec<PatchInput>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let no_color = std::env::var("NO_COLOR").is_ok();
    let plain_logs = std::env::var("SASHIKO_LOG_PLAIN").is_ok();

    let builder = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(!no_color);

    if plain_logs {
        builder.without_time().init();
    } else {
        builder.init();
    }

    let args = Args::parse();
    let settings = Settings::new().unwrap();

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

    let baseline = args.baseline.unwrap_or_else(|| "HEAD".to_string());
    info!("Using baseline: {}", baseline);

    let repo_path = PathBuf::from(&settings.git.repository_path);
    // Use provided or default baseline
    let worktree = GitWorktree::new(&repo_path, &baseline, args.worktree_dir.as_deref()).await?;

    info!("Created worktree at {:?}", worktree.path);
    info!("Found {} patches total", patches.len());

    let mut patch_results = Vec::new();
    let mut all_applied = true;

    // Filter patches to apply: all patches with index <= review_patch_index (if set), or all patches
    let patches_to_apply: Vec<&PatchInput> = if let Some(target_idx) = args.review_patch_index {
        patches.iter().filter(|p| p.index <= target_idx).collect()
    } else {
        patches.iter().collect()
    };

    info!("Applying {} patches...", patches_to_apply.len());

    for p in &patches_to_apply {
        info!("Applying patch part {}", p.index);

        let mut applied_via_am = false;
        let mut am_error = String::new();

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
                        "applied"
                    } else {
                        all_applied = false;
                        "failed"
                    };
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                    if status == "failed" {
                        info!("Failed to apply patch {}: {}", p.index, stderr);
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
                    all_applied = false;
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
    }

    // Determine patches to review
    let patches_to_review: Vec<PatchInput> = if let Some(target_idx) = args.review_patch_index {
        patches
            .iter()
            .filter(|p| p.index == target_idx)
            .cloned()
            .collect()
    } else {
        patches.clone() // Review all
    };

    if all_applied {
        if patches_to_review.is_empty() {
            info!("No patches matched review index or list empty. Skipping AI review.");
            // Return success with patches status (even if we didn't review anything)
            let result_json = json!({
                "patchset_id": patchset_id,
                "baseline": baseline,
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

            let client = Box::new(sashiko::ai::gemini::StdioGeminiClient);

            // Enable read_prompt tool only if explicit caching is NOT used.
            let prompts_tool_path = if args.gemini_cache.is_none() {
                Some(args.prompts.clone())
            } else {
                None
            };

            let tools = ToolBox::new(worktree.path.clone(), prompts_tool_path);
            let prompts = PromptRegistry::new(args.prompts.clone());
            let mut worker = Worker::new(
                client,
                tools,
                prompts,
                settings.ai.max_input_words,
                settings.ai.max_interactions,
                settings.ai.temperature,
                args.gemini_cache,
            );

            let patchset_val = json!({
                "id": patchset_id,
                "subject": subject,
                "patches": patches_to_review
            });

            match worker.run(patchset_val).await {
                Ok(result) => {
                    info!("AI review completed (or stopped).");

                    // Check for review-inline.txt
                    let inline_path = worktree.path.join("review-inline.txt");
                    let inline_content = if inline_path.exists() {
                        match std::fs::read_to_string(&inline_path) {
                            Ok(content) => Some(content),
                            Err(e) => {
                                error!("Failed to read review-inline.txt: {}", e);
                                None
                            }
                        }
                    } else {
                        None
                    };

                    let result_json = json!({
                        "patchset_id": patchset_id,
                        "baseline": baseline,
                        "patches": patch_results,
                        "review": result.output,
                        "error": result.error,
                        "inline_review": inline_content,
                        "input_context": result.input_context,
                        "history": result.history,
                        "tokens_in": result.tokens_in,
                        "tokens_out": result.tokens_out,
                        "tokens_cached": result.tokens_cached
                    });
                    println!("{}", serde_json::to_string(&result_json)?);
                }
                Err(e) => {
                    error!("AI review failed with exception: {}", e);
                    // Even on failure, we print what we have (patches status)
                    let result_json = json!({
                        "patchset_id": patchset_id,
                        "baseline": baseline,
                        "patches": patch_results,
                        "error": e.to_string(),
                        "tokens_in": 0,
                        "tokens_out": 0,
                        "tokens_cached": 0
                    });
                    println!("{}", serde_json::to_string(&result_json)?);
                }
            }
        }
    } else {
        info!("Not all patches applied successfully. Skipping AI review.");
        let result_json = json!({
            "patchset_id": patchset_id,
            "baseline": baseline,
            "patches": patch_results,
            "error": "Patch application failed"
        });
        println!("{}", serde_json::to_string(&result_json)?);
    }

    worktree.remove().await?;

    Ok(())
}
