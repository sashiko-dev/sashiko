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

//! The `sashiko notes` command line.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::git_cmd;
use crate::maintainers::MaintainersIndex;
use crate::notes::plan::{build_plan, render_text};
use crate::notes::profile::{NotesProfile, UnitsConfig, resolve_profile_path};
use crate::notes::scope::resolve_revision;

/// `sashiko notes` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum NotesCommand {
    /// Show how a notes build would be scoped (units, components, entry
    /// points) and estimate its cost, without calling an LLM
    Plan {
        /// Only plan these units (name or slug; repeatable)
        #[arg(long = "unit")]
        units: Vec<String>,

        /// Revision to scope (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        rev: String,

        /// Notes profile (default: notes/profile.toml in the project's
        /// prompt directory)
        #[arg(long)]
        profile: Option<PathBuf>,

        /// Print the plan as JSON
        #[arg(long)]
        json: bool,
    },
}

/// Runs a `sashiko notes` command in the repository `repo`, reading the
/// profile from `prompts_dir` unless one is given explicitly.
pub async fn run(command: &NotesCommand, repo: &Path, prompts_dir: &Path) -> Result<()> {
    match command {
        NotesCommand::Plan {
            units,
            rev,
            profile,
            json,
        } => {
            let profile_path = resolve_profile_path(prompts_dir, profile.as_deref())?;
            let profile = NotesProfile::load(&profile_path)?;
            let sha = resolve_revision(repo, rev).await?;
            let maintainers = match profile.units {
                UnitsConfig::Maintainers {} => Some(load_maintainers_at(repo, &sha).await?),
                UnitsConfig::Directories { .. } => None,
            };
            let report = build_plan(repo, &sha, &profile, maintainers.as_ref(), units).await?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render_text(&report));
            }
            Ok(())
        }
    }
}

/// Reads `MAINTAINERS` from commit `sha`, so the units match the revision
/// being scoped rather than the working tree.
async fn load_maintainers_at(repo: &Path, sha: &str) -> Result<MaintainersIndex> {
    let output = git_cmd::in_dir_async(repo)
        .args(["show", &format!("{sha}:MAINTAINERS")])
        .output()
        .await
        .context("failed to run git show")?;
    if !output.status.success() {
        anyhow::bail!("no MAINTAINERS file at {sha}");
    }
    MaintainersIndex::from_reader(output.stdout.as_slice())
}
