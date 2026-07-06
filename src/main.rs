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

use clap::{Parser, Subcommand, ValueEnum};
use sashiko::db::Database;
use sashiko::events::{Event, ParsedArticle};
use sashiko::ingestor::Ingestor;
use sashiko::local_review::{
    ProgressEvent, ReviewOptions, WorkerOptions, print_worker_json, result_has_error,
    result_has_high_or_critical_findings, run_git_review, run_worker_from_stdin,
};
use sashiko::prompt_bundle;
use sashiko::reviewer::Reviewer;
use sashiko::settings::Settings;
use serde_json::Value;
use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};
use tokio::sync::{Semaphore, mpsc};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_SETTINGS: &str = include_str!("../docs/examples/Settings.example.toml");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Number of last messages to ingest
    #[arg(long)]
    download: Option<usize>,

    /// Enable tracking of configured mailing lists
    #[arg(long)]
    track: bool,

    /// Disable non-read-only API calls (web ui should still work)
    #[arg(long)]
    no_api: bool,

    /// Disable AI interactions (ingestion only)
    #[arg(long)]
    no_ai: bool,

    /// Port to listen on (overrides settings)
    #[arg(long)]
    port: Option<u16>,

    /// Enable debug logging (overrides settings)
    #[arg(long)]
    debug: bool,

    /// Allow non-localhost POST requests (unsafe)
    #[arg(long)]
    enable_unsafe_all_submit: bool,

    /// Debug feature: select which stages from 1-7 to run
    #[arg(long, hide = true, value_delimiter = ',')]
    stages: Option<Vec<u8>>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a user settings file
    Init {
        /// Write settings to this path (default: ~/.config/sashiko.toml)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Overwrite an existing settings file
        #[arg(long)]
        force: bool,

        /// Print the default settings template instead of writing it
        #[arg(long)]
        print: bool,

        /// Reinstall bundled prompt files
        #[arg(long)]
        prompts: bool,
    },

    /// Review a local commit or commit range without starting the daemon
    Review {
        /// Git commit or range, for example HEAD or HEAD~3..HEAD
        #[arg(default_value = "HEAD")]
        input: String,

        /// Baseline reference (default: parent of the first commit)
        #[arg(long)]
        baseline: Option<String>,

        /// Settings file (default: ./Settings.toml, then ~/.config/sashiko.toml)
        #[arg(long)]
        settings: Option<PathBuf>,

        /// Skip AI review and only validate patch extraction/application
        #[arg(long)]
        no_ai: bool,

        /// Custom prompt to append to the review task
        #[arg(long)]
        custom_prompt: Option<String>,

        /// AI provider override
        #[arg(long)]
        ai_provider: Option<String>,

        /// Prompt directory
        #[arg(long)]
        prompts: Option<PathBuf>,

        /// Output format
        #[arg(long, default_value = "text")]
        format: OutputFormat,

        /// When to use color
        #[arg(long, default_value = "auto")]
        color: ColorMode,

        /// Select which stages from 1-7 to run
        #[arg(long, hide = true, value_delimiter = ',')]
        stages: Option<Vec<u8>>,
    },

    /// Internal worker mode for JSON-over-stdio review execution
    #[command(hide = true)]
    Worker {
        /// Read patchset data from JSON via stdin
        #[arg(long)]
        json: bool,

        /// Git revision to use as baseline
        #[arg(long)]
        baseline: Option<String>,

        /// Path to the git repository. Overrides settings.
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Parent directory for creating worktrees
        #[arg(long)]
        worktree_dir: Option<PathBuf>,

        /// Prompt directory
        #[arg(long)]
        prompts: Option<PathBuf>,

        /// Review only this patch index
        #[arg(long)]
        review_patch_index: Option<i64>,

        /// Review this commit directly without applying patches
        #[arg(long)]
        review_commit: Option<String>,

        /// Skip AI review but still validate patch application
        #[arg(long)]
        no_ai: bool,

        /// Reuse an existing worktree path
        #[arg(long)]
        reuse_worktree: Option<PathBuf>,

        /// AI provider override
        #[arg(long)]
        ai_provider: Option<String>,

        /// Custom prompt to append to the review task
        #[arg(long)]
        custom_prompt: Option<String>,

        /// Select which stages from 1-7 to run
        #[arg(long, hide = true, value_delimiter = ',')]
        stages: Option<Vec<u8>>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

const PARSER_VERSION: i32 = 2;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command line arguments
    let cli = Cli::parse();

    // Load settings early to determine log level, but don't fail yet
    let settings_result = Settings::new();

    // Determine log level
    // 1. CLI --debug takes precedence (implies "info")
    // 2. Settings log_level
    // 3. Fallback to "warn" (if settings failed)
    let is_review = matches!(cli.command, Some(Commands::Review { .. }));
    let log_level = if cli.debug {
        "info"
    } else if is_review {
        "warn"
    } else {
        match &settings_result {
            Ok(s) => &s.log_level,
            Err(_) => "warn",
        }
    };

    // Initialize tracing with EnvFilter
    // RUST_LOG env var still overrides everything if present
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    // Determine formatting features independently
    let plain_logs = std::env::var("SASHIKO_LOG_PLAIN").is_ok();
    let use_ansi = std::env::var("NO_COLOR").is_err() && std::io::stdout().is_terminal();

    let builder = fmt()
        .with_env_filter(env_filter)
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

    if cli.debug {
        info!("Debug logging enabled");
    }

    if let Some(command) = &cli.command {
        match command {
            Commands::Init {
                path,
                force,
                print,
                prompts,
            } => {
                handle_init_command(path.clone(), *force, *print, *prompts)?;
                return Ok(());
            }
            Commands::Review {
                input,
                baseline,
                settings,
                no_ai,
                custom_prompt,
                ai_provider,
                prompts,
                format,
                color,
                stages,
            } => {
                return handle_review_command(
                    input.clone(),
                    baseline.clone(),
                    settings.clone(),
                    *no_ai,
                    custom_prompt.clone(),
                    ai_provider.clone(),
                    resolve_prompts_path(prompts.clone())?,
                    *format,
                    *color,
                    stages.clone(),
                )
                .await;
            }
            Commands::Worker {
                json: _,
                baseline,
                repo,
                worktree_dir,
                prompts,
                review_patch_index,
                review_commit,
                no_ai,
                reuse_worktree,
                ai_provider,
                custom_prompt,
                stages,
            } => {
                let result = run_worker_from_stdin(WorkerOptions {
                    settings_path: None,
                    baseline: baseline.clone(),
                    repo: repo.clone(),
                    worktree_dir: worktree_dir.clone(),
                    prompts: resolve_prompts_path(prompts.clone())?,
                    review_patch_index: *review_patch_index,
                    review_commit: review_commit.clone(),
                    no_ai: *no_ai,
                    reuse_worktree: reuse_worktree.clone(),
                    ai_provider: ai_provider.clone(),
                    custom_prompt: custom_prompt.clone(),
                    stages: stages.clone(),
                    scratch_clone: false,
                    current_tree: false,
                })
                .await
                .unwrap_or_else(|e| {
                    serde_json::json!({
                        "patchset_id": 0,
                        "error": e.to_string()
                    })
                });
                print_worker_json(&result).map_err(Box::<dyn std::error::Error>::from)?;
                return Ok(());
            }
        }
    }

    // Now handle settings result properly
    let mut settings = match settings_result {
        Ok(s) => {
            info!("Settings loaded successfully");
            s
        }
        Err(e) => {
            error!("Failed to load settings: {}", e);
            return Err(e.into());
        }
    };

    if cli.no_ai {
        settings.ai.no_ai = true;
        info!("AI interactions disabled via --no-ai flag");
    }

    if cli.no_api {
        settings.server.read_only = true;
        info!("API enabled in READ-ONLY mode via --no-api flag");
    }

    if let Some(port) = cli.port {
        settings.server.port = port;
        info!("Server port overridden via --port flag: {}", port);
    }

    if let Some(stages) = cli.stages {
        settings.review.stages = Some(stages.clone());
        info!("Selected stages via --stages flag: {:?}", stages);
    }

    // Initialize Database
    let db = Arc::new(Database::new(&settings.database).await?);
    db.migrate().await?;

    // Create internal task queues
    // raw_tx -> Parser -> parsed_tx -> DB Worker
    let (raw_tx, mut raw_rx) = mpsc::channel::<Event>(1000);
    let (parsed_tx, mut parsed_rx) = mpsc::channel::<ParsedArticle>(1000);

    // Initialize FetchAgent
    let repo_path = std::path::PathBuf::from(&settings.git.repository_path);
    let (fetch_agent, fetch_tx) = sashiko::fetcher::FetchAgent::new(
        repo_path,
        raw_tx.clone(),
        settings.forge.api_token.clone(),
    );

    // Spawn FetchAgent
    tokio::spawn(async move {
        fetch_agent.run().await;
    });

    // Parser Dispatcher
    let semaphore = Arc::new(Semaphore::new(50));

    // Determine ingestion cutoff timestamp
    // If --download is passed, we accept everything (cutoff = None).
    // If --download is NOT passed:
    //    - If DB has messages, cutoff = oldest message timestamp.
    //    - If DB is empty, cutoff = current time (start time).
    let cutoff_timestamp = if cli.download.is_some() {
        None
    } else {
        match db.get_oldest_message_timestamp().await {
            Ok(Some(ts)) => {
                info!("Ingestion cutoff set to oldest message in DB: {}", ts);
                Some(ts)
            }
            Ok(None) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                info!("DB empty, ingestion cutoff set to start time: {}", now);
                Some(now)
            }
            Err(e) => {
                error!("Failed to get oldest message timestamp: {}", e);
                // Fallback to safe default (current time).
                // Let's assume now to be safe and avoid flooding.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                Some(now)
            }
        }
    };

    let parser_handle = tokio::spawn(async move {
        info!("Parser Dispatcher started");
        while let Some(event) = raw_rx.recv().await {
            let permit = match semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(e) => {
                    error!("Semaphore error: {}", e);
                    break;
                }
            };
            let tx = parsed_tx.clone();
            tokio::spawn(async move {
                let _permit = permit; // Hold permit until task completion

                match event {
                    Event::IngestionFailed { article_id, error } => {
                        if let Err(e) = tx
                            .send(ParsedArticle {
                                group: "error".to_string(),
                                article_id,
                                metadata: None,
                                patch: None,
                                baseline: None,
                                failed_error: Some(error),
                                skip_filters: None,
                                only_filters: None,
                                mr_url: None,
                                mr_title: None,
                                mr_number: None,
                            })
                            .await
                        {
                            error!("Failed to forward IngestionFailed event: {}", e);
                        }
                    }
                    Event::PatchSubmitted {
                        group,
                        article_id,
                        message_id,
                        subject,
                        author,
                        message,
                        diff,
                        base_commit,
                        timestamp,
                        index,
                        total,
                        mr_url,
                        mr_title,
                        mr_number,
                    } => {
                        let root_msg_id = format!("{}@sashiko.local", article_id);

                        // For single patches, we don't want a synthetic parent (the patch is the root)
                        let in_reply_to = if total == 1 {
                            None
                        } else {
                            Some(root_msg_id.clone())
                        };

                        // Pre-parsed patch handling
                        let metadata = sashiko::patch::PatchsetMetadata {
                            message_id: message_id.clone(),
                            subject,
                            author,
                            date: timestamp,
                            received_date: None,
                            in_reply_to,
                            references: vec![root_msg_id.clone()],
                            index,
                            total,
                            to: "submitted".to_string(),
                            cc: "".to_string(),
                            is_patch_or_cover: true,
                            version: None,
                            body: message.clone(),
                        };

                        let patch = Some(sashiko::patch::Patch {
                            message_id,
                            body: message,
                            diff,
                            part_index: index,
                        });

                        if let Err(e) = tx
                            .send(ParsedArticle {
                                group,
                                article_id,
                                metadata: Some(metadata),
                                patch,
                                baseline: base_commit,
                                failed_error: None,
                                skip_filters: None,
                                only_filters: None,
                                mr_url,
                                mr_title,
                                mr_number,
                            })
                            .await
                        {
                            error!("Failed to send pre-parsed article: {}", e);
                        }
                    }
                    Event::RawMboxSubmitted {
                        raw,
                        group,
                        baseline,
                        skip_subjects,
                        only_subjects,
                    } => {
                        let messages = sashiko::ingestor::split_mbox(raw.as_bytes());
                        let count = messages.len();

                        if count > 100 {
                            error!(
                                "Too many messages in mbox submission: {} (limit 100)",
                                count
                            );
                            return;
                        }

                        info!("Processing {} messages from raw mbox submission", count);

                        for msg_raw in messages {
                            let msg_id = sashiko::ingestor::extract_message_id(&msg_raw);
                            let group_clone = group.clone();
                            let tx_clone = tx.clone();
                            let baseline_clone = baseline.clone();
                            let skip_subjects_clone = skip_subjects.clone();
                            let only_subjects_clone = only_subjects.clone();

                            // Offload parsing
                            let parse_result = tokio::task::spawn_blocking(move || {
                                sashiko::patch::parse_email(&msg_raw)
                            })
                            .await;

                            match parse_result {
                                Ok(Ok((metadata, patch_opt))) => {
                                    // Override group "api-submit" -> "manual" to avoid synthetic ID logic
                                    let effective_group = if group_clone == "api-submit" {
                                        "manual".to_string()
                                    } else {
                                        group_clone
                                    };

                                    if let Err(e) = tx_clone
                                        .send(ParsedArticle {
                                            group: effective_group,
                                            article_id: msg_id,
                                            metadata: Some(metadata),
                                            patch: patch_opt,
                                            baseline: baseline_clone,
                                            failed_error: None,
                                            skip_filters: skip_subjects_clone,
                                            only_filters: only_subjects_clone,
                                            mr_url: None,
                                            mr_title: None,
                                            mr_number: None,
                                        })
                                        .await
                                    {
                                        error!("Failed to send parsed article: {}", e);
                                    }
                                }
                                Ok(Err(e)) => {
                                    info!("Parse error for {}: {}", msg_id, e);
                                }
                                Err(e) => {
                                    error!("Join error in parser: {}", e);
                                }
                            }
                        }
                    }
                    Event::ArticleFetched {
                        group,
                        article_id,
                        content,
                        raw,
                        baseline,
                    } => {
                        // Standard raw parsing logic
                        let bytes = match raw {
                            Some(b) => b,
                            None => content.join("\n").into_bytes(),
                        };

                        // Offload CPU parsing to blocking thread pool
                        let parse_result = tokio::task::spawn_blocking(move || {
                            sashiko::patch::parse_email(&bytes)
                        })
                        .await;

                        match parse_result {
                            Ok(Ok((metadata, patch_opt))) => {
                                // Check cutoff
                                if let Some(cutoff) = cutoff_timestamp
                                    && metadata.date < cutoff
                                {
                                    // info!("Skipping fetched article {} (date {} < cutoff {})", article_id, metadata.date, cutoff);
                                    return;
                                }

                                if let Err(e) = tx
                                    .send(ParsedArticle {
                                        group,
                                        article_id,
                                        metadata: Some(metadata),
                                        patch: patch_opt,
                                        baseline,
                                        failed_error: None,
                                        skip_filters: None,
                                        only_filters: None,
                                        mr_url: None,
                                        mr_title: None,
                                        mr_number: None,
                                    })
                                    .await
                                {
                                    error!("Failed to send parsed article: {}", e);
                                }
                            }
                            Ok(Err(e)) => {
                                info!("Parse error for {}: {}", article_id, e);
                            }
                            Err(e) => {
                                error!("Join error in parser: {}", e);
                            }
                        }
                    }
                }
            });
        }
        info!("Parser Dispatcher finished");
    });

    // DB Worker (Transactional Batching)
    let worker_db = db.clone();
    let mapping = settings.subsystems.mapping.clone();
    let _db_worker_handle = tokio::spawn(async move {
        info!("DB Worker started");

        let mut buffer = Vec::with_capacity(100);
        let mut total_processed = 0;
        let mut total_ingested = 0;
        let mut total_errors = 0;

        let policy =
            sashiko::email_policy::EmailPolicyConfig::load("email_policy.toml").unwrap_or_default();

        loop {
            let count = parsed_rx.recv_many(&mut buffer, 100).await;
            if count == 0 {
                break;
            }

            for article in buffer.drain(..) {
                match process_parsed_article(&worker_db, article, &policy, &mapping).await {
                    ProcessStatus::Ingested => total_ingested += 1,
                    ProcessStatus::Error => total_errors += 1,
                }
                total_processed += 1;

                if total_processed % 500 == 0 {
                    info!(
                        "Ingestion Progress: {} processed ({} ingested, {} errors)",
                        total_processed, total_ingested, total_errors
                    );
                }
            }
        }

        // Final stats
        info!(
            "Ingestion Complete: {} processed ({} ingested, {} errors)",
            total_processed, total_ingested, total_errors
        );
    });

    // Start Ingestor (feeds raw_tx)
    let ingestor_handle = if !(settings.forge.enabled && settings.forge.disable_nntp) {
        let ingestor = Ingestor::new(
            settings.clone(),
            db.clone(),
            raw_tx.clone(),
            cli.download,
            cli.track,
        );
        tokio::spawn(async move {
            if let Err(e) = ingestor.run().await {
                error!("Ingestor fatal error: {}", e);
            }
        })
    } else {
        info!("Forge integration is enabled. Lore/NNTP ingestor is disabled.");
        tokio::spawn(async move {
            std::future::pending::<()>().await;
        })
    };

    // Start Web API
    let api_settings = Arc::new(settings.clone());
    let api_db = db.clone();
    let api_tx = raw_tx.clone();
    let api_fetch_tx = fetch_tx.clone();
    let allow_all_submit = cli.enable_unsafe_all_submit;
    let smtp_enabled = settings.smtp.is_some();
    let dry_run = settings.smtp.as_ref().map(|s| s.dry_run).unwrap_or(false);
    tokio::spawn(async move {
        if let Err(e) = sashiko::api::run_server(
            api_settings,
            api_db,
            api_tx,
            api_fetch_tx,
            allow_all_submit,
            smtp_enabled,
            dry_run,
        )
        .await
        {
            error!("Web API fatal error: {}", e);
        }
    });

    // Start Email Worker
    if let Some(smtp_settings) = settings.smtp.clone() {
        let email_worker = sashiko::worker::email::EmailWorker::new(db.clone(), smtp_settings);
        tokio::spawn(async move {
            email_worker.run().await;
        });
    }

    // Start Patchwork Worker (processes API check entries when they exist)
    {
        let pw_policy_path = settings.review.email_policy_path.clone();
        let pw_max_retries = settings.review.max_retries;
        let patchwork_worker = sashiko::worker::patchwork::PatchworkWorker::new(
            db.clone(),
            pw_policy_path,
            pw_max_retries,
        );
        tokio::spawn(async move {
            patchwork_worker.run().await;
        });
    }

    // Initialize custom remotes
    let repo_path = std::path::PathBuf::from(&settings.git.repository_path);

    // Clean up stale worktree directories on disk first
    let worktree_path = std::path::PathBuf::from(&settings.review.worktree_dir);
    if let Err(e) = sashiko::git_ops::cleanup_worktree_dir(&worktree_path).await {
        error!("Failed to clean up stale worktree directories: {}", e);
    }

    // Prune stale worktrees on startup to prevent "bad object" fetch failures
    if let Err(e) = sashiko::git_ops::prune_worktrees(&repo_path).await {
        error!("Failed to prune stale worktrees: {}", e);
    }

    // Ensure submodule config compatibility (unset core.worktree if set)
    if let Err(e) = sashiko::git_ops::ensure_submodule_config_compat(&repo_path).await {
        error!("Failed to ensure submodule config compatibility: {}", e);
    }

    if let Some(custom_remotes) = &settings.git.custom_remotes {
        for remote in custom_remotes {
            info!(
                "Ensuring custom remote {} -> {}",
                remote.name,
                sashiko::utils::redact_secret(&remote.url)
            );
            if let Err(e) =
                sashiko::git_ops::ensure_remote(&repo_path, &remote.name, &remote.url, false).await
            {
                error!("Failed to ensure custom remote {}: {}", remote.name, e);
            }
        }
    }

    // Start Git Sync Worker
    {
        let sync_worker = sashiko::worker::sync::GitSyncWorker::new(repo_path.clone());
        tokio::spawn(async move {
            sync_worker.run().await;
        });
    }

    // Start Reviewer Service
    let reviewer = Reviewer::new(db.clone(), settings.clone()).await;
    tokio::spawn(async move {
        reviewer.start().await;
    });

    let metrics_db = db.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(pending) = metrics_db.count_pending_patches().await {
                sashiko::metrics::set_pending_patches(pending);
            }
            if let Ok(reviewing) = metrics_db.count_reviewing_patches().await {
                sashiko::metrics::set_reviewing_patches(reviewing);
            }
            if let Ok(messages) = metrics_db.count_messages(None, None).await {
                sashiko::metrics::set_messages(messages);
            }
            if let Ok(patchsets) = metrics_db.count_patchsets(None, None).await {
                sashiko::metrics::set_patchsets(patchsets);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    });

    // Keep the main thread running
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    // Abort handles
    ingestor_handle.abort();
    parser_handle.abort();

    Ok(())
}

