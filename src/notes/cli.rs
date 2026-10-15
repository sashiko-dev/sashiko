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
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use crate::git_cmd;
use crate::maintainers::MaintainersIndex;
use crate::notes::agenda::load_agenda;
use crate::notes::build::{BuildConfig, build_unit};
use crate::notes::corpus;
use crate::notes::plan::{build_plan, render_text};
use crate::notes::profile::{NotesProfile, UnitsConfig, resolve_profile_path};
use crate::notes::scope::resolve_revision;
use crate::project::ProjectId;
use crate::settings::Settings;
use crate::toolbox::ToolBox;
use crate::workflow::WorkflowEnv;

/// Default turn budget of one survey, explore or verify stage.
const DEFAULT_STAGE_TURNS: usize = 30;

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

    /// Build notes for units: survey, explore, verify, synthesize, and write
    /// them into a notes set
    Build {
        /// Units to build (name or slug; repeatable, at least one)
        #[arg(long = "unit", required = true)]
        units: Vec<String>,

        /// Only explore these components of the selected units (name;
        /// repeatable). Default: every component.
        #[arg(long = "component")]
        components: Vec<String>,

        /// Revision to build from (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        rev: String,

        /// Notes set to write into, e.g. "6.18"; a set holds one revision
        #[arg(long)]
        set: String,

        /// Notes root directory; the set is written to <out>/<set>/
        #[arg(long)]
        out: PathBuf,

        /// Notes profile (default: notes/profile.toml in the project's
        /// prompt directory)
        #[arg(long)]
        profile: Option<PathBuf>,

        /// Reuse finished steps saved in the work directory
        #[arg(long)]
        resume: bool,

        /// Turn budget of each survey, explore and verify stage
        #[arg(long, default_value_t = DEFAULT_STAGE_TURNS)]
        max_turns: usize,
    },

    /// Analyze the project's fixes from their code (the corpus the agenda
    /// is derived from), appending to <out>/corpus-<k>-of-<n>.jsonl
    Corpus {
        /// Only fixes reachable from this revision are read (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        rev: String,

        /// Directory for the corpus files
        #[arg(long)]
        out: PathBuf,

        /// Only the newest this many fixes
        #[arg(long)]
        limit: Option<usize>,

        /// A deterministic sample of this many of those fixes, for
        /// calibration runs
        #[arg(long)]
        sample: Option<usize>,

        /// Seed of the sample
        #[arg(long, default_value = "sashiko")]
        seed: String,

        /// The share of the fixes this process analyzes, as k/n
        #[arg(long, default_value = "0/1", value_parser = parse_shard)]
        shard: (usize, usize),

        /// Notes profile (default: notes/profile.toml in the project's
        /// prompt directory)
        #[arg(long)]
        profile: Option<PathBuf>,
    },
}

/// Parses a shard given as "k/n" with k < n.
fn parse_shard(text: &str) -> Result<(usize, usize), String> {
    let (k, n) = text
        .split_once('/')
        .ok_or_else(|| format!("expected k/n, got {text:?}"))?;
    let k: usize = k
        .trim()
        .parse()
        .map_err(|_| format!("bad shard index {k:?}"))?;
    let n: usize = n
        .trim()
        .parse()
        .map_err(|_| format!("bad shard count {n:?}"))?;
    if n == 0 || k >= n {
        return Err(format!("shard {k}/{n} needs 0 <= k < n"));
    }
    Ok((k, n))
}

