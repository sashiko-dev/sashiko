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

use clap::{Parser, Subcommand};
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

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Inspect,
    /// Restart failed reviews
    RestartFailed,
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

    if let Some(Commands::Inspect) = cli.command {
        return sashiko::inspector::run_inspection(db)
            .await
            .map_err(|e| e.into());
    }

    if let Some(Commands::RestartFailed) = cli.command {
        let count = db.restart_failed_reviews().await?;
        println!("Successfully restarted {} failed reviews.", count);
        return Ok(());
    }

    // Create internal task queues
    // raw_tx -> Parser -> parsed_tx -> DB Worker
    let (raw_tx, mut raw_rx) = mpsc::channel::<Event>(1000);
    let (parsed_tx, mut parsed_rx) = mpsc::channel::<ParsedArticle>(1000);

    // Initialize FetchAgent
    let repo_path = std::path::PathBuf::from(&settings.git.repository_path);
    let (fetch_agent, fetch_tx) = sashiko::fetcher::FetchAgent::new(repo_path, raw_tx.clone());

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
    let _db_worker_handle = tokio::spawn(async move {
        info!("DB Worker started");

        let mut buffer = Vec::with_capacity(100);
        let mut total_processed = 0;
        let mut total_ingested = 0;
        let mut total_errors = 0;

        loop {
            let count = parsed_rx.recv_many(&mut buffer, 100).await;
            if count == 0 {
                break;
            }

            // info!("Processing batch of {} parsed articles", count); // Too verbose
            if let Err(e) = worker_db.begin_transaction().await {
                error!("Failed to begin transaction: {}", e);
                total_errors += count; // Assume all failed if the transaction fails.
                continue;
            }

            let policy = sashiko::email_policy::EmailPolicyConfig::load("email_policy.toml")
                .unwrap_or_default();

            for article in buffer.drain(..) {
                match process_parsed_article(&worker_db, article, &policy).await {
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

            if let Err(e) = worker_db.commit_transaction().await {
                error!("Failed to commit transaction: {}", e);
                // If commit fails, technically we lost the batch work in DB, but counters are already updated.
                // For simple stats, this is acceptable, but ideally we'd track "pending" stats.
                // Keeping it simple.
            }
        }

        // Final stats
        info!(
            "Ingestion Complete: {} processed ({} ingested, {} errors)",
            total_processed, total_ingested, total_errors
        );
    });

    // Start Ingestor (feeds raw_tx)
    let ingestor = Ingestor::new(
        settings.clone(),
        db.clone(),
        raw_tx.clone(),
        cli.download,
        cli.track,
    );
    let ingestor_handle = tokio::spawn(async move {
        if let Err(e) = ingestor.run().await {
            error!("Ingestor fatal error: {}", e);
        }
    });

    // Start Web API
    let api_settings = settings.server.clone();
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

    // Start Reviewer Service
    let reviewer = Reviewer::new(db.clone(), settings.clone());
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

    // 2. Create Message
    if let Err(e) = worker_db
        .create_message(
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
        )
        .await
    {
        error!("Failed to create message: {}", e);
        return ProcessStatus::Error;
    }

    // Subsystem Identification and Linking
    let mut subsystems = identify_subsystems(&metadata.to, &metadata.cc);
    if group.starts_with("git-import") {
        subsystems.push(("from git".to_string(), "git-import".to_string()));
    }

    let mut subsystem_ids = Vec::new();
    for (name, email) in subsystems {
        match worker_db.ensure_subsystem(&name, &email).await {
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
    let cover_letter_id = if group == "git-fetch" || group == "api-submit" {
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
        } else {
            (
                metadata.subject.clone(),
                metadata.author.clone(),
                metadata.total,
                !group.starts_with("git-import"),
            )
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

// Helper function to map To/Cc to Subsystems
fn identify_subsystems(to: &str, cc: &str) -> Vec<(String, String)> {
    let mut subsystems = Vec::new();
    let mut all_recipients = String::new();
    all_recipients.push_str(to);
    all_recipients.push_str(", ");
    all_recipients.push_str(cc);

    for email in all_recipients.split(',') {
        let email = email.trim();
        if email.is_empty() {
            continue;
        }

        let lower_email = email.to_lowercase();

        // 1. Static Map (Mimic MAINTAINERS)
        if lower_email.contains("linux-kernel@vger.kernel.org") {
            subsystems.push((
                "LKML".to_string(),
                "linux-kernel@vger.kernel.org".to_string(),
            ));
        } else if lower_email.contains("netdev@vger.kernel.org") {
            subsystems.push(("netdev".to_string(), "netdev@vger.kernel.org".to_string()));
        } else if lower_email.contains("bpf@vger.kernel.org") {
            subsystems.push(("bpf".to_string(), "bpf@vger.kernel.org".to_string()));
        } else if lower_email.contains("linux-usb@vger.kernel.org") {
            subsystems.push(("usb".to_string(), "linux-usb@vger.kernel.org".to_string()));
        } else if lower_email.contains("linux-fsdevel@vger.kernel.org") {
            subsystems.push((
                "fsdevel".to_string(),
                "linux-fsdevel@vger.kernel.org".to_string(),
            ));
        } else if lower_email.contains("linux-mm@kvack.org") {
            subsystems.push(("linux-mm".to_string(), "linux-mm@kvack.org".to_string()));
        } else if lower_email.ends_with("@vger.kernel.org")
            || lower_email.ends_with("@lists.linux.dev")
            || lower_email.ends_with("@lists.infradead.org")
        {
            // Fallback: derive name from email user part
            if let Some(name) = lower_email.split('@').next() {
                subsystems.push((name.to_string(), lower_email));
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
        let subsystems = identify_subsystems(to, cc);
        assert!(subsystems.contains(&(
            "LKML".to_string(),
            "linux-kernel@vger.kernel.org".to_string()
        )));
        assert!(subsystems.contains(&("netdev".to_string(), "netdev@vger.kernel.org".to_string())));

        // Test fallback
        let to = "unknown-list@vger.kernel.org";
        let cc = "";
        let subsystems = identify_subsystems(to, cc);
        assert!(subsystems.contains(&(
            "unknown-list".to_string(),
            "unknown-list@vger.kernel.org".to_string()
        )));

        // Test mixed
        let to = "linux-usb@vger.kernel.org, random-user@example.com";
        let cc = "bpf@vger.kernel.org";
        let subsystems = identify_subsystems(to, cc);
        assert!(subsystems.contains(&("usb".to_string(), "linux-usb@vger.kernel.org".to_string())));
        assert!(subsystems.contains(&("bpf".to_string(), "bpf@vger.kernel.org".to_string())));
        // random-user should be ignored as it doesn't match list patterns
        assert_eq!(subsystems.len(), 2);

        // Test linux-mm
        let to = "linux-mm@kvack.org";
        let subsystems = identify_subsystems(to, "");
        assert!(subsystems.contains(&("linux-mm".to_string(), "linux-mm@kvack.org".to_string())));
    }
}