fn handle_init_command(
    path: Option<PathBuf>,
    force: bool,
    print: bool,
    reinstall_prompts: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if print {
        print!("{}", DEFAULT_SETTINGS);
        return Ok(());
    }

    let path = path.unwrap_or_else(Settings::user_config_path);
    if path.exists() && !force {
        if reinstall_prompts {
            let prompts_root = prompt_bundle::install_prompt_bundle(true)?;
            println!("Installed prompts in {}", prompts_root.display());
            return Ok(());
        }
        return Err(format!(
            "{} already exists; use --force to overwrite it",
            path.display()
        )
        .into());
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&path, DEFAULT_SETTINGS)?;
    println!("Wrote {}", path.display());

    let prompts_root = prompt_bundle::install_prompt_bundle(reinstall_prompts)?;
    println!("Installed prompts in {}", prompts_root.display());
    Ok(())
}

fn resolve_prompts_path(path: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = path {
        return Ok(path);
    }

    Ok(prompt_bundle::default_kernel_prompts_path()?)
}

#[derive(Debug, Clone, PartialEq)]
enum PatchStatus {
    Queued,
    PreScreening,
    Planning,
    Reviewing,
    Finished,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PatchState {
    index: i64,
    subject: String,
    status: PatchStatus,
    planned_stages: Vec<u8>,
    active_stages: std::collections::BTreeSet<u8>,
    completed_stages: usize,
    active_stage_turns: std::collections::HashMap<u8, usize>,
}

fn get_terminal_width() -> usize {
    if let Ok(output) = std::process::Command::new("stty").arg("size").output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = stdout.split_whitespace().collect();
        if let Some(cols) = parts
            .get(1)
            .filter(|_| parts.len() == 2)
            .and_then(|s| s.parse::<usize>().ok())
        {
            return cols;
        }
    }

