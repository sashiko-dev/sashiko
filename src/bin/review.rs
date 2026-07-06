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
use sashiko::local_review::{WorkerOptions, print_worker_json, run_worker_from_stdin};
use sashiko::prompt_bundle;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Read patchset data from JSON via stdin (deprecated: always true).
    #[arg(long)]
    json: bool,

    /// Git revision to use as baseline.
    #[arg(long)]
    baseline: Option<String>,

    /// Path to the git repository. Overrides settings.
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Parent directory for creating worktrees.
    #[arg(long)]
    worktree_dir: Option<PathBuf>,

    #[arg(long)]
    prompts: Option<PathBuf>,

    /// Review only this patch index.
    #[arg(long)]
    review_patch_index: Option<i64>,

    /// Review this commit directly without applying patches.
    #[arg(long)]
    review_commit: Option<String>,

    /// Skip AI review but still validate patch application.
    #[arg(long)]
    no_ai: bool,

    /// Reuse an existing worktree path.
    #[arg(long)]
    reuse_worktree: Option<PathBuf>,

    /// AI provider override.
    #[arg(long)]
    ai_provider: Option<String>,

    /// Custom prompt string to append to the review task prompt.
    #[arg(long)]
    custom_prompt: Option<String>,

    /// Select which stages from 1-7 to run.
    #[arg(long, hide = true, value_delimiter = ',')]
    stages: Option<Vec<u8>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("CRITICAL ERROR: Panic detected: {}", info);
    }));

    init_logging();

    let args = Args::parse();
    let result = run_worker_from_stdin(WorkerOptions {
        settings_path: None,
        baseline: args.baseline,
        repo: args.repo,
        worktree_dir: args.worktree_dir,
        prompts: args
            .prompts
            .unwrap_or(prompt_bundle::default_kernel_prompts_path()?),
        review_patch_index: args.review_patch_index,
        review_commit: args.review_commit,
        no_ai: args.no_ai,
        reuse_worktree: args.reuse_worktree,
        ai_provider: args.ai_provider,
        custom_prompt: args.custom_prompt,
        stages: args.stages,
        scratch_clone: false,
        current_tree: false,
    })
    .await?;

    print_worker_json(&result)
}

fn init_logging() {
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
}
