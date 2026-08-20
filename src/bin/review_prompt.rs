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
use sashiko::settings::Settings;
use sashiko::worker::linux_prompt_workflow::run_linux_prompt_review;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about = "Review Sashiko Linux prompts changes")]
struct Args {
    /// Path to the prompt file to review
    #[arg(long)]
    file: Option<PathBuf>,

    /// Path to a git diff file to review
    #[arg(long)]
    diff: Option<PathBuf>,

    /// Git repository path for verification
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Base directory for prompt templates
    #[arg(long)]
    prompts: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let diff_content = if let Some(diff_path) = args.diff {
        tokio::fs::read_to_string(&diff_path)
            .await
            .with_context(|| format!("Failed to read diff file: {}", diff_path.display()))?
    } else if let Some(file_path) = args.file {
        let content = tokio::fs::read_to_string(&file_path)
            .await
            .with_context(|| format!("Failed to read prompt file: {}", file_path.display()))?;
        format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{count} @@\n{content}\n",
            path = file_path.display(),
            count = content.lines().count(),
            content = content
                .lines()
                .map(|l| format!("+{}", l))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        // Read diff from git diff HEAD
        let output = tokio::process::Command::new("git")
            .args(["diff", "HEAD"])
            .output()
            .await?;
        if output.stdout.is_empty() {
            let log_output = tokio::process::Command::new("git")
                .args(["show", "HEAD"])
                .output()
                .await?;
            String::from_utf8_lossy(&log_output.stdout).to_string()
        } else {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
    };

    let settings = Settings::new().ok();
    let state = run_linux_prompt_review(
        &diff_content,
        args.repo.as_deref(),
        args.prompts.as_deref(),
        settings.as_ref().map(|s| &s.ai),
    )
    .await?;

    println!(
        "=== Stage 1 Concerns (Factual / Actionability): {} ===",
        state.stage_1_concerns.len()
    );
    for c in &state.stage_1_concerns {
        println!(
            "- [{}] {}: {}",
            c.get("type").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("description").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("reasoning").and_then(|v| v.as_str()).unwrap_or("")
        );
    }
    println!(
        "\n=== Stage 2 Concerns (Codebase Verification): {} ===",
        state.stage_2_concerns.len()
    );
    for c in &state.stage_2_concerns {
        println!(
            "- [{}] {}: {}",
            c.get("type").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("description").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("reasoning").and_then(|v| v.as_str()).unwrap_or("")
        );
    }
    println!(
        "\n=== Stage 3 Concerns (Index / Placement): {} ===",
        state.stage_3_concerns.len()
    );
    for c in &state.stage_3_concerns {
        println!(
            "- [{}] {}: {}",
            c.get("type").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("description").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("reasoning").and_then(|v| v.as_str()).unwrap_or("")
        );
    }

    println!("\n=== Stage 4 Final Report ===\n{}", state.report);

    if !state.all_concerns.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}