    if let Ok(cols) = std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or(())
    {
        return cols;
    }

    80
}

struct ProgressState {
    patches: std::collections::BTreeMap<i64, PatchState>,
    printed_lines: usize,
    total_turns: usize,
    terminal_width: usize,
    color_choice: ColorChoice,
}

fn stage_short_name(stage: u8) -> &'static str {
    match stage {
        1 => "Goal Analysis",
        2 => "Implementation",
        3 => "Execution Flow",
        4 => "Resource Mgmt",
        5 => "Locking & Sync",
        6 => "Security Audit",
        7 => "Hardware Review",
        8 => "Deduplication",
        9 => "Conflict Resolution",
        10 => "Severity Estimation",
        11 => "Report Generation",
        _ => "Unknown",
    }
}

struct TruncatingWriter {
    limit: usize,
    written: usize,
    color_choice: ColorChoice,
}

impl TruncatingWriter {
    fn new(limit: usize, color_choice: ColorChoice) -> Self {
        Self {
            limit,
            written: 0,
            color_choice,
        }
    }

    fn write_segment(
        &mut self,
        text: &str,
        color: Option<Color>,
        bold: bool,
    ) -> std::io::Result<()> {
        if self.written >= self.limit {
            return Ok(());
        }

        let remaining = self.limit - self.written;
        let (to_write, suffix) = if text.chars().count() > remaining {
            let taken: String = text.chars().take(remaining.saturating_sub(3)).collect();
            (taken, "...")
        } else {
            (text.to_string(), "")
        };

        let mut stderr = StandardStream::stderr(self.color_choice);
        let mut spec = ColorSpec::new();
        if let Some(c) = color {
            spec.set_fg(Some(c));
        }
        if bold {
            spec.set_bold(true);
        }
        stderr.set_color(&spec)?;
        write!(&mut stderr, "{}", to_write)?;

        self.written += to_write.chars().count();

        if !suffix.is_empty() {
            stderr.reset()?;
            write!(&mut stderr, "{}", suffix)?;
            self.written += 3;
        }

        stderr.reset()
    }
}

