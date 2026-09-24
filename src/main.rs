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
use sashiko::events::{Event, MessageSource, ParsedArticle};
use sashiko::ingestor::Ingestor;
use sashiko::local_review::{
    ProgressEvent, ReviewOptions, WorkerOptions, print_worker_json, result_has_error,
    result_has_high_or_critical_findings, run_git_review, run_worker_from_stdin,
};
use sashiko::project::ProjectId;
use sashiko::prompt_bundle;
use sashiko::reviewer::Reviewer;
use sashiko::settings::Settings;
use serde_json::Value;
use std::io::IsTerminal;
use std::io::Write;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use termcolor::{Buffer, BufferWriter, Color, ColorChoice, ColorSpec, StandardStream, WriteColor};
use tokio::signal::unix::{SignalKind, signal};
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

    /// Debug feature: run only these analysis stages, by name
    #[arg(long, hide = true, value_delimiter = ',')]
    stages: Option<Vec<String>>,

    /// The codebase to review (default: the configured project, else linux)
    ///
    /// Global, so it means the same thing wherever it is typed.
    #[arg(long, global = true, env = "SASHIKO_PROJECT")]
    project: Option<ProjectId>,

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

        /// Run only these analysis stages, by name
        #[arg(long, hide = true, value_delimiter = ',')]
        stages: Option<Vec<String>>,
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

        /// Run only these analysis stages, by name
        #[arg(long, hide = true, value_delimiter = ',')]
        stages: Option<Vec<String>>,
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
    // 2. Review command defaults to "warn" (unless --debug)
    // 3. Settings log_level
    // 4. Worker command defaults to "info" (worker logs progress on stderr)
    // 5. Fallback to "warn" (if settings failed)
    let is_review = matches!(cli.command, Some(Commands::Review { .. }));
    let is_worker = matches!(cli.command, Some(Commands::Worker { .. }));
    let log_level = if cli.debug {
        "info"
    } else if is_review {
        "warn"
    } else if let Ok(s) = &settings_result {
        &s.log_level
    } else if is_worker {
        "info"
    } else {
        "warn"
    };

    // Initialize tracing with EnvFilter
    // RUST_LOG env var still overrides everything if present
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    // Determine formatting features independently
    let plain_logs = std::env::var("SASHIKO_LOG_PLAIN").is_ok();
    let use_ansi = std::env::var("NO_COLOR").is_err() && std::io::stderr().is_terminal();

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

    // Resolved once, before anything dispatches on it, so the flag, the
    // environment and the settings file cannot be read in a different order by
    // two different code paths.
    let project = effective_project(
        cli.project,
        settings_result.as_ref().ok().and_then(|s| s.project.kind),
    )?;

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
                    project,
                    input.clone(),
                    baseline.clone(),
                    settings.clone(),
                    *no_ai,
                    custom_prompt.clone(),
                    ai_provider.clone(),
                    resolve_prompts_path(prompts.clone(), project)?,
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
                std::panic::set_hook(Box::new(|info| {
                    eprintln!("CRITICAL ERROR: Panic detected: {}", info);
                }));

                let result = run_worker_from_stdin(WorkerOptions {
                    project,
                    settings_path: None,
                    baseline: baseline.clone(),
                    repo: repo.clone(),
                    worktree_dir: worktree_dir.clone(),
                    prompts: resolve_prompts_path(prompts.clone(), project)?,
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
                .await;

                match result {
                    Ok(val) => {
                        print_worker_json(&val).map_err(Box::<dyn std::error::Error>::from)?;
                        return Ok(());
                    }
                    Err(e) => {
                        let err_val = serde_json::json!({
                            "patchset_id": 0,
                            "error": e.to_string()
                        });
                        let _ = print_worker_json(&err_val);
                        std::process::exit(1);
                    }
                }
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

    // The resolved project is the answer to the flag, the environment and the
    // file together. Writing it back means everything reading the settings
    // from here on, including the reviewer that has to pass it to its worker
    // subprocesses, sees the same answer rather than re-deriving it.
    settings.project.kind = Some(project);
    info!("Reviewing project: {project}");

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

    if let Err(reason) = settings.validate_sign_in_delivery() {
        error!("Refusing to start: {}", reason);
        return Err(reason.into());
    }

    // Initialize Database
    let db = Arc::new(Database::new(&settings.database).await?);
    db.migrate().await?;
    db.ensure_project_stamp(project).await?;

    // Load and initialize authoritative immutable MAINTAINERS index when the
    // reviewed project uses kernel MAINTAINERS.
    let maintainers_index = if project.uses_maintainers() {
        let linux_repo_path = std::path::PathBuf::from(&settings.git.repository_path);
        match sashiko::maintainers::MaintainersIndex::from_top_of_trunk(&linux_repo_path) {
            Ok(idx) => {
                info!(
                    "Successfully parsed and indexed {} MAINTAINERS sections from top-of-trunk of Linus's tree",
                    idx.len()
                );
                Arc::new(idx)
            }
            Err(e) => {
                warn!(
                    "Failed to load MAINTAINERS from top-of-trunk: {}. Using empty index.",
                    e
                );
                Arc::new(sashiko::maintainers::MaintainersIndex::new())
            }
        }
    } else {
        info!("Project {project} does not use kernel MAINTAINERS; skipping index.");
        Arc::new(sashiko::maintainers::MaintainersIndex::new())
    };
    sashiko::maintainers::init_global_maintainers(maintainers_index);

    // Attributes series ingested before attribution existed. Best effort: a
    // failure leaves those series readable by operators only, which is the
    // direction access control should fail in, so it is not worth refusing to
    // start over.
    if let Err(e) = sashiko::backfill::backfill_patchset_maintainer_sections(&db).await {
        warn!("Patchset MAINTAINERS attribution backfill failed: {e}");
    }

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
    let fetch_handle = tokio::spawn(async move {
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
                    Event::IngestionFailed {
                        article_id,
                        error,
                        source,
                    } => {
                        if let Err(e) = tx
                            .send(ParsedArticle {
                                group: "error".to_string(),
                                article_id,
                                source,
                                metadata: None,
                                patch: None,
                                baseline: None,
                                failed_error: Some(error),
                                skip_filters: None,
                                only_filters: None,
                                mr_url: None,
                                mr_title: None,
                                mr_number: None,
                                receipt: None,
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

                        let source = if group.starts_with("git-import") {
                            MessageSource::GitImport
                        } else {
                            MessageSource::GitFetch
                        };

                        if let Err(e) = tx
                            .send(ParsedArticle {
                                group,
                                article_id,
                                source,
                                metadata: Some(metadata),
                                patch,
                                baseline: base_commit,
                                failed_error: None,
                                skip_filters: None,
                                only_filters: None,
                                mr_url,
                                mr_title,
                                mr_number,
                                receipt: None,
                            })
                            .await
                        {
                            error!("Failed to send pre-parsed article: {}", e);
                        }
                    }
                    Event::RawMboxSubmitted {
                        raw,
                        submission_id,
                        source,
                        group,
                        baseline,
                        skip_subjects,
                        only_subjects,
                        submitted_at,
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
                                    // Do not override group "api-submit" to allow grouping logic to trigger
                                    let effective_group = group_clone;

                                    if let Err(e) = tx_clone
                                        .send(ParsedArticle {
                                            group: effective_group,
                                            article_id: submission_id.clone(),
                                            source,
                                            metadata: {
                                                let mut m = metadata;
                                                if submitted_at.is_some() {
                                                    m.received_date = submitted_at;
                                                }
                                                Some(m)
                                            },
                                            patch: patch_opt,
                                            baseline: baseline_clone,
                                            failed_error: None,
                                            skip_filters: skip_subjects_clone,
                                            only_filters: only_subjects_clone,
                                            mr_url: None,
                                            mr_title: None,
                                            mr_number: None,
                                            receipt: None,
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
                        mut receipt,
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
                                    // Dropping this article is a decision, not
                                    // a failure, so the mark may move past it.
                                    if let Some(receipt) = receipt.as_mut() {
                                        receipt.settle();
                                    }
                                    return;
                                }

                                if let Err(e) = tx
                                    .send(ParsedArticle {
                                        group,
                                        article_id,
                                        source: MessageSource::Nntp,
                                        metadata: Some(metadata),
                                        patch: patch_opt,
                                        baseline,
                                        failed_error: None,
                                        skip_filters: None,
                                        only_filters: None,
                                        mr_url: None,
                                        mr_title: None,
                                        mr_number: None,
                                        receipt,
                                    })
                                    .await
                                {
                                    error!("Failed to send parsed article: {}", e);
                                }
                            }
                            Ok(Err(e)) => {
                                // Parsing the same bytes again would fail the
                                // same way, so holding the mark back would
                                // wedge the group on this one article.
                                if let Some(receipt) = receipt.as_mut() {
                                    receipt.settle();
                                }
                                warn!("Dropping unparseable article {}: {}", article_id, e);
                            }
                            Err(e) => {
                                // A parser panic is just as repeatable as a
                                // parse error, so the article is dropped for
                                // the same reason.
                                if let Some(receipt) = receipt.as_mut() {
                                    receipt.settle();
                                }
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
    let db_worker_handle = tokio::spawn(async move {
        info!("DB Worker started");

        let mut buffer = Vec::with_capacity(100);
        let mut total_processed = 0;
        let mut total_ingested = 0;
        let mut total_errors = 0;

        let policy = sashiko::email_policy::EmailPolicyConfig::load("email_policy.toml")
            .expect("Failed to parse email_policy.toml");

        loop {
            let count = parsed_rx.recv_many(&mut buffer, 100).await;
            if count == 0 {
                break;
            }

            let patch_ids = sashiko::prerequisites::calculate_git_patch_id_batch(
                buffer
                    .iter()
                    .map(|article| article.patch.as_ref().map(|patch| patch.diff.as_str()))
                    .collect(),
            )
            .await;
            for (mut article, git_patch_id) in buffer.drain(..).zip(patch_ids) {
                let git_patch_id = match git_patch_id {
                    Ok(git_patch_id) => git_patch_id,
                    Err(e) => {
                        let message_id = article
                            .patch
                            .as_ref()
                            .map_or(article.article_id.as_str(), |patch| {
                                patch.message_id.as_str()
                            });
                        warn!(
                            "Failed to calculate stable patch ID for {}: {}",
                            message_id, e
                        );
                        None
                    }
                };
                let mut receipt = article.receipt.take();
                match process_parsed_article(&worker_db, article, git_patch_id, &policy, &mapping)
                    .await
                {
                    ProcessStatus::Ingested => {
                        // The article is on disk, so the fetch loop may finally
                        // move its mark past it.
                        if let Some(receipt) = receipt.as_mut() {
                            receipt.settle();
                        }
                        total_ingested += 1;
                    }
                    // The receipt is dropped unsettled, which reports the
                    // article as lost and keeps the mark below it.
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

    // Warn about insecure forge webhook configurations
    if settings.forge.enabled && settings.forge.webhook_secret.is_none() {
        if cli.enable_unsafe_all_submit {
            warn!(
                "Accepting unauthenticated webhook requests from all addresses. \
                 Configure forge.webhook_secret for production deployments."
            );
        } else {
            warn!(
                "Forge webhooks enabled without webhook_secret. \
                 Non-localhost requests require --enable-unsafe-all-submit. \
                 See docs/WEBHOOK_SECURITY.md"
            );
        }
    }

    // Start Ingestor (feeds raw_tx)
    let ingestor_handle = if should_start_nntp_ingestor(&settings) {
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
        info!("Lore/NNTP ingestor is disabled (no NNTP config or disabled by forge).");
        tokio::spawn(async move {
            std::future::pending::<()>().await;
        })
    };

    // Start Web API
    let api_settings = Arc::new(settings.clone());
    let api_db = db.clone();
    let api_tx = raw_tx.clone();
    let api_fetch_tx = fetch_tx.clone();
    let local_token_path = settings.local_token_path();
    let local_token = publish_local_token(&local_token_path);
    let server_options = sashiko::api::ServerOptions {
        allow_all_submit: cli.enable_unsafe_all_submit,
        smtp_enabled: settings.smtp.is_some(),
        dry_run: settings.smtp.as_ref().map(|s| s.dry_run).unwrap_or(false),
        local_token,
    };
    let api_handle = tokio::spawn(async move {
        if let Err(e) =
            sashiko::api::run_server(api_settings, api_db, api_tx, api_fetch_tx, server_options)
                .await
        {
            error!("Web API fatal error: {}", e);
        }
    });

    // Start Email Worker
    let email_handle = if let Some(smtp_settings) = settings.smtp.clone() {
        let email_worker = sashiko::worker::email::EmailWorker::new(
            db.clone(),
            smtp_settings,
            settings.server.log_sign_in_links,
        );

        Some(tokio::spawn(async move {
            email_worker.run().await;
        }))
    } else {
        None
    };

    // Start Patchwork Worker (processes API check entries when they exist)
    let patchwork_handle = {
        let pw_policy_path = settings.review.email_policy_path.clone();
        let pw_max_retries = settings.review.max_retries;
        let patchwork_worker = sashiko::worker::patchwork::PatchworkWorker::new(
            db.clone(),
            pw_policy_path,
            pw_max_retries,
        );
        tokio::spawn(async move {
            patchwork_worker.run().await;
        })
    };

    let forge_handle = if settings.forge.enabled {
        let forge_worker = sashiko::worker::forge::ForgeWorker::new(
            db.clone(),
            settings.forge.clone(),
            settings.review.max_retries,
        );
        Some(tokio::spawn(async move {
            forge_worker.run().await;
        }))
    } else {
        None
    };

    let bug_worker_handle = {
        let provider =
            sashiko::ai::create_provider(&settings).expect("Provider setup failed for bug worker");
        let bug_worker = sashiko::worker::bug_worker::BugWorker::new(
            db.clone(),
            provider,
            settings.git.repository_path.clone(),
        );
        tokio::spawn(async move {
            bug_worker.run().await;
        })
    };
    // Initialize custom remotes
    // Start Background Compressor Worker
    let compressor_handle = tokio::spawn(sashiko::worker::compressor::run_compressor(db.clone()));
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

    // Auto-maintenance repacks in the background while the sync worker
    // keeps fetching, which can leave a commit-graph naming objects the
    // repack removed.
    if let Err(e) = sashiko::git_ops::ensure_gc_disabled(&repo_path).await {
        error!("Failed to disable git auto-maintenance: {}", e);
    }

    // Recover the object store the way the worktrees above are
    // recovered.  The write brings the graph up to the refs the last
    // run left behind, and drops a graph that outlived the objects it
    // names rather than reporting it.
    //
    // The walk visits every reachable commit, which runs to minutes
    // on a tree the size of Linux.  Awaiting it here held the sync
    // worker and the reviewer off for that long on every restart.
    // Run it beside those workers instead.  The repack worker
    // rewrites the pack directory.  Both passes take the object-store
    // lock, so it cannot run underneath the walk.
    let graph_repo_path = repo_path.clone();
    let commit_graph_handle = tokio::spawn(async move {
        if let Err(e) = sashiko::git_ops::write_commit_graph(&graph_repo_path).await {
            error!("Failed to write the commit-graph: {}", e);
        }
    });

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
    let sync_handle = {
        let sync_worker = sashiko::worker::sync::GitSyncWorker::new(repo_path.clone());
        tokio::spawn(async move {
            sync_worker.run().await;
        })
    };

    // Start Repack Worker
    let repack_handle = {
        let repack_worker = sashiko::worker::repack::RepackWorker::new(repo_path.clone());
        tokio::spawn(async move {
            repack_worker.run().await;
        })
    };

    // Start Reviewer Service
    let reviewer = Reviewer::new(db.clone(), settings.clone()).await;
    let reviewer_handle = tokio::spawn(async move {
        reviewer.start().await;
    });

    let metrics_db = db.clone();
    let metrics_repo_path = repo_path.clone();
    let metrics_handle = tokio::spawn(async move {
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
            match sashiko::git_ops::pack_stats(&metrics_repo_path).await {
                Ok((packs, bytes)) => sashiko::metrics::set_repo_packs(packs, bytes),
                Err(e) => warn!("Failed to count packs in the review repository: {}", e),
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });

    // Keep the main thread running
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    // Abort all background task handles
    fetch_handle.abort();
    ingestor_handle.abort();
    parser_handle.abort();
    db_worker_handle.abort();
    api_handle.abort();
    if let Some(h) = email_handle {
        h.abort();
    }
    patchwork_handle.abort();
    if let Some(h) = forge_handle {
        h.abort();
    }
    bug_worker_handle.abort();
    compressor_handle.abort();
    commit_graph_handle.abort();
    sync_handle.abort();
    repack_handle.abort();
    reviewer_handle.abort();
    metrics_handle.abort();

    // A token from a dead server authenticates nothing, since the next one
    // draws a new secret, but leaving the file behind invites a local tool to
    // present a credential nobody honours and puzzle over the refusal.
    if let Err(e) = std::fs::remove_file(&local_token_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(
            "Failed to remove the local token at {}: {}",
            local_token_path.display(),
            e
        );
    }

    info!("Shutdown complete.");
    std::process::exit(0);
}

/// Publishes the credential local tooling presents to this process.
///
/// A failure is a warning rather than a fatal error: a read-only state
/// directory is a legitimate deployment, and the server remains fully usable
/// through sign-in links. Only the convenience of local tooling is lost, so it
/// says so plainly rather than refusing to start.
fn publish_local_token(path: &Path) -> Option<sashiko::auth::LocalToken> {
    let token = match sashiko::auth::LocalToken::generate() {
        Ok(token) => token,
        Err(e) => {
            warn!("Failed to generate the local token: {}", e);
            return None;
        }
    };

    match token.write_to(path) {
        Ok(()) => {
            // The path is logged and the secret is not, because the log is read
            // by more people and processes than the file is.
            info!("Local tooling may authenticate with {}", path.display());
            Some(token)
        }
        Err(e) => {
            warn!(
                "Failed to write the local token to {}: {}. Local tools will have to \
                 authenticate like any other caller.",
                path.display(),
                e
            );
            None
        }
    }
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

/// The project this invocation is for.
///
/// The flag wins over the settings file, but a settings file naming a
/// different project is not overridden silently. A settings file is what points
/// at a database, a git tree and a port, so a flag that disagrees with it is
/// asking to run one project against another one's state. Refusing is cheap;
/// noticing afterwards is not.
fn effective_project(
    requested: Option<ProjectId>,
    configured: Option<ProjectId>,
) -> Result<ProjectId, String> {
    match (requested, configured) {
        (Some(flag), Some(configured)) if flag != configured => Err(format!(
            "--project {flag} disagrees with the loaded settings, which are for {configured}; \
             point at the settings for {flag} or drop the flag"
        )),
        (Some(flag), _) => Ok(flag),
        (None, Some(configured)) => Ok(configured),
        (None, None) => Ok(ProjectId::default()),
    }
}

fn resolve_prompts_path(
    path: Option<PathBuf>,
    project: ProjectId,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = path {
        return Ok(path);
    }

    Ok(prompt_bundle::project_prompts_path(project)?)
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
    planned_stages: Vec<String>,
    active_stages: std::collections::BTreeSet<String>,
    completed_stages: usize,
    active_stage_turns: std::collections::HashMap<String, usize>,
    /// Stages that ran before the fan-out resolved, and so are not in
    /// `planned_stages`: the pre-screen and the planner.
    ///
    /// Counted as they start rather than predicted, because whether either runs
    /// depends on `--stages`. A stage that has started is one that ran.
    preliminary_stages: usize,
}

/// The window size of the terminal behind `stream`, in rows and columns, or
/// None where the stream has none.
///
/// A stream can be a terminal and still have no size to report: a pty carries a
/// window size only once something sets one, and answers zeroes until then.
fn window_size(stream: &impl AsFd) -> Option<(usize, usize)> {
    let size = rustix::termios::tcgetwinsize(stream).ok()?;
    if size.ws_row == 0 || size.ws_col == 0 {
        return None;
    }

    Some((size.ws_row as usize, size.ws_col as usize))
}

/// Keeps the display's idea of the terminal width current for as long as the
/// review runs, and repaints at the new width as soon as it changes.
///
/// A window that is resized leaves every line the display drew wrapped to a
/// width the terminal no longer has, and SIGWINCH is the only notice of it: the
/// size has to be asked for again. Does nothing where stderr has no window,
/// there being no size to follow.
///
/// The repaint arrives as an argument rather than being called outright, so that
/// a test can watch a resize reach the display without a frame being painted at
/// the terminal running the test.
fn watch_for_resize(
    state: Arc<std::sync::Mutex<ProgressState>>,
    stream: impl AsFd + Send + 'static,
    mut repaint: impl FnMut(&mut ProgressState) + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    window_size(&stream)?;

    let mut resized = match signal(SignalKind::window_change()) {
        Ok(resized) => resized,
        Err(e) => {
            warn!("Progress display cannot follow the terminal size: {}", e);
            return None;
        }
    };

    // The size this display started from was read before the listener above
    // existed, and a resize in between is a resize nothing will report again.
    // Ask once here, so the gap is closed rather than waited out.
    {
        let mut state = state.lock().unwrap();
        if let Some((_, cols)) = window_size(&stream)
            && state.terminal_width != cols
        {
            state.terminal_width = cols;
            repaint(&mut state);
        }
    }

    Some(tokio::spawn(async move {
        while resized.recv().await.is_some() {
            // The signal is delivered to the process, not to a stream, so it
            // says only that something was resized. Ask what this stream is now
            // and let the answer decide: a wake that leaves the screen the size
            // it was has nothing to redraw for.
            let Some((rows, cols)) = window_size(&stream) else {
                continue;
            };

            let mut state = state.lock().unwrap();
            if (state.terminal_rows, state.terminal_width) == (rows, cols) {
                continue;
            }
            state.terminal_rows = rows;
            state.terminal_width = cols;
            repaint(&mut state);
        }
    }))
}

struct ProgressState {
    project: ProjectId,
    patches: std::collections::BTreeMap<i64, PatchState>,
    /// The status each patch was last reported with, so the appending display
    /// speaks only when one of them changes.
    last_status: std::collections::BTreeMap<i64, String>,
    total_turns: usize,
    terminal_width: usize,
    terminal_rows: usize,
    color_choice: ColorChoice,
    /// Whether the display may keep lines of its own, or has to append them.
    reservation_capable: bool,
    /// Lines currently held at the foot of the screen, zero before the region is
    /// set up and after it is given back.
    reserved: usize,
    /// The screen height those lines were placed against. A resize moves the foot
    /// of the screen, so the region has to be set again even where the count of
    /// lines has not changed.
    region_rows: usize,
    /// Whether the display has said its last word.
    ///
    /// The review's report prints after this, and the resize watcher outlives the
    /// region by a moment: a repaint arriving in that moment would take a region
    /// back and draw a frame across the report. Nothing paints once this is set.
    finished: bool,
}

/// Display label for a stage. Held in the stage tables so that adding a stage
/// needs no edit here.
fn stage_short_name(project: ProjectId, stage: &str) -> &'static str {
    sashiko::workflows::stage_short_label(project, stage).unwrap_or("Unknown")
}

struct TruncatingWriter {
    limit: usize,
    written: usize,
}

impl TruncatingWriter {
    fn new(limit: usize) -> Self {
        Self { limit, written: 0 }
    }

    fn write_segment(
        &mut self,
        out: &mut impl WriteColor,
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

        let mut spec = ColorSpec::new();
        if let Some(c) = color {
            spec.set_fg(Some(c));
        }
        if bold {
            spec.set_bold(true);
        }
        out.set_color(&spec)?;
        write!(out, "{}", to_write)?;

        self.written += to_write.chars().count();

        if !suffix.is_empty() {
            out.reset()?;
            write!(out, "{}", suffix)?;
            self.written += 3;
        }

        out.reset()
    }
}

/// Whether the terminal `info` describes has any color to set.
///
/// isatty says that a stream is a terminal, not what kind of one: vt100 and
/// xterm-mono are terminals with no color at all, and the SGR codes a review
/// would paint at them are at best ignored. terminfo answers it properly, as the
/// number of colors the terminal has and the string that selects one.
fn color_capable(info: &terminfo::Database) -> bool {
    info.get::<terminfo::capability::MaxColors>()
        .is_some_and(|colors| colors.0 >= 8)
        && info.get::<terminfo::capability::SetAForeground>().is_some()
}

/// Whether the terminal `info` describes can keep lines to itself:
/// change_scroll_region to confine scrolling to everything above them,
/// save_cursor and restore_cursor to leave ordinary output where it was, and
/// cursor_address to write the lines outright.
///
/// Independent of whether it has color: a vt100 can do all of this and has no
/// color at all, while TERM=ansi has color and no scroll region.
fn reservation_capable(info: &terminfo::Database) -> bool {
    info.get::<terminfo::capability::ChangeScrollRegion>()
        .is_some()
        && info.get::<terminfo::capability::SaveCursor>().is_some()
        && info.get::<terminfo::capability::RestoreCursor>().is_some()
        && info.get::<terminfo::capability::CursorAddress>().is_some()
}

/// What one of the streams a review writes to can do. Asked of the stream
/// itself, since redirecting one says nothing about the other.
#[derive(Clone, Copy, Debug, PartialEq)]
struct OutputStream {
    color: ColorChoice,
    /// Rows and columns, or 24x80 from a stream that reports no window. Being a
    /// terminal and having a size are separate answers: a pty whose size has
    /// never been set is a terminal that reports none, and "--color always" asks
    /// for escapes on streams that are no terminal at all.
    size: (usize, usize),
    /// Whether the progress display may keep lines of its own here, rather than
    /// appending a line at a time.
    reservation_capable: bool,
}

impl OutputStream {
    /// `terminal` describes what TERM names, or nothing where TERM names
    /// something terminfo does not know: a stream can only do what both it and
    /// the terminal behind it can.
    fn detect(mode: ColorMode, stream: &impl AsFd, terminal: Option<&terminfo::Database>) -> Self {
        let colored = match terminal {
            Some(info) => color_capable(info),
            // Default to color capable if terminfo is missing and TERM is set to
            // anything other than "dumb".
            None => std::env::var("TERM").is_ok_and(|term| !term.is_empty() && term != "dumb"),
        };

        // Lines at the foot of the screen can only be placed on a screen whose
        // height is known. A stream that reports no window gets none, whatever
        // the terminal is otherwise capable of, rather than having them placed
        // against the fallback and written over what is on screen.
        let size = window_size(stream);
        let can_reserve = |info: &terminfo::Database| size.is_some() && reservation_capable(info);

        // "always" and "never" answer for the run rather than for a stream, so
        // only "auto" asks the stream or the terminal anything. A stream that is
        // no terminal has no terminal behind it to ask, so it gets no color
        // whatever TERM says.
        let (color, reservation_capable) = match mode {
            // Forcing color asserts that the escapes arrive whatever isatty
            // says. It asserts nothing about scroll regions, so a terminal
            // terminfo does not know is told about each change rather than
            // trusted with part of the screen.
            ColorMode::Always => (ColorChoice::Always, terminal.is_some_and(can_reserve)),
            ColorMode::Never => (ColorChoice::Never, false),
            ColorMode::Auto if stream.as_fd().is_terminal() => (
                if colored {
                    ColorChoice::Auto
                } else {
                    ColorChoice::Never
                },
                terminal.is_some_and(can_reserve),
            ),
            ColorMode::Auto => (ColorChoice::Never, false),
        };

        Self {
            color,
            reservation_capable,
            size: size.unwrap_or((24, 80)),
        }
    }
}

/// Paints a frame to stderr, where the progress display lives, in one write.
fn render_progress(state: &mut ProgressState) {
    on_stderr(state, paint_progress);
}

/// Gives back whatever the display took of the screen, to stderr.
///
/// Called before the report prints rather than left to a destructor: the review
/// exits through std::process::exit when it has findings, and destructors do not
/// run then.
fn finish_progress(state: &mut ProgressState) {
    on_stderr(state, |state, out| {
        release_progress_region(state, out, RegionExit::Kept)
    });
}

/// Gives the screen back on the way out of a panic, before the panic itself is
/// reported.
///
/// A scroll region is the one thing this display leaves behind that outlives the
/// process, and a panic reaches neither the release below nor a destructor on the
/// way past. The hook writes the reset itself rather than going through the
/// display's state: a panic while that mutex is held would otherwise wait for a
/// lock nothing is going to give back. It saves and restores the cursor around
/// the reset, DECSTBM homing it, or the panic prints from the top of the screen
/// over whatever was there.
fn release_region_on_panic() {
    let reported = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        // Scrolling back to the whole screen, leaving the cursor where the
        // output had reached, and a line to report from.
        let _ = std::io::stderr().write_all(b"\x1b7\x1b[r\x1b8\n");
        reported(panic);
    }));
}

/// Gives the screen back when the review is interrupted.
///
/// Ctrl-C kills the process where it stands, reaching neither the release above
/// nor the panic hook, and the scroll region outlives it: the shell that comes
/// back is confined to the top of the screen until something resets it. So the
/// signals that end a review are taken, the reset written, and the process left
/// to exit with the status that signal would have given it.
///
/// Saves and restores the cursor around the reset, as the panic hook does, or a
/// review interrupted mid-frame reports from the top of the screen.
///
/// Taking them replaces the default action, so the exit status is put back by
/// hand: the shell that started the review still sees it die of the signal it
/// sent. The reset is four bytes and the exit follows it, so the window where a
/// second interrupt would find nothing listening is not one worth covering.
fn release_region_on_interrupt() {
    for kind in [
        SignalKind::interrupt(),
        SignalKind::terminate(),
        SignalKind::hangup(),
    ] {
        let Ok(mut signalled) = signal(kind) else {
            warn!("Progress display cannot give the screen back if interrupted");
            continue;
        };

        tokio::spawn(async move {
            if signalled.recv().await.is_some() {
                // Scrolling back to the whole screen, leaving the cursor where
                // the output had reached, and a line to report from.
                let _ = std::io::stderr().write_all(b"\x1b7\x1b[r\x1b8\n");
                std::process::exit(128 + kind.as_raw_value());
            }
        });
    }
}

/// Writes what `paint` produces to stderr, in one write.
///
/// The one place that knows the display draws on stderr. A whole frame in a
/// single write also arrives whole: every write to stderr takes the same lock
/// inside std, so nothing of the log can land between the cursor being saved and
/// restored and be written into the reserved lines.
fn on_stderr(
    state: &mut ProgressState,
    paint: impl FnOnce(&mut ProgressState, &mut Buffer) -> std::io::Result<()>,
) {
    let stderr = BufferWriter::stderr(state.color_choice);
    let mut frame = stderr.buffer();
    if paint(state, &mut frame).is_ok() {
        let _ = stderr.print(&frame);
    }
}

/// Describes what a patch is doing. `with_turns` adds the turn counter, which
/// changes on every model call and so is only of use to a display that draws
/// over what it said last.
fn status_label(project: ProjectId, p: &PatchState, with_turns: bool) -> String {
    match &p.status {
        PatchStatus::Queued => "Queued".to_string(),
        PatchStatus::PreScreening => "Pre-screening guides...".to_string(),
        PatchStatus::Planning => "Planning stages...".to_string(),
        PatchStatus::Reviewing => {
            if p.active_stages.is_empty() {
                "Reviewing...".to_string()
            } else {
                // A repainting frame leads with the busiest stage, the turn
                // counter beside it saying why that one. Dropping the counter has
                // to drop that order with it: a stage overtaking another would
                // otherwise change the line for a reason the reader cannot see,
                // and an appending display would say the whole line again. Where
                // the counter is hidden, and among stages tied on it, the set's
                // own order decides, which changes only when the set does.
                let busiest = |stage: &String| {
                    let turns = p.active_stage_turns.get(stage).copied().unwrap_or(0);
                    std::cmp::Reverse(if with_turns { turns } else { 0 })
                };
                let top_stage = p
                    .active_stages
                    .iter()
                    .min_by_key(|stage| busiest(stage))
                    .expect("a stage, the set is not empty");
                let top_turn = p.active_stage_turns.get(top_stage).copied().unwrap_or(0);
                let stage_name = stage_short_name(project, top_stage);
                let stage_str = if with_turns && top_turn > 0 {
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
    }
}

/// Appends a line per patch whenever its status changes, without moving the
/// cursor.
///
/// Nothing is erased or overwritten, so the output survives a terminal that
/// cannot be drawn over, and survives being redirected: no escape sequences, and
/// no frame a later repaint would have to find again. The overall bar and the
/// turn counter are dropped, both being things only a repainting display can
/// show without a line per change.
fn paint_progress_plain(
    state: &mut ProgressState,
    out: &mut impl WriteColor,
) -> std::io::Result<()> {
    for (&idx, p) in &state.patches {
        let label = status_label(state.project, p, false);
        if state.last_status.get(&idx) == Some(&label) {
            continue;
        }
        state.last_status.insert(idx, label.clone());
        writeln!(out, "      [Patch {}] {} | {}", idx, p.subject, label)?;
    }

    out.flush()
}

/// Whether `wanted` lines are worth taking out of a screen of `rows`.
///
/// The display is there to be glanced at while a review's own output scrolls
/// past it, so it may have at most three quarters of the screen. Thirty patches
/// on a twenty-four line terminal would otherwise leave nothing to scroll in,
/// and the reader watching a display instead of a review.
fn worth_reserving(wanted: usize, rows: usize) -> bool {
    wanted * 4 <= rows * 3
}

/// Reserves `wanted` lines at the foot of the screen for the display.
///
/// Scrolls up to make room, then confines scrolling to everything above the lines
/// it took with DECSTBM, so ordinary output can never reach them.
///
/// The room is scrolled for at the foot of the screen, which is the only place a
/// newline is certain to scroll rather than to step onto a line that is already
/// free: every line taken comes from somewhere.
///
/// Leaves the cursor at the top of the screen, because DECSTBM homes it and the
/// form that gives the margins back homes it too. Nothing here puts it back,
/// since there is nowhere safe to put it back to while the margins are moving;
/// the caller parks it on the last line of the region once they are settled.
///
/// Run for every frame rather than only where the geometry changed, so it has to
/// be safe against margins it has already set, which it is: the escapes say the
/// same thing again and no room is asked for twice.
///
/// Asked for only where those lines are worth taking, so the caller has already
/// left something to scroll in.
fn reserve_progress_region(
    state: &mut ProgressState,
    out: &mut impl WriteColor,
    wanted: usize,
) -> std::io::Result<()> {
    // Hand back whatever is held first, so the newlines below scroll the whole
    // screen rather than the part above a region, and so the lines held are the
    // only thing this has to be right about.
    write!(out, "\x1b[r")?;

    // Room for the lines not held yet, scrolled for at the foot of the screen,
    // where a newline can only scroll: every line taken comes from somewhere.
    let room = wanted.saturating_sub(state.reserved);
    write!(out, "\x1b[{};1H", state.terminal_rows)?;
    for _ in 0..room {
        writeln!(out)?;
    }

    let split = state.terminal_rows - wanted;
    write!(out, "\x1b[1;{split}r")?;

    state.reserved = wanted;
    state.region_rows = state.terminal_rows;
    Ok(())
}

/// What the display leaves on screen when it gives its region back.
#[derive(Debug, Clone, Copy, PartialEq)]
enum RegionExit {
    /// Erase the frame. The display has outgrown the screen and carries on a
    /// line at a time from here, so the frame it drew is not its last word.
    Erased,
    /// Keep the frame and step past it. The review is over and that frame is
    /// what it ended up saying, so whatever prints next belongs below it.
    Kept,
}

/// Gives the screen back: scrolling returns to the whole screen, and `exit` says
/// what becomes of the frame the display drew in it.
///
/// An `Erased` release clears the lines it held and leaves the cursor where the
/// output had reached, so whatever prints next carries on from there rather than
/// from below lines that are no longer the display's. It says so whether or not
/// lines are currently held: a region left set is the one piece of this display
/// that outlives the process, and a resize can leave the count of held lines
/// behind.
///
/// A `Kept` release leaves the frame where it is and steps below it instead, and
/// is the one that says nothing at all where nothing is held, because the escape
/// that hands the margins back homes the cursor.
fn release_progress_region(
    state: &mut ProgressState,
    out: &mut impl WriteColor,
    exit: RegionExit,
) -> std::io::Result<()> {
    if !state.reservation_capable {
        return Ok(());
    }

    if exit == RegionExit::Kept {
        // Nothing held, nothing said. Giving margins back that were already given
        // back is not free: DECSTBM homes the cursor, so a reset with no region
        // behind it sends the report to the top of the screen and over the log
        // that is already there. This is called twice by design — once when the
        // review says it is complete, once on the way out for the paths that
        // never get there — and the second call is exactly that case.
        if state.reserved == 0 {
            state.finished = true;
            return Ok(());
        }

        // Margins back first, which homes the cursor, then down to the last line
        // the display held — the overall bar — and one line past it. No save and
        // no restore: a restore here would put the cursor back inside the rows
        // the frame occupies, which is what printed the report over them.
        write!(out, "\x1b[r")?;
        write!(out, "\x1b[{};1H", state.terminal_rows)?;
        writeln!(out)?;
        state.reserved = 0;
        state.finished = true;
        return out.flush();
    }

    // Only the lines this display holds are cleared, and only while it still
    // knows which rows those are. A screen resized since the region was placed
    // does not: clearing the last `reserved` rows of a screen that has shrunk
    // below that count would clear all of it, rows that were never the
    // display's. The margins still go back, and the stale frame scrolls away.
    let held = if state.region_rows == state.terminal_rows {
        state.reserved
    } else {
        0
    };

    write!(out, "\x1b7")?;
    let top = state.terminal_rows.saturating_sub(held) + 1;
    for row in top..=state.terminal_rows {
        write!(out, "\x1b[{row};1H\x1b[2K")?;
    }
    write!(out, "\x1b[r\x1b8")?;

    state.reserved = 0;
    out.flush()
}

/// Paints one frame into `out`, erasing the frame before it.
///
/// Every byte the display produces goes here, stderr included, so what a frame
/// is can be asked of a buffer rather than of a terminal. Nothing in this
/// function knows which stream it draws on.
fn paint_progress(state: &mut ProgressState, out: &mut impl WriteColor) -> std::io::Result<()> {
    if state.finished {
        return Ok(());
    }

    // One line per patch and one for the overall bar. Reserving happens once, and
    // again when that count changes, which it does as the patches become known.
    let wanted = state.patches.len() + 1;
    if !state.reservation_capable || !worth_reserving(wanted, state.terminal_rows) {
        // A display that has outgrown the screen was holding a region until now,
        // and hands it back on the way to saying its piece a line at a time.
        if state.reserved > 0 {
            release_progress_region(state, out, RegionExit::Erased)?;
        }
        return paint_progress_plain(state, out);
    }

    // Asserted on every frame, not only when the count of lines or the height of
    // the screen changes. Anything can give the margins back without saying so:
    // tmux redrawing a pane, another program, a stray reset. A frame that took
    // the last one on trust would then paint its lines into a screen that
    // scrolls them away.
    reserve_progress_region(state, out, wanted)?;

    let mut lines_printed = 0;
    let limit = state.terminal_width.saturating_sub(5);

    let top = state.terminal_rows.saturating_sub(state.reserved) + 1;

    for (&idx, p) in &state.patches {
        write!(out, "\x1b[{};1H\x1b[2K", top + lines_printed)?;
        let status_str = status_label(state.project, p, true);

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

        let mut tw = TruncatingWriter::new(limit);
        let _ = tw.write_segment(out, &format!("      [Patch {}] ", idx), None, false);
        let _ = tw.write_segment(out, &subject_padded, None, false);
        let _ = tw.write_segment(out, " | ", None, false);

        let (status_color, status_bold) = match &p.status {
            PatchStatus::Queued => (None, false),
            PatchStatus::PreScreening | PatchStatus::Planning => (Some(Color::Cyan), false),
            PatchStatus::Reviewing => (Some(Color::Cyan), true),
            PatchStatus::Finished => (Some(Color::Green), true),
        };
        let _ = tw.write_segment(out, &status_str, status_color, status_bold);

        lines_printed += 1;
    }

    let total_patches = state.patches.len();
    if total_patches > 0 {
        let total_stages: usize = state
            .patches
            .values()
            .map(|p| patch_stage_total(state.project, p))
            .sum();
        let completed_stages: usize = state.patches.values().map(|p| p.completed_stages).sum();
        let width = 20;
        let (display_completed_stages, percent, filled) =
            calculate_progress_metrics(total_stages, completed_stages, width);

        write!(out, "\x1b[{};1H\x1b[2K", top + lines_printed)?;
        let mut tw = TruncatingWriter::new(limit);
        let _ = tw.write_segment(out, "Overall: [", None, true);

        let filled_bar = "█".repeat(filled);
        let _ = tw.write_segment(out, &filled_bar, Some(Color::Green), false);

        let empty_bar = "░".repeat(width.saturating_sub(filled));
        let _ = tw.write_segment(out, &empty_bar, None, false);

        let _ = tw.write_segment(out, "] ", None, true);

        let stats = format!(
            "{}% | {}/{} stages | {} turns",
            percent, display_completed_stages, total_stages, state.total_turns
        );
        let _ = tw.write_segment(out, &stats, None, false);
    }

    // The last line of the region, which is where ordinary output carries on
    // from. Parked outright rather than saved and restored: a restore puts the
    // cursor back where it was, and where it was can be outside the region a
    // resize has just moved, which leaves every log record overwriting the same
    // row with no way back. The column is lost with it, so a record written
    // without a newline would be overwritten; the log writes whole lines.
    //
    // On a screen with room to spare this pulls the log down to meet the display
    // and leaves a blank band above it until the output scrolls that away, which
    // is the price of not knowing the row the log had reached.
    write!(
        out,
        "\x1b[{split};1H",
        split = state.terminal_rows - state.reserved
    )?;
    out.flush()
}

/// Stages this patch will have run by the time it is finished.
///
/// The pre-screen and the planner are added as they start, since whether they
/// run is not knowable in advance. The rest are whatever the fan-out resolved
/// to, plus the consolidation stages that always follow; before it resolves,
/// assume every stage, which is where a review with no `--stages` and a generous
/// planner ends up anyway.
fn patch_stage_total(project: ProjectId, p: &PatchState) -> usize {
    // Once a patch is done, what it ran is all it was ever going to run. The
    // workflow leaves by an early exit whenever a stage empties the concern
    // list, and the consolidation stages after that point never run, though
    // planned_stages counted them. Whatever else went unrun, a failed stage
    // under BestEffort say, is settled here too.
    if matches!(p.status, PatchStatus::Finished) {
        return p.completed_stages;
    }

    let planned = if p.planned_stages.is_empty() {
        sashiko::workflows::default_stage_count(project)
    } else {
        p.planned_stages.len()
    };

    p.preliminary_stages + planned
}

fn calculate_progress_metrics(
    total_stages: usize,
    completed_stages: usize,
    width: usize,
) -> (usize, usize, usize) {
    if total_stages == 0 {
        return (0, 0, 0);
    }

    let display_completed_stages = completed_stages.min(total_stages);
    let percent = display_completed_stages.saturating_mul(100) / total_stages;
    let filled = (display_completed_stages.saturating_mul(width) / total_stages).min(width);

    (display_completed_stages, percent, filled)
}

#[allow(clippy::too_many_arguments)]
async fn handle_review_command(
    project: ProjectId,
    input: String,
    baseline: Option<String>,
    settings_path: Option<PathBuf>,
    no_ai: bool,
    custom_prompt: Option<String>,
    ai_provider: Option<String>,
    prompts: PathBuf,
    format: OutputFormat,
    color: ColorMode,
    stages: Option<Vec<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // The report goes to stdout and the progress display to stderr, and what one
    // of them can do says nothing about the other. TERM describes the terminal
    // rather than either stream, so both are asked against the one description.
    let terminal = terminfo::Database::from_env().ok();
    let report = OutputStream::detect(color, &std::io::stdout(), terminal.as_ref());
    let display = OutputStream::detect(color, &std::io::stderr(), terminal.as_ref());

    let repo_path = current_git_toplevel()?;
    if project.uses_maintainers()
        && sashiko::maintainers::get_global_maintainers().is_none()
        && let Ok(idx) = sashiko::maintainers::MaintainersIndex::from_top_of_trunk(&repo_path)
    {
        sashiko::maintainers::init_global_maintainers(Arc::new(idx));
    }
    eprintln!("Reviewing: {}", input);
    eprintln!("Using prompts: {}", prompts.display());

    if sashiko::git_ops::is_dirty(&repo_path)
        .await
        .unwrap_or(false)
    {
        eprint_colored(display.color, Color::Yellow, "WARNING:")?;
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
        project,
        patches: std::collections::BTreeMap::new(),
        total_turns: 0,
        last_status: std::collections::BTreeMap::new(),
        terminal_width: display.size.1,
        terminal_rows: display.size.0,
        color_choice: display.color,
        reservation_capable: display.reservation_capable,
        reserved: 0,
        region_rows: 0,
        finished: false,
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
                            preliminary_stages: 0,
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

                // The first frame, drawn before any stage has started. Every
                // patch is known and queued by now, and the work between here
                // and the first stage — a provider each, a cache to open, a git
                // call per patch — is time the display would otherwise spend
                // blank, with the count it is about to report already settled.
                render_progress(&mut s);
            }
            ProgressEvent::AiReviewPreScreenStarted { patch_index } => {
                if let Some(p) = s.patches.get_mut(&patch_index) {
                    p.status = PatchStatus::PreScreening;
                    p.preliminary_stages += 1;
                    render_progress(&mut s);
                }
            }
            ProgressEvent::AiReviewPlanningStarted { patch_index } => {
                if let Some(p) = s.patches.get_mut(&patch_index) {
                    p.status = PatchStatus::Planning;
                    p.preliminary_stages += 1;
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
                // The region goes back before this prints, and the frame it was
                // holding stays: it is the review's final state, a hundred per
                // cent of the stages it ran. So this line, and the report after
                // it, start below the frame instead of inside it or over it.
                finish_progress(&mut s);
                eprintln!("Review complete");
            }
        }
    };

    if display.reservation_capable {
        release_region_on_panic();
        release_region_on_interrupt();
    }

    // Only a display that draws again has a size to be wrong about: appended
    // lines are never drawn twice.
    let resize = if display.reservation_capable {
        watch_for_resize(progress_state.clone(), std::io::stderr(), render_progress)
    } else {
        None
    };

    let result = run_git_review(
        repo_path,
        input.clone(),
        ReviewOptions {
            project,
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
    .await;

    if let Some(resize) = resize {
        // Asked to stop, and waited for. A repaint already past its await and
        // waiting on the display's lock has nowhere to be cancelled, so without
        // the wait it can take the screen again after it has been given back, and
        // leave the terminal scrolling inside a region nothing owns.
        resize.abort();
        let _ = resize.await;
    }

    // Before anything else prints, and before a failed review carries its error
    // out of here: the report is ordinary output, and the screen has to be whole
    // again for it either way.
    finish_progress(&mut progress_state.lock().unwrap());
    let result = result?;

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutputFormat::Text => {
            print_review_result(&result, &input, report.color)?;
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
    let output = sashiko::git_cmd::in_dir(&cwd)
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
    git_patch_id: Option<String>,
    policy: &sashiko::email_policy::EmailPolicyConfig,
    subsystem_mapping: &[sashiko::settings::SubsystemMapping],
) -> ProcessStatus {
    let ParsedArticle {
        group,
        article_id,
        source,
        metadata,
        patch,
        baseline,
        failed_error,
        skip_filters,
        only_filters,
        mr_url,
        mr_title,
        mr_number,
        // The caller settles the receipt, because only it knows whether the
        // whole batch made it through.
        receipt: _,
    } = article;

    let root_msg_id = resolve_root_msg_id(source, &article_id);

    // Handle ingestion failure
    if let Some(err) = failed_error {
        info!("Handling ingestion failure for {}: {}", article_id, err);
        if let Err(e) = worker_db.update_patchset_error(&root_msg_id, &err).await {
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

    // The MAINTAINERS sections this part touches, which is a different
    // question from the mailing list labels above and answered from different
    // evidence. Those come from Cc: and name lists that anyone can address;
    // these come from the files the patch actually changes and name the people
    // the kernel trusts with that code. Only these decide who may later read
    // the series' review transcripts, so they are kept apart all the way down
    // to their own table rather than merged into the labels here.
    let maintainer_sections: Vec<sashiko::db::AttributedSubsystem> = patch_opt
        .as_ref()
        .map(|p| sashiko::maintainers::sections_for_diff(&p.diff))
        .unwrap_or_default()
        .into_iter()
        .map(sashiko::db::AttributedSubsystem::from_maintainers)
        .collect();

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

    let cover_letter_id: Option<String> = if group == "git-fetch" {
        // Always use root_msg_id for git-fetch to match the placeholder ID
        Some(root_msg_id.clone())
    } else if group == "api-submit" || group.starts_with("git-import") {
        if metadata.total == 1 {
            Some(metadata.message_id.clone())
        } else {
            Some(root_msg_id.clone())
        }
    } else if metadata.index == 0 || metadata.total == 1 {
        Some(metadata.message_id.clone())
    } else {
        // git send-email points a part at its cover letter, but a series is
        // just as often posted in reply to an unrelated thread or to its own
        // previous version. Naming this series after that message would hand
        // it somebody else's identity, so a part that cannot find its own
        // cover letter names the series after itself; the database keeps the
        // lowest-numbered part's name.
        let parent_is_our_cover_letter = match metadata.in_reply_to.as_deref() {
            Some(parent) => worker_db
                .message_is_cover_letter_for(
                    parent,
                    &metadata.author,
                    metadata.total,
                    metadata.version,
                )
                .await
                .unwrap_or(false),
            None => false,
        };

        if parent_is_our_cover_letter {
            metadata.in_reply_to.clone()
        } else {
            Some(metadata.message_id.clone())
        }
    };

    if metadata.is_patch_or_cover {
        let (subject, author, total_parts, strict_author, effective_version) = if is_git_import {
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
                metadata.version,
            )
        } else if group == "git-fetch"
            && let (Some(title), Some(number)) = (&mr_title, &mr_number)
        {
            let mr_ver = worker_db
                .get_mr_version_for_commit_range(*number, &root_msg_id)
                .await
                .unwrap_or(1);
            (
                sashiko::forge::format_mr_subject(mr_url.as_deref(), *number, mr_ver, title),
                metadata.author.clone(),
                metadata.total,
                is_strict_author(source, metadata.total),
                Some(mr_ver),
            )
        } else {
            (
                metadata.subject.clone(),
                metadata.author.clone(),
                metadata.total,
                is_strict_author(source, metadata.total),
                metadata.version,
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
                cover_letter_id.as_deref(),
                metadata.message_id.as_str(),
                &subject,
                &author,
                // Use server-side timestamp (received_date) when available,
                // falling back to the email's Date: header. This prevents
                // stale mbox timestamps from skewing queue ordering.
                metadata.received_date.unwrap_or(metadata.date),
                total_parts,
                PARSER_VERSION,
                &metadata.to,
                &metadata.cc,
                effective_version,
                metadata.index,
                baseline_id,
                strict_author,
                skip_filters.as_ref(),
                only_filters.as_ref(),
            )
            .await
        {
            Ok(Some(patchset_id)) => {
                if mr_url.is_some() || mr_title.is_some() || mr_number.is_some() {
                    let slug = if let (Some(url), Some(num)) = (&mr_url, mr_number) {
                        sashiko::forge::extract_repo_name_from_mr_url(url)
                            .map(|repo| format!("{}-{}", repo, num))
                    } else {
                        None
                    };
                    if let Err(e) = worker_db
                        .update_patchset_mr_metadata(
                            patchset_id,
                            mr_url.as_deref(),
                            mr_title.as_deref(),
                            mr_number,
                            slug.as_deref(),
                        )
                        .await
                    {
                        error!(
                            "Failed to update MR metadata for patchset {}: {}",
                            patchset_id, e
                        );
                    }
                }

                #[allow(clippy::collapsible_if)]
                if let Some(until) = embargo_until {
                    if let Err(e) = worker_db
                        .set_patchset_embargo_until_if_non_terminal(patchset_id, until)
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

                // Added rather than replaced: a series arrives one part at a
                // time, and the attribution is the union of what every part
                // touches. A failure here is logged and not fatal, because it
                // can only leave the series attributed to fewer sections than
                // it should be, which withholds transcripts rather than
                // disclosing them. Dropping the whole part instead would lose
                // the patch itself over a permissions detail.
                if let Err(e) = worker_db
                    .add_patchset_maintainer_sections(patchset_id, &maintainer_sections)
                    .await
                {
                    error!(
                        "Failed to attribute patchset {} to MAINTAINERS sections: {}",
                        patchset_id, e
                    );
                }

                if let Some(patch) = patch_opt {
                    match worker_db
                        .create_patch_with_git_patch_id(
                            patchset_id,
                            &patch.message_id,
                            patch.part_index,
                            &patch.diff,
                            git_patch_id.as_deref(),
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

fn resolve_root_msg_id(source: MessageSource, article_id: &str) -> String {
    match source {
        MessageSource::Nntp
        | MessageSource::ApiFetchThread
        | MessageSource::GitArchive
        | MessageSource::ApiInject => article_id.to_string(),
        MessageSource::GitFetch | MessageSource::GitImport => {
            format!("{}@sashiko.local", article_id)
        }
    }
}

fn is_strict_author(source: MessageSource, total_parts: u32) -> bool {
    match source {
        MessageSource::GitImport | MessageSource::GitArchive => false,
        MessageSource::GitFetch if total_parts > 1 => false,
        MessageSource::ApiInject if total_parts > 1 => false,
        MessageSource::ApiFetchThread if total_parts > 1 => false,
        _ => true,
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

fn should_start_nntp_ingestor(settings: &Settings) -> bool {
    settings.has_nntp_config() && !(settings.forge.enabled && settings.forge.disable_nntp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use termcolor::Buffer;

    /// The terminfo entry for `term`, where this machine has one.
    ///
    /// Sashiko builds and tests in environments that may have no terminfo
    /// database at all, so we fall back to empty instead of failing.
    fn terminfo_for(term: &str) -> Option<terminfo::Database> {
        terminfo::Database::from_name(term).ok()
    }

    fn progress_state(display: OutputStream) -> ProgressState {
        ProgressState {
            project: ProjectId::Linux,
            patches: std::collections::BTreeMap::new(),
            last_status: std::collections::BTreeMap::new(),
            total_turns: 0,
            terminal_width: display.size.1,
            terminal_rows: display.size.0,
            color_choice: display.color,
            reservation_capable: display.reservation_capable,
            reserved: 0,
            region_rows: 0,
            finished: false,
        }
    }

    fn patch_state(status: PatchStatus) -> PatchState {
        PatchState {
            index: 1,
            subject: "a patch".to_string(),
            status,
            planned_stages: Vec::new(),
            active_stages: std::collections::BTreeSet::new(),
            completed_stages: 0,
            active_stage_turns: std::collections::HashMap::new(),
            preliminary_stages: 0,
        }
    }

    /// A stream the display may keep lines of its own on, with no color: what a
    /// vt100 answers, and what keeps a painted frame out of the test output.
    fn reserving(size: (usize, usize)) -> OutputStream {
        OutputStream {
            color: ColorChoice::Never,
            reservation_capable: true,
            size,
        }
    }

    /// A stream the display is told about each change on.
    fn appending(size: (usize, usize)) -> OutputStream {
        OutputStream {
            color: ColorChoice::Never,
            reservation_capable: false,
            size,
        }
    }

    /// Sets a pty's window size, the way a terminal emulator does for the pty
    /// it owns.
    fn resize(pty: &std::fs::File, rows: u16, cols: u16) {
        rustix::termios::tcsetwinsize(
            pty,
            rustix::termios::Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .expect("set the pty window size");
    }

    #[tokio::test]
    async fn test_a_resize_is_followed_while_the_review_runs() {
        let pty = std::fs::File::open("/dev/ptmx").expect("open /dev/ptmx");
        resize(&pty, 24, 80);

        let state = Arc::new(std::sync::Mutex::new(progress_state(reserving((24, 80)))));

        // Counted rather than painted: a frame painted here would be written to
        // the terminal running the test, and escape sequences left there outlive
        // the run.
        let painted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = painted.clone();
        let watching = watch_for_resize(
            state.clone(),
            pty.try_clone().expect("dup the pty"),
            move |_| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .expect("follow a terminal that has a size");

        // What a terminal emulator does: change the window, then say so.
        resize(&pty, 24, 132);
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::WINCH)
            .expect("signal ourselves");

        // The signal is delivered to the task, so give it a chance to run.
        for _ in 0..200 {
            if state.lock().unwrap().terminal_width == 132 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            state.lock().unwrap().terminal_width,
            132,
            "the display kept drawing to the old width"
        );

        // And it is drawn again at that width, rather than waiting for whatever
        // the review does next.
        assert_eq!(painted.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A signal that leaves the width where it is redraws nothing. SIGWINCH
        // reaches the whole process, so a listener that repainted for every one
        // of them would repaint for windows that are not this stream.
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::WINCH)
            .expect("signal ourselves");
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(painted.load(std::sync::atomic::Ordering::SeqCst), 1);

        watching.abort();
    }

    #[tokio::test]
    async fn test_a_resize_between_starting_and_listening_is_not_missed() {
        // The display starts from the size it was given, and the window changes
        // before anything is listening for the signal that says so.
        let pty = std::fs::File::open("/dev/ptmx").expect("open /dev/ptmx");
        resize(&pty, 24, 80);
        let state = Arc::new(std::sync::Mutex::new(progress_state(reserving((24, 80)))));
        resize(&pty, 24, 132);

        // No signal is raised here: that one is gone. Registering asks for the
        // size itself, so the display is drawing to the window as it now is.
        let painted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = painted.clone();
        let watching = watch_for_resize(
            state.clone(),
            pty.try_clone().expect("dup the pty"),
            move |_| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .expect("follow a terminal that has a size");

        assert_eq!(state.lock().unwrap().terminal_width, 132);
        assert_eq!(painted.load(std::sync::atomic::Ordering::SeqCst), 1);

        watching.abort();
    }

    #[tokio::test]
    async fn test_a_stream_with_no_window_is_not_followed() {
        // Nothing to follow: no size to ask for, and no terminal to resize.
        let redirected = std::fs::File::open("/dev/null").expect("open /dev/null");
        let state = Arc::new(std::sync::Mutex::new(progress_state(reserving((24, 80)))));

        assert!(watch_for_resize(state, redirected, render_progress).is_none());
    }

    #[test]
    fn test_window_size_asks_the_stream_it_is_given() {
        // Nothing to report from a stream that is no terminal.
        let redirected = std::fs::File::open("/dev/null").expect("open /dev/null");
        assert_eq!(window_size(&redirected), None);

        // Nor from a terminal whose size has never been set: a fresh pty master
        // answers isatty yes and zeroes for its window.
        let pty = std::fs::File::open("/dev/ptmx").expect("open /dev/ptmx");
        assert!(pty.is_terminal());
        assert_eq!(window_size(&pty), None);

        // Give that pty a size and the ioctl reads it back, rows first.
        rustix::termios::tcsetwinsize(
            &pty,
            rustix::termios::Winsize {
                ws_row: 42,
                ws_col: 118,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .expect("set the pty window size");
        assert_eq!(window_size(&pty), Some((42, 118)));
    }

    #[test]
    fn test_each_stream_is_asked_about_itself() {
        // std::io::IsTerminal cannot be implemented outside std, so these are
        // real descriptors: a pty master answers yes, /dev/null answers no.
        let pty = std::fs::File::open("/dev/ptmx").expect("open /dev/ptmx");
        let redirected = std::fs::File::open("/dev/null").expect("open /dev/null");
        let xterm = terminfo_for("xterm");

        // "auto" is the only mode that asks a stream anything, and it asks the
        // one it was handed: a redirected stdout must not silence stderr. Said of
        // a terminal that has color, where this machine can describe one.
        if let Some(xterm) = xterm.as_ref() {
            assert_eq!(
                OutputStream::detect(ColorMode::Auto, &pty, Some(xterm)).color,
                ColorChoice::Auto
            );
        }

        // A stream that is no terminal gets no color from a terminal that has
        // it: TERM describes the screen someone may be looking at, not the file
        // this stream was redirected to.
        assert_eq!(
            OutputStream::detect(ColorMode::Auto, &redirected, xterm.as_ref()).color,
            ColorChoice::Never
        );

        // "always" and "never" are answers about the run, so neither the stream
        // nor the terminal gets a say. This is what makes "--color always" work
        // under a Docker pipe, where the escapes reach the terminal but isatty
        // says no.
        for stream in [&pty, &redirected] {
            assert_eq!(
                OutputStream::detect(ColorMode::Always, stream, None).color,
                ColorChoice::Always
            );
            assert_eq!(
                OutputStream::detect(ColorMode::Never, stream, xterm.as_ref()).color,
                ColorChoice::Never
            );
        }
    }

    #[test]
    fn test_color_is_the_terminals_answer_and_not_isattys() {
        let pty = std::fs::File::open("/dev/ptmx").expect("open /dev/ptmx");

        // A terminal with no color to set. isatty cannot tell this apart from an
        // xterm, and terminfo can.
        assert!(pty.is_terminal());
        if let Some(vt100) = terminfo_for("vt100") {
            assert_eq!(
                OutputStream::detect(ColorMode::Auto, &pty, Some(&vt100)).color,
                ColorChoice::Never
            );
        }

        // Which terminals have color, straight from the database, for those of
        // them this machine has: a build environment carrying no terminfo at all
        // is a thing this has to work on, so it is a thing the test runs on.
        for (term, color) in [
            ("xterm", true),
            ("xterm-256color", true),
            ("linux", true),
            ("screen", true),
            ("ansi", true),
            ("vt100", false),
            ("xterm-mono", false),
            ("dumb", false),
        ] {
            if let Some(info) = terminfo_for(term) {
                assert_eq!(color_capable(&info), color, "{term}");
            }
        }
    }

    #[test]
    fn test_a_reserved_frame_addresses_its_own_lines() {
        let mut state = progress_state(reserving((24, 100)));
        state.patches.insert(1, patch_state(PatchStatus::Queued));

        // Two lines wanted, so scrolling is confined to the 22 above them and
        // the frame is written at rows 23 and 24. Nothing is erased by walking
        // the cursor: each line is addressed and cleared where it stands.
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("paint a frame");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert_eq!(state.reserved, 2);
        assert!(
            painted.contains("\x1b[1;22r"),
            "no scroll region: {painted:?}"
        );
        assert!(painted.contains("\x1b[23;1H\x1b[2K"), "{painted:?}");
        assert!(painted.contains("\x1b[24;1H\x1b[2K"), "{painted:?}");
        assert!(
            !painted.contains("\x1b[F"),
            "walked the cursor: {painted:?}"
        );

        // The cursor ends on the last line of the region, which is where
        // ordinary output carries on from. Nothing is saved or restored.
        assert!(painted.ends_with("\x1b[22;1H"), "{painted:?}");
        assert!(!painted.contains("\x1b7"), "saved the cursor: {painted:?}");
        assert!(
            !painted.contains("\x1b8"),
            "restored the cursor: {painted:?}"
        );

        // Every frame asserts the region, not only the first: anything can hand
        // the margins back without saying so.
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("paint a frame");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert!(painted.contains("\x1b[1;22r"), "{painted:?}");
        assert!(painted.contains("\x1b[23;1H\x1b[2K"), "{painted:?}");

        // A patch more is a line more, so the region is set again.
        state.patches.insert(2, patch_state(PatchStatus::Reviewing));
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("paint a frame");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert_eq!(state.reserved, 3);
        assert!(painted.contains("\x1b[1;21r"), "{painted:?}");
    }

    #[test]
    fn test_room_is_scrolled_for_at_the_foot_of_the_screen() {
        let mut state = progress_state(reserving((24, 100)));
        state.patches.insert(1, patch_state(PatchStatus::Queued));

        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("paint a frame");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");

        // Two lines scrolled for at row 24, the foot of the screen, where a
        // newline can only scroll. The region is handed back first, so those
        // newlines scroll all of the screen rather than the part above a region
        // already held, and the frame that follows is addressed outright.
        let addressed = painted.find("\x1b[23;1H").expect("a frame follows");
        assert_eq!(&painted[..addressed], "\x1b[r\x1b[24;1H\n\n\x1b[1;22r");

        // A patch more wants a line more, so one line more is scrolled for and
        // the region moves up with it.
        state.patches.insert(2, patch_state(PatchStatus::Reviewing));
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("paint a frame");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        let addressed = painted.find("\x1b[22;1H\x1b[2K").expect("a frame follows");
        assert_eq!(&painted[..addressed], "\x1b[r\x1b[24;1H\n\x1b[1;21r");
        assert!(painted.ends_with("\x1b[21;1H"), "{painted:?}");
    }

    #[test]
    fn test_the_last_frame_is_left_standing_when_the_review_ends() {
        let mut state = progress_state(reserving((24, 100)));
        state.patches.insert(1, patch_state(PatchStatus::Queued));
        paint_progress(&mut state, &mut Buffer::no_color()).expect("paint a frame");
        assert_eq!((state.reserved, state.region_rows), (2, 24));

        let mut end = Buffer::no_color();
        release_progress_region(&mut state, &mut end, RegionExit::Kept)
            .expect("give the screen back");
        let written = String::from_utf8(end.into_inner()).expect("utf-8");

        // The margins go back, the cursor goes to the last line the display held,
        // and one newline scrolls the screen so the line after it is free. What
        // the review prints next — "Review complete", the report — lands there,
        // leaving the finished frame on screen above it.
        assert_eq!(written, "\x1b[r\x1b[24;1H\n");
        assert_eq!(state.reserved, 0);

        // Nothing erased and nothing restored: erasing would take the frame with
        // it, and a restore would put the cursor back among its rows, which is
        // how the report came to print over them.
        assert!(!written.contains("\x1b[2K"), "{written:?}");
        assert!(!written.contains("\x1b7"), "{written:?}");
        assert!(!written.contains("\x1b8"), "{written:?}");

        // Said again on the way out, as the review does: a display holding
        // nothing writes nothing. Handing back margins that are already back
        // homes the cursor, and the report would print from the top of the screen
        // over the log that is there.
        let mut end = Buffer::no_color();
        release_progress_region(&mut state, &mut end, RegionExit::Kept).expect("say nothing");
        assert!(end.into_inner().is_empty());
    }

    #[test]
    fn test_nothing_paints_once_the_display_has_finished() {
        let mut state = progress_state(reserving((24, 100)));
        state.patches.insert(1, patch_state(PatchStatus::Queued));
        paint_progress(&mut state, &mut Buffer::no_color()).expect("paint a frame");

        let mut end = Buffer::no_color();
        release_progress_region(&mut state, &mut end, RegionExit::Kept)
            .expect("give the screen back");
        assert!(state.finished);

        // The report prints from here, and the resize watcher is still running for
        // a moment yet: a repaint in that moment would take a region back and draw
        // across it. So a frame asked for now writes nothing at all.
        let mut late = Buffer::no_color();
        paint_progress(&mut state, &mut late).expect("say nothing");
        assert!(late.into_inner().is_empty());
        assert_eq!(state.reserved, 0);
    }

    #[test]
    fn test_a_screen_that_has_shrunk_is_not_cleared_whole() {
        let mut state = progress_state(reserving((24, 100)));
        state.patches.insert(1, patch_state(PatchStatus::Queued));
        paint_progress(&mut state, &mut Buffer::no_color()).expect("paint a frame");
        assert_eq!((state.reserved, state.region_rows), (2, 24));

        // The screen is now shorter than the display was holding. Clearing the
        // last two rows of it would clear rows that were never the display's, and
        // where it has shrunk below the count held, all of them. It knows where
        // those lines were only while the screen is the height they were placed
        // against, so here it clears nothing and hands the margins back.
        state.terminal_rows = 2;
        let mut end = Buffer::no_color();
        release_progress_region(&mut state, &mut end, RegionExit::Erased)
            .expect("give the screen back");
        let written = String::from_utf8(end.into_inner()).expect("utf-8");
        assert_eq!(written, "\x1b7\x1b[r\x1b8");
        assert_eq!(state.reserved, 0);
    }

    #[test]
    fn test_a_resize_moves_the_region_with_the_foot_of_the_screen() {
        let mut state = progress_state(reserving((24, 100)));
        state.patches.insert(1, patch_state(PatchStatus::Queued));
        paint_progress(&mut state, &mut Buffer::no_color()).expect("paint a frame");
        assert_eq!((state.reserved, state.region_rows), (2, 24));

        // A shorter screen puts those lines somewhere else, and the cursor ends
        // on the last line of the region as it now is. The screen shrinking under
        // a display that put the cursor back where the log had left it is how the
        // cursor came to be stranded below the bottom margin, with every record
        // after it rewriting the same row and no resize able to help: the frame
        // that followed saved that row and restored it again.
        state.terminal_rows = 12;
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("paint a frame");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert_eq!((state.reserved, state.region_rows), (2, 12));
        assert!(painted.contains("\x1b[1;10r"), "{painted:?}");
        assert!(painted.contains("\x1b[11;1H\x1b[2K"), "{painted:?}");
        // Twelve rows less the two held, so the log carries on from inside the
        // margins rather than below them.
        assert!(painted.ends_with("\x1b[10;1H"), "{painted:?}");
    }

    #[test]
    fn test_the_screen_is_given_back_whole() {
        let mut state = progress_state(reserving((24, 100)));
        state.patches.insert(1, patch_state(PatchStatus::Queued));
        paint_progress(&mut state, &mut Buffer::no_color()).expect("paint a frame");
        assert_eq!(state.reserved, 2);

        // A region left set is what outlives the process, so it is given back
        // whatever the display thinks it is holding.
        let mut end = Buffer::no_color();
        release_progress_region(&mut state, &mut end, RegionExit::Erased)
            .expect("give the screen back");
        let written = String::from_utf8(end.into_inner()).expect("utf-8");
        assert_eq!(state.reserved, 0);

        // Standing down mid-review: the two lines it held are cleared, scrolling
        // goes back to the whole screen, and the cursor is left where the output
        // had reached, since the lines it appends from here carry on from there.
        assert_eq!(
            written,
            "\x1b7\x1b[23;1H\x1b[2K\x1b[24;1H\x1b[2K\x1b[r\x1b8"
        );

        // Said again with nothing held, since a region left set outlives the
        // process and a resize can leave the count behind.
        let mut end = Buffer::no_color();
        release_progress_region(&mut state, &mut end, RegionExit::Erased).expect("say so again");
        assert_eq!(
            String::from_utf8(end.into_inner()).expect("utf-8"),
            "\x1b7\x1b[r\x1b8"
        );

        // A display that never took a region has none to give back.
        let mut state = progress_state(appending((24, 100)));
        let mut end = Buffer::no_color();
        release_progress_region(&mut state, &mut end, RegionExit::Erased).expect("nothing to do");
        assert!(end.into_inner().is_empty());
    }

    #[test]
    fn test_a_display_that_wants_most_of_the_screen_does_not_get_it() {
        // Three quarters is the most it may have, so eighteen lines of twenty
        // four: seventeen patches and the overall bar.
        assert!(worth_reserving(18, 24));
        assert!(!worth_reserving(19, 24));
        assert!(worth_reserving(3, 4));
        assert!(!worth_reserving(4, 4));

        let mut state = progress_state(reserving((24, 100)));
        for idx in 1..=17 {
            state.patches.insert(idx, patch_state(PatchStatus::Queued));
        }
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("paint a frame");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert_eq!(state.reserved, 18);
        assert!(painted.contains("\x1b[1;6r"), "{painted:?}");

        // One patch more and the display is not worth the screen. The region
        // goes back as it changes over, so what scrolled only inside it scrolls
        // everywhere again, and each change is appended from there on.
        state.patches.insert(18, patch_state(PatchStatus::Queued));
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("append instead");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert_eq!(state.reserved, 0);

        // The lines it held are cleared and the region handed back, and what it
        // says from there on is text: nothing addresses a row again.
        let (given_back, appended) = painted
            .split_once("\x1b[r\x1b8")
            .expect("the region goes back");
        assert!(given_back.contains("\x1b[2K"), "{given_back:?}");
        assert!(
            !appended.contains('\x1b'),
            "still addressing lines: {appended:?}"
        );
        assert!(
            appended.contains("[Patch 18] a patch | Queued\n"),
            "{appended:?}"
        );

        // And once it has changed over, it says so once: the reset is not
        // repeated with every line it appends afterwards.
        state.patches.insert(19, patch_state(PatchStatus::Queued));
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("append instead");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert!(!painted.contains("\x1b"), "said it again: {painted:?}");
        assert!(painted.contains("[Patch 19] a patch | Queued\n"));

        // A screen too small for any of it never takes a region to begin with.
        let mut state = progress_state(reserving((4, 100)));
        for idx in 1..=3 {
            state.patches.insert(idx, patch_state(PatchStatus::Queued));
        }
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("append instead");
        assert_eq!(state.reserved, 0);
        assert!(
            String::from_utf8(frame.into_inner())
                .expect("utf-8")
                .contains("[Patch 1] a patch | Queued")
        );
    }

    #[test]
    fn test_a_terminal_with_no_scroll_region_gets_appended_lines() {
        let mut state = progress_state(appending((24, 80)));
        state.patches.insert(1, patch_state(PatchStatus::Queued));

        // Nothing is erased and no escape sequence is written, so the output
        // survives a terminal that can keep the display no lines of its own, and
        // a file with no terminal behind it at all.
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("append a line");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert_eq!(painted, "      [Patch 1] a patch | Queued\n");
        assert_eq!(
            state.last_status.get(&1).map(String::as_str),
            Some("Queued")
        );

        // A status that has not changed is not said again: the appending display
        // speaks only when something does change.
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("append nothing");
        assert!(frame.into_inner().is_empty());

        state.patches.insert(1, patch_state(PatchStatus::Finished));
        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("append a line");
        let painted = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert_eq!(painted, "      [Patch 1] a patch | Finished\n");
    }

    #[test]
    fn test_how_the_display_draws_is_the_terminals_answer_too() {
        let pty = std::fs::File::open("/dev/ptmx").expect("open /dev/ptmx");
        resize(&pty, 24, 80);
        let keeps_lines = |info: &terminfo::Database| {
            OutputStream::detect(ColorMode::Auto, &pty, Some(info)).reservation_capable
        };

        // Terminals that can keep the display lines of their own, for those of
        // them this machine has an entry for.
        for term in ["xterm", "linux", "screen", "vt100"] {
            if let Some(info) = terminfo_for(term) {
                assert!(keeps_lines(&info), "{term}");
            }
        }

        // TERM=ansi can be drawn over and has no scroll region, which is the
        // majority answer across the installed database: it is told about each
        // change instead, as a dumb terminal is.
        for term in ["ansi", "dumb"] {
            if let Some(info) = terminfo_for(term) {
                assert!(!keeps_lines(&info), "{term}");
                assert!(!reservation_capable(&info), "{term}");
            }
        }

        // As is a TERM terminfo does not know: a screen is not carved up on a
        // guess.
        assert!(!OutputStream::detect(ColorMode::Auto, &pty, None).reservation_capable);

        // A vt100 keeps its lines and still has no color: the two answers are
        // separate, and neither is read off the other.
        if let Some(vt100) = terminfo_for("vt100") {
            assert_eq!(
                OutputStream::detect(ColorMode::Auto, &pty, Some(&vt100)).color,
                ColorChoice::Never
            );
        }

        // Nor does a capable terminal keep lines where it reports no window: the
        // foot of the screen is not known, and lines placed against the 24x80
        // fallback would land in the middle of a taller one, over whatever was
        // there. A fresh pty master is exactly that terminal.
        let sizeless = std::fs::File::open("/dev/ptmx").expect("open /dev/ptmx");
        assert_eq!(window_size(&sizeless), None);
        if let Some(xterm) = terminfo_for("xterm") {
            let display = OutputStream::detect(ColorMode::Auto, &sizeless, Some(&xterm));
            assert!(!display.reservation_capable);
            assert_eq!(display.size, (24, 80));
            assert_eq!(display.color, ColorChoice::Auto);
        }
    }

    #[test]
    fn test_an_appended_line_does_not_turn_on_hidden_turn_counts() {
        // Two stages running, and the appending display has said so once.
        let mut state = progress_state(appending((24, 80)));
        let mut p = patch_state(PatchStatus::Reviewing);
        p.active_stages.insert("locking".to_string());
        p.active_stages.insert("security".to_string());
        p.active_stage_turns.insert("locking".to_string(), 2);
        state.patches.insert(1, p.clone());

        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("append a line");
        let first = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert!(!first.is_empty(), "said nothing at all");

        // The other stage takes more turns than the first. Nothing the reader can
        // see has changed: the same two stages are running, and the counter that
        // moved is not in the line.
        p.active_stage_turns.insert("security".to_string(), 5);
        state.patches.insert(1, p);

        let mut frame = Buffer::no_color();
        paint_progress(&mut state, &mut frame).expect("append nothing");
        let second = String::from_utf8(frame.into_inner()).expect("utf-8");
        assert!(
            second.is_empty(),
            "said it again over a hidden turn count: {first:?} then {second:?}"
        );
    }

    #[test]
    fn test_status_label_drops_the_turn_counter_when_not_repainting() {
        let mut p = patch_state(PatchStatus::Reviewing);
        p.active_stages.insert("locking".to_string());
        p.active_stage_turns.insert("locking".to_string(), 3);

        assert_eq!(
            status_label(ProjectId::Linux, &p, true),
            "Locking & Sync (turn 3)"
        );
        assert_eq!(status_label(ProjectId::Linux, &p, false), "Locking & Sync");

        // A repainting frame still leads with the busiest stage, since the counter
        // it shows says why that one leads. Security overtakes locking here.
        p.active_stages.insert("security".to_string());
        p.active_stage_turns.insert("security".to_string(), 9);
        assert_eq!(
            status_label(ProjectId::Linux, &p, true),
            "Security Audit (turn 9) (+1 stages)"
        );

        // Without the counter the set's own order decides, so the line holds still
        // while the counters move underneath it.
        assert_eq!(
            status_label(ProjectId::Linux, &p, false),
            "Locking & Sync (+1 stages)"
        );
        p.active_stage_turns.insert("locking".to_string(), 40);
        assert_eq!(
            status_label(ProjectId::Linux, &p, false),
            "Locking & Sync (+1 stages)"
        );

        // It changes when the set changes, which is what the line is for.
        p.active_stages.remove("locking");
        assert_eq!(status_label(ProjectId::Linux, &p, false), "Security Audit");
    }

    #[test]
    fn test_the_stage_total_counts_only_the_preliminaries_that_ran() {
        let every = sashiko::workflows::default_stage_count(ProjectId::Linux);
        let planned = |names: &[&str], preliminary| PatchState {
            planned_stages: names.iter().map(|n| n.to_string()).collect(),
            preliminary_stages: preliminary,
            ..patch_state(PatchStatus::Reviewing)
        };
        let total = |p: &PatchState| patch_stage_total(ProjectId::Linux, p);

        // A full review: the pre-screen and the planner both ran, and the fan-out
        // has not resolved yet, so assume the rest of them.
        assert_eq!(total(&planned(&[], 2)), 2 + every);

        // --stages locking: neither preliminary ran, and the plan is what was
        // asked for plus the consolidation stages that always follow.
        assert_eq!(total(&planned(&["locking"], 0)), 1);

        // A patch that left by an early exit ran fewer stages than it planned,
        // and the ones it skipped must leave the total rather than strand the bar
        // short of the work it did.
        let mut done = planned(&["locking"], 2);
        done.status = PatchStatus::Finished;
        done.completed_stages = 4;
        assert_eq!(total(&done), 4);
    }

    #[test]
    fn test_progress_metrics_clamp_completed_stages_to_total() {
        assert_eq!(calculate_progress_metrics(1, 5, 20), (1, 100, 20));
        assert_eq!(calculate_progress_metrics(5, 2, 20), (2, 40, 8));
        assert_eq!(calculate_progress_metrics(0, 5, 20), (0, 0, 0));
    }

    #[test]
    fn test_project_defaults_to_linux_when_nothing_names_one() {
        // An existing deployment passes no flag and has no kind in its
        // settings, and must keep getting the kernel behaviour.
        assert_eq!(effective_project(None, None), Ok(ProjectId::Linux));
    }

    #[test]
    fn test_project_flag_and_settings_are_each_enough_alone() {
        assert_eq!(
            effective_project(Some(ProjectId::Sashiko), None),
            Ok(ProjectId::Sashiko)
        );
        assert_eq!(
            effective_project(None, Some(ProjectId::Sashiko)),
            Ok(ProjectId::Sashiko)
        );
    }

    #[test]
    fn test_project_flag_wins_when_the_settings_agree() {
        assert_eq!(
            effective_project(Some(ProjectId::Linux), Some(ProjectId::Linux)),
            Ok(ProjectId::Linux)
        );
    }

    #[test]
    fn test_disagreeing_project_is_refused_rather_than_resolved() {
        // The settings file is what names the database, the git tree and the
        // port. Letting the flag win here would run one project against
        // another one's state, so neither side wins and the run stops.
        let err = effective_project(Some(ProjectId::Sashiko), Some(ProjectId::Linux)).unwrap_err();
        assert!(err.contains("sashiko"), "{err}");
        assert!(err.contains("linux"), "{err}");
    }

    #[test]
    fn test_project_is_accepted_wherever_it_is_typed() {
        // Global, so it parses before a subcommand, after one, and with none.
        let bare = Cli::try_parse_from(["sashiko", "--project", "sashiko"]).unwrap();
        assert_eq!(bare.project, Some(ProjectId::Sashiko));

        let before = Cli::try_parse_from(["sashiko", "--project", "sashiko", "review"]).unwrap();
        assert_eq!(before.project, Some(ProjectId::Sashiko));

        let after = Cli::try_parse_from(["sashiko", "review", "--project", "sashiko"]).unwrap();
        assert_eq!(after.project, Some(ProjectId::Sashiko));

        let absent = Cli::try_parse_from(["sashiko", "review"]).unwrap();
        assert_eq!(absent.project, None);
    }

    #[test]
    fn test_unknown_project_is_rejected_by_the_parser() {
        assert!(Cli::try_parse_from(["sashiko", "--project", "freebsd"]).is_err());
    }

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
        assert!(should_start_nntp_ingestor(&settings));
    }

    #[test]
    fn test_nntp_ingestor_disabled_by_default_with_forge() {
        let mut settings = Settings::new().unwrap();
        settings.forge.enabled = true;
        settings.forge.disable_nntp = true; // This is the default
        assert!(!should_start_nntp_ingestor(&settings));
    }

    #[test]
    fn test_nntp_ingestor_disabled_when_nntp_config_omitted() {
        let mut settings = Settings::new().unwrap();
        settings.nntp.server = String::new();
        settings.forge.enabled = false;
        assert!(!should_start_nntp_ingestor(&settings));
    }

    #[test]
    fn test_resolve_root_msg_id() {
        assert_eq!(
            resolve_root_msg_id(MessageSource::Nntp, "foo@bar.com"),
            "foo@bar.com"
        );
        assert_eq!(
            resolve_root_msg_id(MessageSource::ApiFetchThread, "foo@bar.com"),
            "foo@bar.com"
        );
        assert_eq!(
            resolve_root_msg_id(MessageSource::GitArchive, "foo@bar.com"),
            "foo@bar.com"
        );
        assert_eq!(
            resolve_root_msg_id(MessageSource::ApiInject, "sashiko-123"),
            "sashiko-123"
        );
        assert_eq!(
            resolve_root_msg_id(MessageSource::GitFetch, "abc123_sha"),
            "abc123_sha@sashiko.local"
        );
        assert_eq!(
            resolve_root_msg_id(MessageSource::GitImport, "range_a_b"),
            "range_a_b@sashiko.local"
        );
    }

    #[test]
    fn test_is_strict_author() {
        assert!(is_strict_author(MessageSource::Nntp, 1));
        assert!(is_strict_author(MessageSource::Nntp, 6));
        assert!(!is_strict_author(MessageSource::ApiFetchThread, 6)); // Lenient for series
        assert!(!is_strict_author(MessageSource::GitFetch, 6)); // Lenient for series
        assert!(is_strict_author(MessageSource::GitFetch, 1)); // Strict for singleton

        assert!(!is_strict_author(MessageSource::GitImport, 6));
        assert!(!is_strict_author(MessageSource::GitArchive, 6));

        assert!(is_strict_author(MessageSource::ApiInject, 1)); // Strict for singleton
        assert!(!is_strict_author(MessageSource::ApiInject, 6)); // Lenient for series
    }

    #[test]
    fn test_format_mr_subject() {
        assert_eq!(
            sashiko::forge::format_mr_subject(
                Some("https://github.com/sashiko-dev/sashiko/pull/502"),
                502,
                1,
                "Fix PR display prefix"
            ),
            "#502: Fix PR display prefix"
        );
        assert_eq!(
            sashiko::forge::format_mr_subject(None, 502, 1, "Fix PR display prefix"),
            "#502: Fix PR display prefix"
        );
        assert_eq!(
            sashiko::forge::format_mr_subject(
                Some("https://gitlab.com/example/repo/-/merge_requests/502"),
                502,
                1,
                "Fix MR display prefix"
            ),
            "!502: Fix MR display prefix"
        );
    }
}