/// Runs a `sashiko notes` command in the repository `repo`, reading the
/// profile from `prompts_dir` unless one is given explicitly. `settings` is
/// required by commands that call an LLM.
pub async fn run(
    command: &NotesCommand,
    project: ProjectId,
    repo: &Path,
    prompts_dir: &Path,
    settings: Option<&Settings>,
) -> Result<()> {
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
        NotesCommand::Build {
            units,
            components,
            rev,
            set,
            out,
            profile,
            resume,
            max_turns,
        } => {
            let settings = settings
                .context("notes build calls an LLM and needs a settings file (see sashiko init)")?;
            if !is_safe_set_id(set) {
                bail!("--set must be a plain name such as \"6.18\", got {set:?}");
            }
            let profile_path = resolve_profile_path(prompts_dir, profile.as_deref())?;
            let profile = NotesProfile::load(&profile_path)?;
            let agenda = load_agenda(prompts_dir, &profile.agenda.modules)?;
            let sha = resolve_revision(repo, rev).await?;
            let maintainers = match profile.units {
                UnitsConfig::Maintainers {} => Some(load_maintainers_at(repo, &sha).await?),
                UnitsConfig::Directories { .. } => None,
            };
            let mut report = build_plan(repo, &sha, &profile, maintainers.as_ref(), units).await?;
            if !components.is_empty() {
                for unit in &mut report.units {
                    unit.components.retain(|c| components.contains(&c.name));
                }
                report.units.retain(|u| !u.components.is_empty());
                if report.units.is_empty() {
                    bail!("no selected unit has any of the components {components:?}");
                }
            }

            let env = llm_env(settings, repo, prompts_dir, &sha).await?;
            let cfg = BuildConfig {
                project: project.display_name().to_string(),
                set_id: set.clone(),
                revision: sha.clone(),
                set_dir: out.join(set),
                work_dir: crate::utils::data_home()?
                    .join("sashiko/notes-work")
                    .join(project.as_str())
                    .join(set)
                    // Saved steps belong to one revision; resuming must never
                    // mix in results built from another.
                    .join(&sha),
                resume: *resume,
                max_turns: *max_turns,
                temperature: settings.ai.temperature,
                model: settings.ai.model.clone(),
                note_bytes: profile.limits.unit_note_bytes,
                history: profile.history.clone(),
            };

            let mut failures = Vec::new();
            for unit in &report.units {
                match build_unit(repo, &env, &agenda, unit, &cfg).await {
                    Ok(summary) => println!("{}", serde_json::to_string(&summary)?),
                    Err(err) => {
                        eprintln!("{}: build failed: {err:#}", unit.name);
                        failures.push(unit.name.clone());
                    }
                }
            }
            if !failures.is_empty() {
                bail!("{} unit(s) failed: {failures:?}", failures.len());
            }
            Ok(())
        }
        NotesCommand::Corpus {
            rev,
            out,
            limit,
            sample,
            seed,
            shard,
            profile,
        } => {
            let settings = settings.context(
                "notes corpus calls an LLM and needs a settings file (see sashiko init)",
            )?;
            let profile_path = resolve_profile_path(prompts_dir, profile.as_deref())?;
            let profile = NotesProfile::load(&profile_path)?;
            let sha = resolve_revision(repo, rev).await?;
            let mut fixes =
                corpus::list_fixes(repo, &sha, &profile.history.fix_patterns, *limit).await?;
            if let Some(size) = sample {
                fixes = corpus::sample(&fixes, *size, seed);
            }
            let (index, count) = *shard;
            let fixes = corpus::shard(&fixes, index, count);
            std::fs::create_dir_all(out)
                .with_context(|| format!("failed to create {}", out.display()))?;
            let env = llm_env(settings, repo, prompts_dir, &sha).await?;
            let cfg = corpus::CorpusConfig {
                project: project.display_name().to_string(),
                out_file: out.join(format!("corpus-{index}-of-{count}.jsonl")),
                temperature: settings.ai.temperature,
                introducer: profile.history.introducer()?,
            };
            eprintln!(
                "analyzing {} fixes reachable from {sha} into {}",
                fixes.len(),
                cfg.out_file.display()
            );
            let summary = corpus::analyze(repo, &env, &fixes, &cfg).await?;
            println!("{}", serde_json::to_string(&summary)?);
            Ok(())
        }
    }
}

/// The LLM environment of a notes command: the configured provider with
/// the review pipeline's decorations, and git tools pinned to `sha`.
async fn llm_env<'a>(
    settings: &Settings,
    repo: &Path,
    prompts_dir: &'a Path,
    sha: &str,
) -> Result<WorkflowEnv<'a>> {
    let provider = crate::ai::create_provider_cached(&settings.ai, None).await?;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(
        settings.review.concurrency.max(1),
    ));
    let quota = Arc::new(crate::ai::quota::QuotaManager::new());
    let provider =
        crate::local_review::decorate_provider(provider, &settings.ai, &semaphore, &quota, &None);
    let mut tools = ToolBox::new(repo.to_path_buf(), Some(prompts_dir.to_path_buf()));
    tools.set_virtual_head(sha.to_string());
    Ok(WorkflowEnv {
        provider,
        tools: Arc::new(tools),
        base_dir: prompts_dir,
        context_tag: None,
    })
}

/// A set ID becomes a directory name: letters, digits, dots, dashes and
/// underscores only.
fn is_safe_set_id(set: &str) -> bool {
    !set.is_empty()
        && set != "."
        && set != ".."
        && set
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_shard() {
        assert_eq!(parse_shard("0/1"), Ok((0, 1)));
        assert_eq!(parse_shard("3/8"), Ok((3, 8)));
        assert!(parse_shard("8/8").is_err());
        assert!(parse_shard("1/0").is_err());
        assert!(parse_shard("x").is_err());
    }

    #[test]
    fn test_is_safe_set_id() {
        assert!(is_safe_set_id("6.18"));
        assert!(!is_safe_set_id(".."));
        assert!(!is_safe_set_id("a/b"));
    }
}