fn render_progress(state: &mut ProgressState) {
    if state.printed_lines > 0 {
        for _ in 0..state.printed_lines {
            eprint!("\x1b[F\x1b[2K");
        }
        let _ = std::io::stderr().flush();
    }

    let mut lines_printed = 0;
    let limit = state.terminal_width.saturating_sub(5);

    for (&idx, p) in &state.patches {
        let status_str = match &p.status {
            PatchStatus::Queued => "Queued".to_string(),
            PatchStatus::PreScreening => "Pre-screening guides...".to_string(),
            PatchStatus::Planning => "Planning stages...".to_string(),
            PatchStatus::Reviewing => {
                if p.active_stages.is_empty() {
                    "Reviewing...".to_string()
                } else {
                    let mut stages_with_turns: Vec<(u8, usize)> = p
                        .active_stages
                        .iter()
                        .map(|&st| {
                            let turn = p.active_stage_turns.get(&st).cloned().unwrap_or(0);
                            (st, turn)
                        })
                        .collect();
                    stages_with_turns.sort_by(|a, b| b.1.cmp(&a.1));

                    let (top_stage, top_turn) = stages_with_turns[0];
                    let stage_name = stage_short_name(top_stage);
                    let stage_str = if top_turn > 0 {
                        format!("{} (turn {})", stage_name, top_turn)
                    } else {
                        stage_name.to_string()
                    };

                    if p.active_stages.len() > 1 {
                        format!("{} (+{} stages)", stage_str, p.active_stages.len() - 1)
                    } else {
                        stage_str
                    }
                }
            }
            PatchStatus::Finished => "Finished".to_string(),
        };

        // Calculate available width for subject to guarantee status is never truncated
        let fixed_overhead = 16 + 3; // "      [Patch X] " + " | "
        let status_len = status_str.chars().count();
        let available_for_subject = limit
            .saturating_sub(fixed_overhead)
            .saturating_sub(status_len);

        let target_subject_width = 30;
        let subject_width = std::cmp::min(target_subject_width, available_for_subject);

        let mut subject_padded = if p.subject.chars().count() > subject_width {
            if subject_width > 3 {
                let taken: String = p.subject.chars().take(subject_width - 3).collect();
                format!("{}...", taken.trim_end())
            } else {
                "...".to_string()
            }
        } else {
            p.subject.clone()
        };

        let padding_chars = subject_width.saturating_sub(subject_padded.chars().count());
        if padding_chars > 0 {
            subject_padded.push_str(&" ".repeat(padding_chars));
        }

        let mut tw = TruncatingWriter::new(limit, state.color_choice);
        let _ = tw.write_segment(&format!("      [Patch {}] ", idx), None, false);
        let _ = tw.write_segment(&subject_padded, None, false);
        let _ = tw.write_segment(" | ", None, false);

        let (status_color, status_bold) = match &p.status {
            PatchStatus::Queued => (None, false),
            PatchStatus::PreScreening | PatchStatus::Planning => (Some(Color::Cyan), false),
            PatchStatus::Reviewing => (Some(Color::Cyan), true),
            PatchStatus::Finished => (Some(Color::Green), true),
        };
        let _ = tw.write_segment(&status_str, status_color, status_bold);

        eprintln!();
        lines_printed += 1;
    }

    let total_patches = state.patches.len();
    if total_patches > 0 {
        let total_stages: usize = state
            .patches
            .values()
            .map(|p| {
                if p.planned_stages.is_empty() {
                    11
                } else {
                    p.planned_stages.len()
                }
            })
            .sum();
        let completed_stages: usize = state.patches.values().map(|p| p.completed_stages).sum();
        let percent = if total_stages > 0 {
            (completed_stages * 100) / total_stages
        } else {
            0
        };
        let width = 20;
        let filled = if total_stages > 0 {
            (completed_stages * width) / total_stages
        } else {
            0
        };

        let mut tw = TruncatingWriter::new(limit, state.color_choice);
        let _ = tw.write_segment("Overall: [", None, true);

        let filled_bar = "█".repeat(filled);
        let _ = tw.write_segment(&filled_bar, Some(Color::Green), false);

        let empty_bar = "░".repeat(width - filled);
        let _ = tw.write_segment(&empty_bar, None, false);

        let _ = tw.write_segment("] ", None, true);

        let stats = format!(
            "{}% | {}/{} stages | {} turns",
            percent, completed_stages, total_stages, state.total_turns
        );
        let _ = tw.write_segment(&stats, None, false);

        eprintln!();
        lines_printed += 1;
    }

    state.printed_lines = lines_printed;
    let _ = std::io::stderr().flush();
}

#[allow(clippy::too_many_arguments)]
async fn handle_review_command(
    input: String,
    baseline: Option<String>,
    settings_path: Option<PathBuf>,
    no_ai: bool,
    custom_prompt: Option<String>,
    ai_provider: Option<String>,
    prompts: PathBuf,
    format: OutputFormat,
    color: ColorMode,
    stages: Option<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let color_choice = match color {
        ColorMode::Always => ColorChoice::Always,
        ColorMode::Never => ColorChoice::Never,
        ColorMode::Auto => {
            if std::io::stdout().is_terminal() {
                ColorChoice::Auto
            } else {
                ColorChoice::Never
            }
        }
    };

    let repo_path = current_git_toplevel()?;
    eprintln!("Reviewing: {}", input);
    eprintln!("Using prompts: {}", prompts.display());

    if sashiko::git_ops::is_dirty(&repo_path)
        .await
        .unwrap_or(false)
    {
        eprint_colored(color_choice, Color::Yellow, "WARNING:")?;
        eprintln!(
            " Working directory is dirty. The AI reviewer might see uncommitted changes when analyzing files."
        );
        if std::io::stdin().is_terminal() {
            print!("Do you want to proceed? [y/N]: ");
            std::io::stdout().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let trimmed = input.trim().to_lowercase();
            if trimmed != "y" && trimmed != "yes" {
                eprintln!("Aborted.");
                return Ok(());
            }
        }
    }

    let progress_state = std::sync::Arc::new(std::sync::Mutex::new(ProgressState {
        patches: std::collections::BTreeMap::new(),
        printed_lines: 0,
        total_turns: 0,
        terminal_width: get_terminal_width(),
        color_choice,
    }));

    let progress_state_clone = progress_state.clone();
    let progress = move |event: ProgressEvent| {
        let mut s = progress_state_clone.lock().unwrap();
        match event {
            ProgressEvent::ResolvingInput { .. } => {}
            ProgressEvent::ResolvedCommits { commits } => {
                for commit in commits {
                    s.patches.insert(
                        commit.index,
                        PatchState {
                            index: commit.index,
                            subject: commit.subject.clone(),
                            status: PatchStatus::Queued,
                            planned_stages: Vec::new(),
                            active_stages: std::collections::BTreeSet::new(),
                            completed_stages: 0,
                            active_stage_turns: std::collections::HashMap::new(),
                        },
                    );
                }
            }
            ProgressEvent::BaselineResolved { rev, sha } => {
                eprintln!("Baseline: {} ({})", rev, sha);
            }
            ProgressEvent::CurrentTreeReady { .. } => {}
            ProgressEvent::WorktreeCreated { path } => {
                eprintln!("Created temporary worktree at {}", path.display());
            }
            ProgressEvent::ApplyingPatch {
                index,
                total,
                subject,
            } => {
                eprintln!("   Applying patch {}/{}: {}", index, total, subject);
            }
            ProgressEvent::PatchApplied { index } => {
                eprintln!("   Applied patch {}", index);
            }
            ProgressEvent::PatchFailed { index, error } => {
                eprintln!("   Failed to apply patch {}: {}", index, error);
            }
            ProgressEvent::AiReviewStarted { patches } => {
                eprintln!(
                    "Running review for {} patch{}",
                    patches,
                    if patches == 1 { "" } else { "es" }
                );
            }
            ProgressEvent::AiReviewPreScreenStarted { patch_index } => {
                if let Some(p) = s.patches.get_mut(&patch_index) {
                    p.status = PatchStatus::PreScreening;
                    render_progress(&mut s);
                }
            }
            ProgressEvent::AiReviewPlanningStarted { patch_index } => {
                if let Some(p) = s.patches.get_mut(&patch_index) {
                    p.status = PatchStatus::Planning;
                    render_progress(&mut s);
                }
            }
            ProgressEvent::AiReviewPlanReady {
                patch_index,
                planned_stages,
            } => {
                if let Some(p) = s.patches.get_mut(&patch_index) {
                    p.status = PatchStatus::Reviewing;
                    p.planned_stages = planned_stages;
                    render_progress(&mut s);
                }
            }
            ProgressEvent::AiReviewStageStarted { patch_index, stage } => {
                if let Some(p) = s.patches.get_mut(&patch_index) {
                    p.status = PatchStatus::Reviewing;
                    p.active_stages.insert(stage);
                    render_progress(&mut s);
                }
            }
            ProgressEvent::AiReviewStageTurn {
                patch_index,
                stage,
                turn,
                ..
            } => {
                if let Some(p) = s.patches.get_mut(&patch_index) {
                    p.active_stage_turns.insert(stage, turn);
                }
                s.total_turns += 1;
                render_progress(&mut s);
            }
            ProgressEvent::AiReviewStageFinished { patch_index, stage } => {
                if let Some(p) = s.patches.get_mut(&patch_index) {
                    p.active_stages.remove(&stage);
                    p.active_stage_turns.remove(&stage);
                    p.completed_stages += 1;
                    render_progress(&mut s);
                }
            }
            ProgressEvent::AiReviewAttempt {
                patch_index: _,
                attempt,
                max_attempts,
            } => {
                if attempt > 1 {
                    // Let's just log this since we don't want stdout/stderr output to disrupt the rewrite loop
                    info!("AI review retry (attempt {}/{})", attempt, max_attempts);
                }
            }
            ProgressEvent::AiReviewFinished { patch_index } => {
                if let Some(p) = s.patches.get_mut(&patch_index) {
                    p.status = PatchStatus::Finished;
                    render_progress(&mut s);
                }
            }
            ProgressEvent::ReviewComplete => {
                // Ensure overall review is finished and printed lines cleared or kept
                // Let's not call render_progress here, just print review complete
                eprintln!("Review complete");
            }
        }
    };

    let result = run_git_review(
        repo_path,
        input.clone(),
        ReviewOptions {
            baseline,
            settings_path,
            prompts,
            no_ai,
            ai_provider,
            custom_prompt,
            stages,
        },
        Some(&progress),
    )
    .await?;

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutputFormat::Text => {
            print_review_result(&result, &input, color_choice)?;
        }
    }

    if result_has_error(&result) {
        std::process::exit(3);
    }
    if result_has_high_or_critical_findings(&result) {
        std::process::exit(1);
    }

    Ok(())
}

