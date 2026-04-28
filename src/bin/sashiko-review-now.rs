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
    ai,
    git_ops::{self, GitWorktree},
    settings::Settings,
    worker::{Worker, prompts::PromptRegistry, tools::ToolBox},
};
use serde_json::json;
use std::path::PathBuf;
use tracing::info;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Revision range (e.g., "HEAD~2..HEAD")
    range: String,

    /// Git repository path. Defaults to current directory.
    #[arg(long, short = 'r')]
    repo: Option<PathBuf>,

    /// AI provider to use.
    #[arg(long, default_value = "gemini")]
    ai_provider: String,

    /// AI model to use.
    #[arg(long, default_value = "gemini-3.1-pro-preview")]
    ai_model: String,

    /// Prompt directory.
    #[arg(long)]
    prompts: Option<PathBuf>,

    /// Custom prompt string to append to the user task prompt.
    #[arg(long)]
    custom_prompt: Option<String>,

    /// Select which stages from 1-7 to run.
    #[arg(long, hide = true, value_delimiter = ',')]
    stages: Option<Vec<u8>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let args = Args::parse();
    let repo_path = args.repo.unwrap_or_else(|| PathBuf::from("."));
    info!(
        "Resolving baseline: {}",
        args.range.split("..").next().unwrap_or("HEAD~1")
    );
    let baseline_sha = git_ops::get_commit_hash(
        &repo_path,
        args.range.split("..").next().unwrap_or("HEAD~1"),
    )
    .await?;

    let diff_output = tokio::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["diff", &args.range])
        .output()
        .await?;

    if !diff_output.status.success() {
        anyhow::bail!(
            "Failed to generate diff for range {}: {}",
            args.range,
            String::from_utf8_lossy(&diff_output.stderr)
        );
    }
    let diff_str = String::from_utf8_lossy(&diff_output.stdout);

    let worktree = GitWorktree::new(&repo_path, &baseline_sha, None).await?;

    let res = async {
        let apply_result = worktree.apply_raw_diff(&diff_str).await?;

        if !apply_result.status.success() {
            anyhow::bail!(
                "Failed to apply diff: {}",
                String::from_utf8_lossy(&apply_result.stderr)
            );
        }

        tokio::process::Command::new("git")
            .current_dir(&worktree.path)
            .args(["add", "."])
            .output()
            .await?;

        tokio::process::Command::new("git")
            .current_dir(&worktree.path)
            .args([
                "-c",
                "user.email=sashiko@localhost",
                "-c",
                "user.name=Sashiko Bot",
                "commit",
                "-m",
                "Squashed review commit",
            ])
            .output()
            .await?;

        let squashed_sha = git_ops::get_commit_hash(&worktree.path, "HEAD").await?;

        let provider = ai::create_provider(&Settings {
            log_level: "info".to_string(),
            database: sashiko::settings::DatabaseSettings {
                url: "".to_string(),
                token: "".to_string(),
            },
            nntp: sashiko::settings::NntpSettings {
                server: "".to_string(),
                port: 0,
            },
            smtp: None,
            mailing_lists: sashiko::settings::MailingListsSettings { track: vec![] },
            ai: sashiko::settings::AiSettings {
                provider: args.ai_provider,
                model: args.ai_model,
                max_input_tokens: 900000,
                max_interactions: 50,
                temperature: 1.0,
                api_timeout_secs: 300,
                no_ai: false,
                log_turns: false,
                claude: None,
                gemini: None,
                bedrock: None,
                openai_compat: None,
            },
            server: sashiko::settings::ServerSettings {
                host: "".to_string(),
                port: 0,
                read_only: true,
            },
            git: sashiko::settings::GitSettings {
                repository_path: repo_path.to_string_lossy().to_string(),
            },
            review: sashiko::settings::ReviewSettings {
                concurrency: 1,
                worktree_dir: "review_trees".to_string(),
                timeout_seconds: 7200,
                max_retries: 3,
                max_lines_changed: 10000,
                max_files_touched: 200,
                ignore_files: vec![],
                max_total_tokens: 5000000,
                max_total_output_tokens: 500000,
                review_tool_override: None,
                stages: args.stages.clone(),
            },
        })?;

        let prompts_tool_path = args.prompts.as_ref().map(|p| p.join("tool.md"));
        let tools = ToolBox::new(worktree.path.clone(), prompts_tool_path);
        let prompts = PromptRegistry::new(args.prompts.clone());

        let series_range = Some(args.range.clone());

        let mut worker = Worker::new(
            provider,
            tools,
            prompts,
            sashiko::worker::WorkerConfig {
                max_input_tokens: 900000,
                max_interactions: 50,
                temperature: 1.0,
                custom_prompt: args.custom_prompt.clone(),
                series_range,
                stages: args.stages.clone(),
            },
        );

        let git_show = worktree
            .get_commit_show(&squashed_sha)
            .await
            .unwrap_or_default();
        let patchset_val = json!({
            "id": 1,
            "subject": format!("Squashed review of {}", args.range),
            "patches": [
                {
                    "index": 1,
                    "subject": format!("Squashed review of {}", args.range),
                    "diff": git_show,
                    "commit_id": Some(squashed_sha.clone()),
                    "git_show": git_show,
                    "commit_message_full": worktree.get_commit_message(&squashed_sha).await.unwrap_or_default(),
                }
            ],
            "patch_index": Some(1)
        });

        info!("Starting review...");
        let result = worker.run(patchset_val).await?;

        if let Some(output) = result.output {
            println!("# Review Findings Report\n");
            println!("**Concerns Count:** {}\n", output["concerns_count"]);
            if let Some(findings) = output["findings"].as_array() {
                println!("## Technical Findings\n");
                for (i, finding) in findings.iter().enumerate() {
                    let problem = finding["problem"].as_str().unwrap_or("");
                    let severity = finding["severity"].as_str().unwrap_or("");
                    let severity_explanation = finding["severity_explanation"].as_str().unwrap_or("");
                    println!("### {}. {} ({})", i + 1, problem, severity);
                    println!("{}\n", severity_explanation);
                }
            }
            if let Some(review_inline) = output["review_inline"].as_str() {
                println!("## Inline Review Comments\n");
                println!("{}\n", review_inline);
            }
        } else if let Some(err) = result.error {
            println!("Review failed: {}", err);
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    worktree.remove().await?;

    res
}
