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

use clap::Parser;
use sashiko::db::Database;
use sashiko::events::{Event, ParsedArticle};
use sashiko::ingestor::Ingestor;
use sashiko::reviewer::Reviewer;
use sashiko::settings::Settings;
use std::io::IsTerminal;
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

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
    let log_level = if cli.debug {
        "info"
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
        .with_writer(sashiko::logging::IgnoreBrokenPipe(std::io::stdout))
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