fn current_git_toplevel() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let output = std::process::Command::new("git")
        .current_dir(&cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;

    if !output.status.success() {
        return Err(format!(
            "current directory is not inside a git repository: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }

    Ok(Path::new(String::from_utf8_lossy(&output.stdout).trim()).to_path_buf())
}

fn print_review_result(
    result: &Value,
    _input: &str,
    color_choice: ColorChoice,
) -> std::io::Result<()> {
    if let Some(error) = result.get("error").and_then(|v| v.as_str())
        && !error.is_empty()
    {
        println!();
        print_colored(color_choice, Color::Red, "Error: ")?;
        println!("{}", error);
        return Ok(());
    }

    let Some(review) = result.get("review") else {
        return Ok(());
    };
    let Some(findings) = review.get("findings").and_then(|v| v.as_array()) else {
        print_colored(color_choice, Color::Green, "\nNo AI review was run.\n")?;
        return Ok(());
    };

    let counts = count_findings(findings);
    let total = counts.critical + counts.high + counts.medium + counts.low;
    if total == 0 {
        print_colored(color_choice, Color::Green, "\nNo issues found.\n")?;
    } else {
        println!("\nFindings:");
        print!("  Critical: ");
        print_colored(color_choice, Color::Red, &counts.critical.to_string())?;
        print!("  High: ");
        print_colored(color_choice, Color::Red, &counts.high.to_string())?;
        print!("  Medium: ");
        print_colored(color_choice, Color::Yellow, &counts.medium.to_string())?;
        print!("  Low: ");
        print_colored(color_choice, Color::Cyan, &counts.low.to_string())?;
        println!("\n");

        let mut grouped_findings: std::collections::BTreeMap<i64, Vec<&Value>> =
            std::collections::BTreeMap::new();
        let mut ungrouped_findings = Vec::new();

        for finding in findings {
            if let Some(p_idx) = finding.get("patch_index").and_then(|v| v.as_i64()) {
                grouped_findings.entry(p_idx).or_default().push(finding);
            } else {
                ungrouped_findings.push(finding);
            }
        }

        for (p_idx, patch_findings) in grouped_findings {
            let subject = patch_findings
                .first()
                .and_then(|f| f.get("patch_subject"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            print!("  --- Patch [{}] ", p_idx);
            if !subject.is_empty() {
                print_colored(color_choice, Color::Cyan, subject)?;
            }
            println!(" ---");
            for finding in patch_findings {
                print_finding(finding, color_choice)?;
            }
            println!();
        }

        if !ungrouped_findings.is_empty() {
            println!("  --- General Findings ---");
            for finding in ungrouped_findings {
                print_finding(finding, color_choice)?;
            }
        }
    }

    if let Some(inline) = result.get("inline_review").and_then(|v| v.as_str())
        && !inline.trim().is_empty()
        && inline.trim() != "No issues found."
    {
        println!("\nInline Review:");
        for line in inline.lines() {
            if line.starts_with("diff ") || line.starts_with("+++") || line.starts_with("---") {
                println!("{}", line);
            } else if line.starts_with('+') {
                print_colored(color_choice, Color::Green, line)?;
                println!();
            } else if line.starts_with('-') {
                print_colored(color_choice, Color::Red, line)?;
                println!();
            } else if line.starts_with("@@") {
                print_colored(color_choice, Color::Cyan, line)?;
                println!();
            } else {
                println!("{}", line);
            }
        }
    }

    let tokens_in = result
        .get("tokens_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let tokens_out = result
        .get("tokens_out")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let tokens_cached = result
        .get("tokens_cached")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if tokens_in > 0 || tokens_out > 0 || tokens_cached > 0 {
        println!(
            "\nTokens: {} in / {} out / {} cached",
            tokens_in, tokens_out, tokens_cached
        );
    }

    Ok(())
}

fn print_finding(finding: &Value, color_choice: ColorChoice) -> std::io::Result<()> {
    let severity = finding
        .get("severity")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let color = match severity.to_ascii_lowercase().as_str() {
        "critical" | "high" => Color::Red,
        "medium" => Color::Yellow,
        "low" => Color::Cyan,
        _ => Color::White,
    };
    let problem = finding
        .get("problem")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    print!("  ");
    print_colored(color_choice, color, &format!("[{}] ", severity))?;
    println!("{}", problem);
    Ok(())
}

#[derive(Default)]
struct FindingCounts {
    critical: usize,
    high: usize,
    medium: usize,
    low: usize,
}

fn count_findings(findings: &[Value]) -> FindingCounts {
    let mut counts = FindingCounts::default();
    for finding in findings {
        if finding
            .get("preexisting")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        match finding
            .get("severity")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "critical" => counts.critical += 1,
            "high" => counts.high += 1,
            "medium" => counts.medium += 1,
            "low" => counts.low += 1,
            _ => {}
        }
    }
    counts
}

fn print_colored(color_choice: ColorChoice, color: Color, text: &str) -> std::io::Result<()> {
    let mut stdout = StandardStream::stdout(color_choice);
    stdout.set_color(ColorSpec::new().set_fg(Some(color)))?;
    write!(&mut stdout, "{}", text)?;
    stdout.reset()
}

fn eprint_colored(color_choice: ColorChoice, color: Color, text: &str) -> std::io::Result<()> {
    let mut stderr = StandardStream::stderr(color_choice);
    stderr.set_color(ColorSpec::new().set_fg(Some(color)))?;
    write!(&mut stderr, "{}", text)?;
    stderr.reset()
}

enum ProcessStatus {
    Ingested,
    Error,
}

async fn process_parsed_article(
    worker_db: &Database,
    article: ParsedArticle,
    policy: &sashiko::email_policy::EmailPolicyConfig,
    subsystem_mapping: &[sashiko::settings::SubsystemMapping],
) -> ProcessStatus {
    let ParsedArticle {
        group,
        article_id,
        metadata,
        patch,
        baseline,
        failed_error,
        skip_filters,
        only_filters,
        mr_url,
        mr_title,
        mr_number,
    } = article;

    // Handle ingestion failure
    if let Some(err) = failed_error {
        info!("Handling ingestion failure for {}: {}", article_id, err);
        if let Err(e) = worker_db.update_patchset_error(&article_id, &err).await {
            error!("Failed to update patchset error in DB: {}", e);
        }
        return ProcessStatus::Ingested; // Successfully handled the failure event
    }

    let mut metadata = match metadata {
        Some(m) => m,
        None => {
            error!(
                "Missing metadata for article {} (group: {})",
                article_id, group
            );
            return ProcessStatus::Error;
        }
    };

    let mut patch_opt = patch;

    let author_email = sashiko::patch::extract_email(&metadata.author);

    if sashiko::email_router::EmailRouter::is_ignored_author(policy, &author_email) {
        if metadata.is_patch_or_cover {
            info!(
                "Ignoring patch/cover from {} according to email policy",
                author_email
            );
        }
        metadata.is_patch_or_cover = false;
        patch_opt = None;
    }

    // Resolve baseline ID if provided
    let baseline_id = if let Some(b) = baseline {
        match worker_db.create_baseline(None, None, Some(&b)).await {
            Ok(id) => Some(id),
            Err(e) => {
                error!("Failed to create baseline for {}: {}", b, e);
                None
            }
        }
    } else {
        None
    };

    // 1. Thread Resolution
    let (thread_id, is_git_import, git_import_total) =
        if let Some(rest) = group.strip_prefix("git-import:") {
            // format is "count:range"
            let parts: Vec<&str> = rest.splitn(2, ':').collect();
            let (total_count, range) = if parts.len() == 2 {
                (parts[0].parse::<u32>().unwrap_or(0), parts[1])
            } else {
                (0, rest)
            };

            let safe_range = range.replace(['/', ':', ' ', '.'], "_");
            let root_msg_id = format!("git-import-{}@sashiko.local", safe_range);
            match worker_db
                .ensure_thread_for_message(&root_msg_id, metadata.date)
                .await
            {
                Ok(tid) => (tid, true, total_count),
                Err(e) => {
                    error!("Failed to ensure thread for git import {}: {}", range, e);
                    return ProcessStatus::Error;
                }
            }
        } else if group == "git-fetch" || group == "api-submit" {
            // Group these by article_id (which is the range or single SHA/local_id)
            // For singletons, the message itself is the root.
            let root_msg_id = if metadata.total == 1 {
                metadata.message_id.clone()
            } else {
                format!("{}@sashiko.local", article_id)
            };

            match worker_db
                .ensure_thread_for_message(&root_msg_id, metadata.date)
                .await
            {
                Ok(tid) => (tid, false, 0),
                Err(e) => {
                    error!("Failed to ensure thread for group {}: {}", group, e);
                    return ProcessStatus::Error;
                }
            }
        } else if let Some(ref reply_to) = metadata.in_reply_to {
            match worker_db
                .ensure_thread_for_message(reply_to, metadata.date)
                .await
            {
                Ok(tid) => (tid, false, 0),
                Err(e) => {
                    error!("Failed to ensure thread for parent {}: {}", reply_to, e);
                    return ProcessStatus::Error;
                }
            }
        } else {
            match worker_db
                .ensure_thread_for_message(&metadata.message_id, metadata.date)
                .await
            {
                Ok(tid) => (tid, false, 0),
                Err(e) => {
                    error!(
                        "Failed to ensure thread for self {}: {}",
                        metadata.message_id, e
                    );
                    return ProcessStatus::Error;
                }
            }
        };

    let is_git_hash = article_id.len() == 40 && article_id.chars().all(|c| c.is_ascii_hexdigit());
    // Only optimize storage (skip body) if it's a bulk git import where we have the archives
    let (body_to_store, git_hash_opt) = if is_git_hash && group.starts_with("git-import") {
        ("", Some(article_id.as_str()))
    } else {
        (metadata.body.as_str(), None)
    };

    let refs_hdr = if metadata.references.is_empty() {
        None
    } else {
        let cleaned_refs: Vec<String> = metadata
            .references
            .iter()
            .map(|r| r.trim_matches(|c| c == '<' || c == '>').to_string())
            .collect();
        Some(cleaned_refs.join(" "))
    };

    // 2. Create Message
    if let Err(e) = worker_db
        .create_message_with_references(
            &metadata.message_id,
            thread_id,
            metadata.in_reply_to.as_deref(),
            &metadata.author,
            &metadata.subject,
            metadata.date,
            body_to_store,
            &metadata.to,
            &metadata.cc,
            git_hash_opt,
            Some(&group),
            refs_hdr.as_deref(),
        )
        .await
    {
        error!("Failed to create message: {}", e);
        return ProcessStatus::Error;
    }

    // Subsystem Identification and Linking
    let mut subsystems = identify_subsystems(&metadata.to, &metadata.cc, subsystem_mapping);

    if let Some(p) = patch_opt.as_ref() {
        let files = sashiko::baseline::extract_files_from_diff(&p.diff);
        let path_subsystems = identify_subsystems_from_paths(&files, subsystem_mapping);
        subsystems.extend(path_subsystems);
    }

    if group.starts_with("git-import") || group == "git-fetch" {
        let (label, email) = if let Some(url) = &mr_url {
            if let Some(repo_name) = sashiko::forge::extract_repo_name_from_mr_url(url) {
                let email = format!("git-import-{}", repo_name);
                (repo_name, email)
            } else {
                ("from git".to_string(), "git-import".to_string())
            }
        } else {
            ("from git".to_string(), "git-import".to_string())
        };
        subsystems.push((label, email));
    }
    subsystems.sort();
    subsystems.dedup();

    let mut subsystem_ids = Vec::new();
    for (name, email) in &subsystems {
        match worker_db.ensure_subsystem(name, email).await {
            Ok(sid) => subsystem_ids.push(sid),
            Err(e) => error!("Failed to ensure subsystem {}: {}", name, e),
        }
    }

    if let Ok(Some(msg_id_db)) = worker_db
        .get_message_id_by_msg_id(&metadata.message_id)
        .await
    {
        // Link to Mailing List
        match worker_db.get_mailing_list_id_by_name(&group).await {
            Ok(Some(list_id)) => {
                if let Err(e) = worker_db
                    .add_message_to_mailing_list(msg_id_db, list_id)
                    .await
                {
                    error!(
                        "Failed to link message {} to list {}: {}",
                        metadata.message_id, group, e
                    );
                } else {
                    // info!("Linked message {} to list {}", metadata.message_id, group);
                }
            }
            Ok(None) => {
                if group != "git-fetch" && group != "manual" {
                    warn!("Mailing list not found for group: {}", group);
                }
            }
            Err(e) => {
                error!("Failed to resolve mailing list for group {}: {}", group, e);
            }
        }

        // Link Subsystems
        for &sid in &subsystem_ids {
            if let Err(e) = worker_db.add_subsystem_to_message(msg_id_db, sid).await {
                error!("Failed to link message to subsystem: {}", e);
            }
            if let Err(e) = worker_db.add_subsystem_to_thread(thread_id, sid).await {
                error!("Failed to link thread to subsystem: {}", e);
            }
        }

        // Link Recipients
        process_recipients(worker_db, msg_id_db, &metadata.to, "To").await;
        process_recipients(worker_db, msg_id_db, &metadata.cc, "Cc").await;
    }

    // Removed baseline detection from ingestion as it's now part of review process

    // Removed per-article info log
    /*
    let subject = if metadata.subject.len() > 80 {
        format!("{}...", &metadata.subject[..77])
    } else {
        metadata.subject.clone()
    };
    info!(
        "Article: group={}, id={}, author={}, subject=\"{}\"",
        group, article_id, metadata.author, subject
    );
    */

    let root_msg_id = format!("{}@sashiko.local", article_id);
    let cover_letter_id = if group == "git-fetch" {
        // Always use root_msg_id for git-fetch to match the placeholder ID
        Some(root_msg_id.as_str())
    } else if group == "api-submit" {
        if metadata.total == 1 {
            Some(metadata.message_id.as_str())
        } else {
            Some(root_msg_id.as_str())
        }
    } else if metadata.index == 0 || metadata.total == 1 {
        Some(metadata.message_id.as_str())
    } else {
        metadata.in_reply_to.as_deref()
    };

    if metadata.is_patch_or_cover {
        let (subject, author, total_parts, strict_author) = if is_git_import {
            let range = group
                .strip_prefix("git-import:")
                .and_then(|s| s.split_once(':').map(|(_, r)| r))
                .unwrap_or("unknown");
            (
                format!("Git Import: {}", range),
                "Sashiko Git Import".to_string(),
                if git_import_total > 0 {
                    git_import_total
                } else {
                    metadata.total
                },
                false,
            )
        } else if group == "git-fetch"
            && let (Some(title), Some(number)) = (&mr_title, &mr_number)
        {
            if metadata.total == 1 || metadata.index == 1 {
                (
                    format!("!{}: {}", number, title),
                    metadata.author.clone(),
                    metadata.total,
                    true,
                )
            } else {
                (
                    metadata.subject.clone(),
                    metadata.author.clone(),
                    metadata.total,
                    !group.starts_with("git-import"),
                )
            }
        } else {
            (
                metadata.subject.clone(),
                metadata.author.clone(),
                metadata.total,
                !group.starts_with("git-import"),
            )
        };

        let max_embargo_hours = calculate_embargo_hours(&subject, &subsystems, policy);

        let embargo_until = if max_embargo_hours > 0 {
            let base_time = metadata.received_date.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            });
            Some(base_time + (max_embargo_hours as i64) * 3600)
        } else {
            None
        };

        match worker_db
            .create_patchset(
                thread_id,
                cover_letter_id,
                metadata.message_id.as_str(),
                &subject,
                &author,
                metadata.date,
                total_parts,
                PARSER_VERSION,
                &metadata.to,
                &metadata.cc,
                metadata.version,
                metadata.index,
                baseline_id,
                strict_author,
                skip_filters.as_ref(),
                only_filters.as_ref(),
            )
            .await
        {
            Ok(Some(patchset_id)) => {
                #[allow(clippy::collapsible_if)]
                if let Some(until) = embargo_until {
                    if let Err(e) = worker_db
                        .set_patchset_embargo_until(patchset_id, until)
                        .await
                    {
                        error!(
                            "Failed to set embargo_until for patchset {}: {}",
                            patchset_id, e
                        );
                    }
                }

                for &sid in &subsystem_ids {
                    if let Err(e) = worker_db.add_subsystem_to_patchset(patchset_id, sid).await {
                        error!("Failed to link patchset to subsystem: {}", e);
                    }
                }

                if let Some(patch) = patch_opt {
                    match worker_db
                        .create_patch(
                            patchset_id,
                            &patch.message_id,
                            patch.part_index,
                            &patch.diff,
                        )
                        .await
                    {
                        Ok(patch_id) => {
                            for &sid in &subsystem_ids {
                                if let Err(e) =
                                    worker_db.add_subsystem_to_patch(patch_id, sid).await
                                {
                                    error!("Failed to link patch to subsystem: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to save patch: {}", e);
                            return ProcessStatus::Error;
                        }
                    }
                }
                ProcessStatus::Ingested
            }
            Ok(None) => {
                // Skipped patchset creation (reply mismatch or duplicate)
                // BUT message was ingested successfully.
                ProcessStatus::Ingested
            }
            Err(e) => {
                error!("Failed to save patchset: {}", e);
                ProcessStatus::Error
            }
        }
    } else {
        // Skipped patchset creation/update for non-patch message
        // BUT message was ingested successfully.
        ProcessStatus::Ingested
    }
}

async fn process_recipients(
    db: &Database,
    message_id: i64,
    recipients: &str,
    recipient_type: &str,
) {
    for raw in recipients.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }

        let (name, email) = if let Some(start) = raw.find('<') {
            if let Some(end) = raw.find('>') {
                if end > start {
                    let name = raw[..start].trim();
                    let email = &raw[start + 1..end];
                    (
                        if name.is_empty() { None } else { Some(name) },
                        email.trim(),
                    )
                } else {
                    (None, raw)
                }
            } else {
                (None, raw)
            }
        } else {
            (None, raw)
        };

        if email.is_empty() {
            continue;
        }

        match db.ensure_person(name, email).await {
            Ok(person_id) => {
                if let Err(e) = db
                    .add_message_recipient(message_id, person_id, recipient_type)
                    .await
                {
                    // Ignore duplicates
                    if !e.to_string().contains("UNIQUE constraint failed") {
                        error!(
                            "Failed to add recipient {} to message {}: {}",
                            email, message_id, e
                        );
                    }
                }
            }
            Err(e) => {
                error!("Failed to ensure person {}: {}", email, e);
            }
        }
    }
}

fn extract_subject_prefixes(subject: &str) -> Vec<String> {
    let mut prefixes = Vec::new();
    let mut in_bracket = false;
    let mut current_block = String::new();

    for c in subject.chars() {
        if c == '[' {
            in_bracket = true;
            current_block.clear();
        } else if c == ']' {
            if in_bracket {
                let parts = current_block.split_whitespace();
                for part in parts {
                    let part_lower = part.to_lowercase();
                    if part_lower == "patch" || part_lower == "rfc" {
                        continue;
                    }
                    if part_lower.starts_with('v')
                        && part_lower[1..].chars().all(|c| c.is_ascii_digit())
                    {
                        continue;
                    }
                    if part_lower.contains('/')
                        && part_lower.chars().all(|c| c.is_ascii_digit() || c == '/')
                    {
                        continue;
                    }
                    if !part_lower.is_empty() {
                        prefixes.push(part_lower.to_string());
                    }
                }
            }
            in_bracket = false;
        } else if in_bracket {
            current_block.push(c);
        }
    }
    prefixes
}

// Helper function to map To/Cc to Subsystems
fn calculate_embargo_hours(
    subject: &str,
    subsystems: &[(String, String)],
    policy: &sashiko::email_policy::EmailPolicyConfig,
) -> u32 {
    let subject_prefixes = extract_subject_prefixes(subject);
    let mut matched_subsystem_policies = Vec::new();

    for (_, email) in subsystems {
        for sp in policy.subsystems.values() {
            #[allow(clippy::collapsible_if)]
            if sp.lists.iter().any(|list| email.contains(list)) {
                matched_subsystem_policies.push(sp);
            }
        }
    }

    let mut explicit_delays = Vec::new();
    let mut prefix_matched_delays = Vec::new();

    for sp in &matched_subsystem_policies {
        if let Some(delay) = sp.embargo_hours {
            explicit_delays.push(delay);

            if !sp.subject_prefixes.is_empty() {
                for prefix in &subject_prefixes {
                    if sp
                        .subject_prefixes
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case(prefix))
                    {
                        prefix_matched_delays.push(delay);
                        break;
                    }
                }
            }
        }
    }

    let delays_to_consider = if !prefix_matched_delays.is_empty() {
        prefix_matched_delays
    } else {
        explicit_delays
    };

    if !delays_to_consider.is_empty() {
        *delays_to_consider.iter().min().unwrap()
    } else {
        policy.defaults.embargo_hours.unwrap_or(0)
    }
}

fn identify_subsystems(
    to: &str,
    cc: &str,
    mapping: &[sashiko::settings::SubsystemMapping],
) -> Vec<(String, String)> {
    let mut subsystems = Vec::new();
    let mut all_recipients = String::new();
    all_recipients.push_str(to);
    all_recipients.push_str(", ");
    all_recipients.push_str(cc);

    let compiled_rules: Vec<_> = mapping
        .iter()
        .filter_map(|rule| {
            regex::Regex::new(&rule.pattern)
                .ok()
                .map(|re| (re, &rule.name))
        })
        .collect();

    for email in all_recipients.split(',') {
        let email = email.trim();
        if email.is_empty() {
            continue;
        }

        let lower_email = email.to_lowercase();
        let mut matched = false;

        for (re, name) in &compiled_rules {
            if re.is_match(&lower_email) {
                subsystems.push(((*name).clone(), lower_email.clone()));
                matched = true;
            }
        }

        // Fallback for known kernel lists if no mapping is provided
        if !matched {
            if lower_email.contains("linux-kernel@vger.kernel.org") {
                subsystems.push(("LKML".to_string(), lower_email));
            } else if lower_email.contains("netdev@vger.kernel.org") {
                subsystems.push(("netdev".to_string(), lower_email));
            } else if (lower_email.ends_with("@vger.kernel.org")
                || lower_email.ends_with("@lists.linux.dev")
                || lower_email.ends_with("@lists.infradead.org")
                || lower_email.ends_with("@kvack.org"))
                && let Some(name) = lower_email.split('@').next()
            {
                subsystems.push((name.to_string(), lower_email));
            }
        }
    }

    subsystems.sort();
    subsystems.dedup();
    subsystems
}

fn identify_subsystems_from_paths(
    paths: &[String],
    mapping: &[sashiko::settings::SubsystemMapping],
) -> Vec<(String, String)> {
    let mut subsystems = Vec::new();
    let compiled_rules: Vec<_> = mapping
        .iter()
        .filter_map(|rule| {
            regex::Regex::new(&rule.pattern)
                .ok()
                .map(|re| (re, &rule.name))
        })
        .collect();

    for path in paths {
        let lower_path = path.to_lowercase();
        for (re, name) in &compiled_rules {
            if re.is_match(&lower_path) {
                subsystems.push(((*name).clone(), (*name).clone() + "@forge.local"));
            }
        }
    }

    subsystems.sort();
    subsystems.dedup();
    subsystems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parsing() {
        let args = vec!["sashiko", "--download", "100", "--track", "--no-api"];
        let cli = Cli::parse_from(args);
        assert_eq!(cli.download, Some(100));
        assert!(cli.track);
        assert!(cli.no_api);

        let args = vec!["sashiko"];
        let cli = Cli::parse_from(args);
        assert_eq!(cli.download, None);
        assert!(!cli.track);
        assert!(!cli.no_api);
    }

    #[test]
    fn test_cli_no_ai() {
        let args = vec!["sashiko", "--no-ai"];
        let cli = Cli::parse_from(args);
        assert!(cli.no_ai);

        let args = vec!["sashiko"];
        let cli = Cli::parse_from(args);
        assert!(!cli.no_ai);
    }

    #[test]
    fn test_cli_port() {
        let args = vec!["sashiko", "--port", "8080"];
        let cli = Cli::parse_from(args);
        assert_eq!(cli.port, Some(8080));

        let args = vec!["sashiko"];
        let cli = Cli::parse_from(args);
        assert_eq!(cli.port, None);
    }

    #[test]
    fn test_cli_init() {
        let args = vec!["sashiko", "init", "--path", "/tmp/sashiko.toml", "--force"];
        let cli = Cli::parse_from(args);
        match cli.command {
            Some(Commands::Init {
                path,
                force,
                print,
                prompts,
            }) => {
                assert_eq!(path.as_deref(), Some(Path::new("/tmp/sashiko.toml")));
                assert!(force);
                assert!(!print);
                assert!(!prompts);
            }
            _ => panic!("expected init command"),
        }
    }

    #[test]
    fn test_init_command_writes_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sashiko.toml");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", temp.path().join("data"));
        }

        handle_init_command(Some(path.clone()), false, false, false).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, DEFAULT_SETTINGS);

        assert!(handle_init_command(Some(path.clone()), false, false, false).is_err());
        handle_init_command(Some(path), true, false, false).unwrap();

        unsafe {
            if let Some(value) = old_xdg {
                std::env::set_var("XDG_DATA_HOME", value);
            } else {
                std::env::remove_var("XDG_DATA_HOME");
            }
        }
    }

    #[test]
    fn test_cli_review() {
        let args = vec!["sashiko", "review"];
        let cli = Cli::parse_from(args);
        match cli.command {
            Some(Commands::Review { input, .. }) => {
                assert_eq!(input, "HEAD");
            }
            _ => panic!("expected review command"),
        }

        let args = vec![
            "sashiko",
            "review",
            "HEAD~2..HEAD",
            "--baseline",
            "main",
            "--no-ai",
            "--format",
            "json",
            "--color",
            "never",
        ];
        let cli = Cli::parse_from(args);
        match cli.command {
            Some(Commands::Review {
                input,
                baseline,
                settings,
                no_ai,
                format,
                color,
                ..
            }) => {
                assert_eq!(input, "HEAD~2..HEAD");
                assert_eq!(baseline.as_deref(), Some("main"));
                assert!(settings.is_none());
                assert!(no_ai);
                assert!(matches!(format, OutputFormat::Json));
                assert!(matches!(color, ColorMode::Never));
            }
            _ => panic!("expected review command"),
        }
    }

    #[test]
    fn test_cli_worker_hidden_command() {
        let args = vec![
            "sashiko",
            "worker",
            "--baseline",
            "HEAD~1",
            "--review-patch-index",
            "2",
            "--no-ai",
        ];
        let cli = Cli::parse_from(args);
        match cli.command {
            Some(Commands::Worker {
                baseline,
                review_patch_index,
                no_ai,
                ..
            }) => {
                assert_eq!(baseline.as_deref(), Some("HEAD~1"));
                assert_eq!(review_patch_index, Some(2));
                assert!(no_ai);
            }
            _ => panic!("expected worker command"),
        }
    }

    #[test]
    fn test_identify_subsystems() {
        // Test known subsystem
        let to = "linux-kernel@vger.kernel.org";
        let cc = "netdev@vger.kernel.org";
        let subsystems = identify_subsystems(to, cc, &[]);
        assert!(subsystems.contains(&(
            "LKML".to_string(),
            "linux-kernel@vger.kernel.org".to_string()
        )));
        assert!(subsystems.contains(&("netdev".to_string(), "netdev@vger.kernel.org".to_string())));

        // Test fallback
        let to = "unknown-list@vger.kernel.org";
        let cc = "";
        let subsystems = identify_subsystems(to, cc, &[]);
        assert!(subsystems.contains(&(
            "unknown-list".to_string(),
            "unknown-list@vger.kernel.org".to_string()
        )));

        // Test mixed
        let to = "linux-usb@vger.kernel.org, random-user@example.com";
        let cc = "bpf@vger.kernel.org";
        let subsystems = identify_subsystems(to, cc, &[]);
        assert!(subsystems.contains(&(
            "linux-usb".to_string(),
            "linux-usb@vger.kernel.org".to_string()
        )));
        assert!(subsystems.contains(&("bpf".to_string(), "bpf@vger.kernel.org".to_string())));
        // random-user should be ignored as it doesn't match list patterns
        assert_eq!(subsystems.len(), 2);

        // Test linux-mm
        let to = "linux-mm@kvack.org";
        let subsystems = identify_subsystems(to, "", &[]);
        assert!(subsystems.contains(&("linux-mm".to_string(), "linux-mm@kvack.org".to_string())));
    }

    #[test]
    fn test_identify_subsystems_custom_and_fallback() {
        let custom_mapping = vec![sashiko::settings::SubsystemMapping {
            pattern: ".*custom-list@example.com.*".to_string(),
            name: "Custom".to_string(),
        }];

        // Test that a known default list is still identified even with custom mappings present.
        let to = "netdev@vger.kernel.org";
        let cc = "custom-list@example.com";
        let subsystems = identify_subsystems(to, cc, &custom_mapping);

        assert_eq!(subsystems.len(), 2); // Both custom and default should be found
        assert!(subsystems.contains(&("netdev".to_string(), "netdev@vger.kernel.org".to_string())));
        assert!(
            subsystems.contains(&("Custom".to_string(), "custom-list@example.com".to_string()))
        );
    }

    #[test]
    fn test_identify_subsystems_from_paths() {
        let mapping = vec![sashiko::settings::SubsystemMapping {
            pattern: "^drivers/usb/.*".to_string(),
            name: "usb".to_string(),
        }];

        let paths = vec![
            "drivers/usb/core/devio.c".to_string(),
            "README.md".to_string(),
        ];
        let subsystems = identify_subsystems_from_paths(&paths, &mapping);

        assert_eq!(subsystems.len(), 1);
        assert!(subsystems.contains(&("usb".to_string(), "usb@forge.local".to_string())));
    }

    #[test]
    fn test_calculate_embargo_hours() {
        use sashiko::email_policy::{EmailPolicyConfig, SubsystemPolicy};
        use std::collections::HashMap;

        let mut subsystems_policy = HashMap::new();
        subsystems_policy.insert(
            "net".to_string(),
            SubsystemPolicy {
                lists: vec!["netdev@vger.kernel.org".to_string()],
                embargo_hours: Some(24),
                subject_prefixes: vec!["net".to_string(), "net-next".to_string()],
                ..Default::default()
            },
        );
        subsystems_policy.insert(
            "bpf".to_string(),
            SubsystemPolicy {
                lists: vec!["bpf@vger.kernel.org".to_string()],
                embargo_hours: Some(0),
                subject_prefixes: vec!["bpf".to_string(), "bpf-next".to_string()],
                ..Default::default()
            },
        );

        let policy = EmailPolicyConfig {
            defaults: SubsystemPolicy {
                embargo_hours: Some(1),
                ..Default::default()
            },
            subsystems: subsystems_policy,
        };

        // Case 1: No matching subsystems -> falls back to default
        let subs = vec![(
            "LKML".to_string(),
            "linux-kernel@vger.kernel.org".to_string(),
        )];
        assert_eq!(
            calculate_embargo_hours("[PATCH some-tree 1/2] foo", &subs, &policy),
            1
        );

        // Case 2: Single match
        let subs = vec![("netdev".to_string(), "netdev@vger.kernel.org".to_string())];
        assert_eq!(
            calculate_embargo_hours("[PATCH net-next v3 1/2] foo", &subs, &policy),
            24
        );

        // Case 3: Multiple matches without subject prefix match -> takes minimum
        let subs = vec![
            ("netdev".to_string(), "netdev@vger.kernel.org".to_string()),
            ("bpf".to_string(), "bpf@vger.kernel.org".to_string()),
        ];
        assert_eq!(
            calculate_embargo_hours("[PATCH 1/2] foo", &subs, &policy),
            0
        );

        // Case 4: Multiple matches with subject prefix match for net -> uses net
        let subs = vec![
            ("netdev".to_string(), "netdev@vger.kernel.org".to_string()),
            ("bpf".to_string(), "bpf@vger.kernel.org".to_string()),
        ];
        assert_eq!(
            calculate_embargo_hours("[PATCH net-next v3 1/2] foo", &subs, &policy),
            24
        );

        // Case 5: Multiple matches with subject prefix match for bpf -> uses bpf
        let subs = vec![
            ("netdev".to_string(), "netdev@vger.kernel.org".to_string()),
            ("bpf".to_string(), "bpf@vger.kernel.org".to_string()),
        ];
        assert_eq!(
            calculate_embargo_hours("[RFC PATCH bpf-next] foo", &subs, &policy),
            0
        );
    }

    #[test]
    fn test_nntp_ingestor_enabled_with_forge() {
        let mut settings = Settings::new().unwrap();
        settings.forge.enabled = true;
        settings.forge.disable_nntp = false;
        let should_start_ingestor = !(settings.forge.enabled && settings.forge.disable_nntp);
        assert!(should_start_ingestor);
    }

    #[test]
    fn test_nntp_ingestor_disabled_by_default_with_forge() {
        let mut settings = Settings::new().unwrap();
        settings.forge.enabled = true;
        settings.forge.disable_nntp = true; // This is the default
        let should_start_ingestor = !(settings.forge.enabled && settings.forge.disable_nntp);
        assert!(!should_start_ingestor);
    }
}
