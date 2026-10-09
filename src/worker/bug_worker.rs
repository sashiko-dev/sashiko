use crate::ai::AiProvider;
use crate::api::BugInput;
use crate::db::{AttributedSubsystem, Database, SubsystemSource};
use crate::toolbox::ToolBox;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

/// How long a claim stays valid without being renewed.
///
/// This is deliberately far shorter than an analysis takes. The worker renews
/// the lease while it works, so the value only has to outlast a renewal
/// interval, and a worker that dies is noticed in minutes rather than after a
/// whole pipeline's worth of time.
const BUG_LEASE_TTL_SECONDS: i64 = 300;
/// How often a running analysis pushes its lease forward. Comfortably shorter
/// than the lease so that one failed renewal does not forfeit the claim.
const BUG_LEASE_RENEW_INTERVAL_SECONDS: u64 = 60;
const BUG_MAX_ATTEMPTS: i64 = 3;

/// Drops whichever future remains when analysis finishes or ownership is lost.
async fn run_while_leased<T>(
    analysis: impl std::future::Future<Output = T>,
    lease: impl std::future::Future<Output = ()>,
) -> Option<T> {
    tokio::select! {
        result = analysis => Some(result),
        () = lease => None,
    }
}

/// Retries transient renewal errors only within the last confirmed lease.
async fn maintain_lease(
    db: &Database,
    bug_id: i64,
    owner: &str,
    lease_ttl_seconds: i64,
    mut deadline: tokio::time::Instant,
) {
    let renew_interval_secs = ((clamp_lease_ttl_seconds(lease_ttl_seconds) as u64) / 3)
        .clamp(1, BUG_LEASE_RENEW_INTERVAL_SECONDS);
    loop {
        let renewal = tokio::time::timeout_at(deadline, async {
            sleep(Duration::from_secs(renew_interval_secs)).await;
            let started = tokio::time::Instant::now();
            (
                started,
                db.renew_bug_lease(bug_id, owner, lease_ttl_seconds).await,
            )
        })
        .await;
        match renewal {
            Ok((started, Ok(true))) => deadline = lease_deadline(started, lease_ttl_seconds),
            Ok((_, Ok(false))) | Err(_) => return,
            Ok((_, Err(e))) => error!("Failed to renew the lease on bug {}: {}", bug_id, e),
        }
    }
}

const MAX_BUG_LEASE_TTL_SECONDS: i64 = 86_400;

fn clamp_lease_ttl_seconds(lease_ttl_seconds: i64) -> i64 {
    lease_ttl_seconds.clamp(2, MAX_BUG_LEASE_TTL_SECONDS)
}

fn lease_deadline(started: tokio::time::Instant, lease_ttl_seconds: i64) -> tokio::time::Instant {
    // SQLite expiry is measured in whole seconds. Stop conservatively before
    // the stored expiry even when the claim starts near a second boundary.
    let secs = clamp_lease_ttl_seconds(lease_ttl_seconds)
        .saturating_sub(1)
        .max(1) as u64;
    started + Duration::from_secs(secs)
}

fn new_claim_id(worker_id: &str) -> String {
    format!("{}:{:032x}", worker_id, fastrand::u128(..))
}

const FIX_CHECK_SCAN_CHUNK_SIZE: usize = 250;
const MAX_FIX_CHECK_SCAN_CHUNKS: usize = 80;
const MAX_CACHED_RANGES_PER_SWEEP: usize = 64;

enum FixCheckClassification {
    Untouched,
    NeedsLlm(Vec<String>),
    SkipAndTouch,
    DeferredForNextSweep,
}

#[derive(Clone)]
pub struct BugWorker {
    db: Arc<Database>,
    provider: Arc<dyn AiProvider>,
    repo_path: String,
    project: crate::project::ProjectId,
    settings: crate::settings::LinuxBugSettings,
    /// Identifies this worker in the lease it takes, so that a lease which
    /// never gets released can be traced back to a process.
    worker_id: String,
}

impl BugWorker {
    pub fn new(db: Arc<Database>, provider: Arc<dyn AiProvider>, repo_path: String) -> Self {
        let db = Arc::new(db.with_bug_actor("system", "sashiko:linux_bug", None));
        Self {
            db,
            provider,
            repo_path,
            project: crate::project::ProjectId::Linux,
            settings: crate::settings::LinuxBugSettings {
                lease_ttl_seconds: BUG_LEASE_TTL_SECONDS,
                max_attempts: BUG_MAX_ATTEMPTS,
                ..Default::default()
            },
            worker_id: format!(
                "{}:{}",
                std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string()),
                std::process::id()
            ),
        }
    }

    pub fn with_project(mut self, project: crate::project::ProjectId) -> Self {
        self.project = project;
        let bug_tool = match project {
            crate::project::ProjectId::Linux => "sashiko:linux_bug",
            crate::project::ProjectId::Sashiko => "sashiko:sashiko_bug",
        };
        self.db = Arc::new(self.db.with_bug_actor("system", bug_tool, None));
        self
    }

    pub fn with_settings(mut self, settings: crate::settings::LinuxBugSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Runs a bounded two-phase sweep checking whether open bugs have been
    /// fixed in the upstream mainline tree:
    /// 1. Phase 1 scans open bugs in chunks using keyset pagination and an
    ///    in-memory `CommitRangeIndex`, batch-advancing all bugs with zero
    ///    candidate commits without consuming `fix_check_batch_size`.
    /// 2. Phase 2 claims and runs LLM verification for up to
    ///    `fix_check_batch_size` bugs that have candidate commits.
    pub async fn check_open_bugs_upstream(&self) -> usize {
        if !self.settings.fix_check_enabled {
            return 0;
        }
        let batch_size = self.settings.fix_check_batch_size;
        if batch_size == 0 {
            return 0;
        }
        let repo_path = std::path::Path::new(&self.repo_path);
        let Some(linus_sha) = crate::workflows::linux_bug::resolve_linus_sha(repo_path).await
        else {
            warn!(
                "Skipping upstream bug fix check: could not resolve mainline tree SHA in {}",
                self.repo_path
            );
            return 0;
        };

        let (advanced_without_llm, llm_queue) = self
            .sweep_zero_token_fix_checks(repo_path, &linus_sha, batch_size)
            .await;
        if advanced_without_llm > 0 {
            info!(
                "Advanced {} open bug(s) verified_on_sha to {} via zero-token git pre-filter",
                advanced_without_llm, linus_sha
            );
        }

        let lease_ttl_seconds = clamp_lease_ttl_seconds(self.settings.lease_ttl_seconds);
        let mut llm_checked = 0;
        if !llm_queue.is_empty() {
            info!(
                "Evaluating {} open bug(s) with candidate commits (batch limit {}) against mainline SHA {}...",
                llm_queue.len(),
                batch_size,
                linus_sha
            );
        }
        for (bug_id, precomputed_candidates) in llm_queue {
            if self
                .verify_single_bug_upstream(
                    repo_path,
                    &linus_sha,
                    lease_ttl_seconds,
                    bug_id,
                    precomputed_candidates,
                )
                .await
            {
                llm_checked += 1;
            }
        }

        advanced_without_llm + llm_checked
    }

    async fn sweep_zero_token_fix_checks(
        &self,
        repo_path: &std::path::Path,
        linus_sha: &str,
        batch_size: usize,
    ) -> (usize, Vec<(i64, Option<Vec<String>>)>) {
        let mut range_cache: std::collections::HashMap<
            String,
            crate::workflows::linux_bug::CommitRangeLookup,
        > = std::collections::HashMap::new();
        let mut llm_queue: Vec<(i64, Option<Vec<String>>)> = Vec::new();
        let mut cursor: Option<(i64, i64)> = None;
        let mut total_advanced = 0usize;

        for _ in 0..MAX_FIX_CHECK_SCAN_CHUNKS {
            let chunk = match self
                .db
                .list_open_bugs_for_fix_check(linus_sha, cursor, FIX_CHECK_SCAN_CHUNK_SIZE)
                .await
            {
                Ok(c) if c.is_empty() => break,
                Ok(c) => c,
                Err(e) => {
                    error!(
                        "Failed to list open bugs for upstream fix check at {}: {}",
                        linus_sha, e
                    );
                    break;
                }
            };
            cursor = chunk.last().map(|b| (b.updated_at, b.id));

            let mut untouched_linux = Vec::new();
            let mut untouched_sashiko = Vec::new();
            let mut skipped_ids = Vec::new();

            for bug in chunk {
                let classification = if llm_queue.len() >= batch_size {
                    Self::classify_bug_from_cached_index(linus_sha, &bug, &range_cache)
                } else {
                    self.classify_bug_for_fix_check(repo_path, linus_sha, &bug, &mut range_cache)
                        .await
                };
                match classification {
                    FixCheckClassification::SkipAndTouch => skipped_ids.push(bug.id),
                    FixCheckClassification::DeferredForNextSweep => {}
                    FixCheckClassification::Untouched => {
                        let proj = crate::workflows::linux_bug::infer_project_from_bug_or_tool(
                            Some(&bug.bugid),
                            None,
                            self.project,
                        );
                        match proj {
                            crate::project::ProjectId::Linux => untouched_linux.push(bug),
                            crate::project::ProjectId::Sashiko => untouched_sashiko.push(bug),
                        }
                    }
                    FixCheckClassification::NeedsLlm(candidates) => {
                        llm_queue.push((bug.id, Some(candidates)));
                    }
                }
            }

            total_advanced += self
                .flush_zero_token_chunk(
                    linus_sha,
                    &untouched_linux,
                    &untouched_sashiko,
                    &skipped_ids,
                )
                .await;

            if llm_queue.len() >= batch_size {
                break;
            }
        }

        (total_advanced, llm_queue)
    }

    fn classify_bug_from_cached_index(
        linus_sha: &str,
        bug: &crate::db::Bug,
        range_cache: &std::collections::HashMap<
            String,
            crate::workflows::linux_bug::CommitRangeLookup,
        >,
    ) -> FixCheckClassification {
        let Some(prev_sha) = bug.verified_on_sha() else {
            return FixCheckClassification::SkipAndTouch;
        };
        let prev_trimmed = prev_sha.trim();
        if prev_trimmed.is_empty() || prev_trimmed == linus_sha {
            return FixCheckClassification::Untouched;
        }
        let files = crate::workflows::linux_bug::extract_bug_files(bug);
        if files.is_empty() {
            return FixCheckClassification::SkipAndTouch;
        }
        let intro_sha = bug.introducing_commit_sha();
        match range_cache.get(prev_trimmed) {
            Some(crate::workflows::linux_bug::CommitRangeLookup::NotAncestor) => {
                FixCheckClassification::SkipAndTouch
            }
            Some(crate::workflows::linux_bug::CommitRangeLookup::Indexed(index)) => {
                match index.find_candidates(&files, intro_sha.as_deref()) {
                    None => FixCheckClassification::SkipAndTouch,
                    Some(c) if c.is_empty() => FixCheckClassification::Untouched,
                    Some(_) => FixCheckClassification::DeferredForNextSweep,
                }
            }
            Some(crate::workflows::linux_bug::CommitRangeLookup::TooLarge) | None => {
                FixCheckClassification::DeferredForNextSweep
            }
        }
    }

    async fn classify_bug_for_fix_check(
        &self,
        repo_path: &std::path::Path,
        linus_sha: &str,
        bug: &crate::db::Bug,
        range_cache: &mut std::collections::HashMap<
            String,
            crate::workflows::linux_bug::CommitRangeLookup,
        >,
    ) -> FixCheckClassification {
        let Some(prev_sha) = bug.verified_on_sha() else {
            return FixCheckClassification::SkipAndTouch;
        };
        let prev_trimmed = prev_sha.trim();
        if prev_trimmed.is_empty() || prev_trimmed == linus_sha {
            return FixCheckClassification::Untouched;
        }
        let files = crate::workflows::linux_bug::extract_bug_files(bug);
        if files.is_empty() {
            return FixCheckClassification::SkipAndTouch;
        }
        let intro_sha = bug.introducing_commit_sha();

        if !range_cache.contains_key(prev_trimmed)
            && range_cache.len() < MAX_CACHED_RANGES_PER_SWEEP
        {
            let lookup = match crate::workflows::linux_bug::build_commit_range_index(
                repo_path,
                prev_trimmed,
                linus_sha,
            )
            .await
            {
                Ok(l) => l,
                Err(e) => {
                    warn!(
                        "Failed to build commit range index for {}..{}: {}",
                        prev_trimmed, linus_sha, e
                    );
                    crate::workflows::linux_bug::CommitRangeLookup::TooLarge
                }
            };
            range_cache.insert(prev_trimmed.to_string(), lookup);
        }

        match range_cache.get(prev_trimmed) {
            Some(crate::workflows::linux_bug::CommitRangeLookup::NotAncestor) => {
                FixCheckClassification::SkipAndTouch
            }
            Some(crate::workflows::linux_bug::CommitRangeLookup::Indexed(index)) => {
                match index.find_candidates(&files, intro_sha.as_deref()) {
                    None => FixCheckClassification::SkipAndTouch,
                    Some(c) if c.is_empty() => FixCheckClassification::Untouched,
                    Some(c) => FixCheckClassification::NeedsLlm(c),
                }
            }
            Some(crate::workflows::linux_bug::CommitRangeLookup::TooLarge) | None => {
                match crate::workflows::linux_bug::find_candidate_fix_commits(
                    repo_path,
                    prev_trimmed,
                    linus_sha,
                    &files,
                    intro_sha.as_deref(),
                )
                .await
                {
                    Ok(None) => FixCheckClassification::SkipAndTouch,
                    Ok(Some(c)) if c.is_empty() => FixCheckClassification::Untouched,
                    Ok(Some(c)) => FixCheckClassification::NeedsLlm(c),
                    Err(e) => {
                        warn!(
                            "Failed to query candidate fix commits for bug {} in {}..{}: {}",
                            bug.bugid, prev_trimmed, linus_sha, e
                        );
                        FixCheckClassification::SkipAndTouch
                    }
                }
            }
        }
    }

    async fn flush_zero_token_chunk(
        &self,
        linus_sha: &str,
        untouched_linux: &[crate::db::Bug],
        untouched_sashiko: &[crate::db::Bug],
        skipped_ids: &[i64],
    ) -> usize {
        let mut advanced = 0usize;
        if !untouched_linux.is_empty() {
            let db = self
                .db
                .with_bug_actor("sashiko", "sashiko:linux_bug:fix_check", None);
            match db
                .advance_open_bugs_without_llm_batch(untouched_linux, linus_sha)
                .await
            {
                Ok(n) => advanced += n,
                Err(e) => warn!("Failed to batch-advance untouched Linux bugs: {}", e),
            }
        }
        if !untouched_sashiko.is_empty() {
            let db = self
                .db
                .with_bug_actor("sashiko", "sashiko:sashiko_bug:fix_check", None);
            match db
                .advance_open_bugs_without_llm_batch(untouched_sashiko, linus_sha)
                .await
            {
                Ok(n) => advanced += n,
                Err(e) => warn!("Failed to batch-advance untouched Sashiko bugs: {}", e),
            }
        }
        if !skipped_ids.is_empty()
            && let Err(e) = self.db.touch_open_bugs_fix_check_batch(skipped_ids).await
        {
            warn!("Failed to batch-touch skipped fix-check bugs: {}", e);
        }
        advanced
    }

    async fn verify_single_bug_upstream(
        &self,
        repo_path: &std::path::Path,
        linus_sha: &str,
        lease_ttl_seconds: i64,
        bug_id: i64,
        precomputed_candidates: Option<Vec<String>>,
    ) -> bool {
        let claim_id = new_claim_id(&self.worker_id);
        let claim_started = tokio::time::Instant::now();
        let bug = match self
            .db
            .claim_specific_open_bug_for_fix_check(bug_id, linus_sha, &claim_id, lease_ttl_seconds)
            .await
        {
            Ok(Some(b)) => b,
            Ok(None) => return false,
            Err(e) => {
                error!(
                    "Failed to claim open bug #{} for upstream fix check at {}: {}",
                    bug_id, linus_sha, e
                );
                return false;
            }
        };

        let effective_project = crate::workflows::linux_bug::infer_project_from_bug_or_tool(
            Some(&bug.bugid),
            None,
            self.project,
        );
        let scoped_db = self.db.with_bug_claim(bug.id, &claim_id);
        let Some(res) = run_while_leased(
            crate::workflows::linux_bug::check_bug_fixed_upstream_with_candidates(
                self.provider.as_ref(),
                repo_path,
                &scoped_db,
                &bug,
                linus_sha,
                effective_project,
                precomputed_candidates,
            ),
            maintain_lease(
                &scoped_db,
                bug.id,
                &claim_id,
                lease_ttl_seconds,
                lease_deadline(claim_started, lease_ttl_seconds),
            ),
        )
        .await
        else {
            warn!(
                "Stopping upstream fix check for bug {} after losing its lease",
                bug.bugid
            );
            return false;
        };

        match res {
            Ok(outcome) => {
                match outcome {
                    crate::workflows::linux_bug::UpstreamFixCheckOutcome::FixedUpstream {
                        fixing_commit_sha,
                        ..
                    } => {
                        info!(
                            "Marked open bug {} as fixed upstream by commit {}",
                            bug.bugid, fixing_commit_sha
                        );
                    }
                    crate::workflows::linux_bug::UpstreamFixCheckOutcome::AdvancedWithoutLlm {
                        ..
                    } => {
                        info!(
                            "Advanced open bug {} verified_on_sha to {} (0 commits touched affected files)",
                            bug.bugid, linus_sha
                        );
                    }
                    crate::workflows::linux_bug::UpstreamFixCheckOutcome::StillPresentAfterLlm {
                        ..
                    } => {
                        info!(
                            "Open bug {} confirmed still present at {}",
                            bug.bugid, linus_sha
                        );
                    }
                    _ => {}
                }
                true
            }
            Err(e) => {
                warn!("Upstream fix check failed for bug {}: {}", bug.bugid, e);
                if let Err(db_err) = scoped_db.touch_bug_fix_check_timestamp(bug.id).await {
                    warn!(
                        "Failed to update fix check timestamp for bug {}: {}",
                        bug.bugid, db_err
                    );
                }
                false
            }
        }
    }

    pub async fn run(&self) {
        let concurrency = self.settings.concurrency.max(1);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
        let lease_ttl_seconds = clamp_lease_ttl_seconds(self.settings.lease_ttl_seconds);
        let max_attempts = self.settings.max_attempts.max(1);
        let fix_check_enabled = self.settings.fix_check_enabled;
        let fix_check_interval = self.settings.fix_check_interval_seconds;

        info!(
            "Starting Bug Worker as {} (project {}, concurrency {}, lease {}s renewed every {}s, {} attempts max, fix check enabled: {}, interval {}s)...",
            self.worker_id,
            self.project.as_str(),
            concurrency,
            lease_ttl_seconds,
            ((lease_ttl_seconds as u64) / 3).clamp(1, BUG_LEASE_RENEW_INTERVAL_SECONDS),
            max_attempts,
            fix_check_enabled,
            fix_check_interval
        );
        if let Err(e) = self.db.recover_stale_running_bugs().await {
            error!(
                "Failed to requeue interrupted bug analyses on startup: {}",
                e
            );
        }
        let mut last_fix_check: Option<tokio::time::Instant> = None;
        let mut fix_check_handle: Option<tokio::task::JoinHandle<usize>> = None;

        loop {
            if fix_check_handle.as_ref().is_some_and(|h| h.is_finished())
                && let Some(handle) = fix_check_handle.take()
                && let Err(e) = handle.await
            {
                error!("Background upstream bug fix check task failed: {}", e);
            }

            if fix_check_enabled
                && fix_check_interval > 0
                && fix_check_handle.is_none()
                && last_fix_check
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(fix_check_interval))
            {
                last_fix_check = Some(tokio::time::Instant::now());
                let worker = self.clone();
                fix_check_handle = Some(tokio::spawn(async move {
                    worker.check_open_bugs_upstream().await
                }));
            }

            let Ok(Ok(permit)) =
                tokio::time::timeout(Duration::from_secs(5), semaphore.clone().acquire_owned())
                    .await
            else {
                continue;
            };

            let claim_id = new_claim_id(&self.worker_id);
            let claim_started = tokio::time::Instant::now();
            match self
                .db
                .claim_pending_bug(&claim_id, lease_ttl_seconds, max_attempts)
                .await
            {
                Ok(Some(bug)) => {
                    let provider = self.provider.clone();
                    let db = self.db.clone();
                    let repo_path = self.repo_path.clone();
                    let worker_project = self.project;

                    tokio::spawn(async move {
                        let _permit = permit;
                        let effective_project =
                            crate::workflows::linux_bug::infer_project_from_bug_or_tool(
                                Some(&bug.bugid),
                                None,
                                worker_project,
                            );
                        let bug_tool = match effective_project {
                            crate::project::ProjectId::Linux => "sashiko:linux_bug",
                            crate::project::ProjectId::Sashiko => "sashiko:sashiko_bug",
                        };
                        let actor = if !bug.reporter.is_empty() {
                            bug.reporter.as_str()
                        } else {
                            "sashiko"
                        };
                        let db = db.with_bug_claim(bug.id, &claim_id).with_bug_actor(
                            actor,
                            bug_tool,
                            Some(provider.get_capabilities().model_name),
                        );
                        info!("Processing raw bug ID {} ({})", bug.id, bug.bugid);

                        let input = if let Some(raw) = bug.raw_input() {
                            serde_json::from_str::<BugInput>(&raw).ok()
                        } else {
                            None
                        }
                        .unwrap_or_else(|| BugInput {
                            problem: bug.problem().to_string(),
                            reasoning: bug
                                .severity_explanation()
                                .unwrap_or_else(|| "No reasoning provided.".to_string()),
                            locations: bug.locations(),
                            // The stored bug keeps the subsystem names but the
                            // read model drops their provenance, so they are
                            // rebuilt as caller supplied. Nothing is lost: the
                            // analysis re-resolves subsystems from MAINTAINERS
                            // before the outcome is written back.
                            subsystems: bug
                                .subsystems
                                .iter()
                                .map(|name| {
                                    AttributedSubsystem::new(name, SubsystemSource::CallerSupplied)
                                })
                                .collect(),
                            source_files: bug.source_files().unwrap_or_default(),
                            commit_sha: bug.discovered_in_commit.clone(),
                            patchset_id: bug.discovered_in_patchset_id,
                            patch_id: bug.discovered_in_patch_id,
                            baseline_sha: bug.discovered_in_commit.clone(),
                            review_id: None,
                        });

                        let mut tb = ToolBox::new(std::path::PathBuf::from(&repo_path), None);
                        if let Some(ref sha) = bug.discovered_in_commit {
                            tb.set_virtual_head(sha.clone());
                        }
                        let tools = Some(Arc::new(tb));

                        let analysis = run_while_leased(
                            crate::workflows::linux_bug::process_issue_worker_for_project(
                                provider.as_ref(),
                                tools,
                                &db,
                                &bug,
                                input,
                                Some("bug_worker"),
                                effective_project,
                            ),
                            maintain_lease(
                                &db,
                                bug.id,
                                &claim_id,
                                lease_ttl_seconds,
                                lease_deadline(claim_started, lease_ttl_seconds),
                            ),
                        )
                        .await;
                        let Some(analysis) = analysis else {
                            warn!(
                                "Cancelled analysis of bug {} after losing its lease",
                                bug.id
                            );
                            return;
                        };

                        match analysis {
                            Ok(outcome) => {
                                info!("Successfully processed raw bug {}: {}", bug.id, outcome);
                                // The workflow records the outcome; this only
                                // drops the claim so the row stops looking
                                // like it is still being worked on.
                                if let Err(e) = db.release_bug_lease(bug.id).await {
                                    error!("Failed to release lease on bug {}: {}", bug.id, e);
                                }
                            }
                            Err(e) => {
                                error!("Failed to process bug {}: {}", bug.id, e);
                                let error_msg = format!("Error during async processing: {}", e);
                                if let Err(e) = db.fail_bug_analysis(bug.id, &error_msg).await {
                                    warn!(
                                        "Could not settle failed analysis of bug {}: {}",
                                        bug.id, e
                                    );
                                }
                            }
                        }
                    });
                }
                Ok(None) => {
                    drop(permit);
                    // Nothing left to claim, so this is the cheapest moment to
                    // retire the bugs that have run out of attempts.
                    if let Err(e) = self.db.abandon_exhausted_bugs(max_attempts).await {
                        error!("Failed to abandon exhausted bugs: {}", e);
                    }
                    sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    drop(permit);
                    error!("Database error while claiming a bug for analysis: {}", e);
                    sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn losing_a_lease_drops_the_analysis() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let result = run_while_leased(
            async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            },
            async {},
        )
        .await;
        assert!(result.is_none());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn completion_and_unwinding_drop_renewal() {
        for panic in [false, true] {
            let dropped = Arc::new(AtomicBool::new(false));
            let guard = Dropped(dropped.clone());
            let result = std::panic::AssertUnwindSafe(run_while_leased(
                async {
                    assert!(!panic, "analysis unwound");
                    42
                },
                async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                },
            ));
            let result = futures::FutureExt::catch_unwind(result).await;
            if panic {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), Some(42));
            }
            assert!(dropped.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn renewal_stops_at_the_last_confirmed_deadline() {
        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".into(),
            token: String::new(),
        })
        .await
        .unwrap();
        // No schema is needed: expiry must stop the renewal before its next
        // database request, rather than waiting another renewal interval.
        tokio::time::timeout(
            Duration::from_secs(1),
            maintain_lease(
                &db,
                1,
                "owner",
                BUG_LEASE_TTL_SECONDS,
                tokio::time::Instant::now(),
            ),
        )
        .await
        .unwrap();
    }

    #[test]
    fn attempts_from_one_process_have_distinct_claim_ids() {
        let first = new_claim_id("host:123");
        let second = new_claim_id("host:123");
        assert!(first.starts_with("host:123:"));
        assert_ne!(first, second);
    }

    struct DummyProvider;

    #[async_trait::async_trait]
    impl AiProvider for DummyProvider {
        async fn generate_content(
            &self,
            _req: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            anyhow::bail!("DummyProvider should not be called on zero-candidate advance")
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "dummy".to_string(),
                context_window_size: 8192,
            }
        }
    }

    #[tokio::test]
    async fn check_open_bugs_upstream_advances_bug_with_active_lease() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

        let run_git = |args: &[&str]| {
            let out = crate::git_cmd::in_dir(repo).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        run_git(&["init", "-b", "master"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        std::fs::write(repo.join("foo.c"), "int foo(void) { return 0; }\n").unwrap();
        run_git(&["add", "foo.c"]);
        run_git(&["commit", "-m", "initial"]);
        let sha1 = run_git(&["rev-parse", "HEAD"]);

        std::fs::write(repo.join("bar.c"), "int bar(void) { return 1; }\n").unwrap();
        run_git(&["add", "bar.c"]);
        run_git(&["commit", "-m", "unrelated change"]);
        let sha2 = run_git(&["rev-parse", "HEAD"]);

        let db = Arc::new(
            Database::new(&crate::settings::DatabaseSettings {
                url: ":memory:".into(),
                token: String::new(),
            })
            .await
            .unwrap(),
        );
        db.migrate().await.unwrap();

        let id = db
            .create_bug(&crate::db::NewBug {
                bugid: "linux-worker-test".to_string(),
                title: "bug in foo".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 1000,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: Some(sha1.clone()),
                source_ref: Some(sha1.clone()),
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![],
            })
            .await
            .unwrap();
        db.update_bug_outcome(
            id,
            crate::db::UpdateBugOutcomeParams {
                lifecycle_status: crate::db::BugLifecycleStatus::Open,
                problem: Some("bug in foo"),
                source_files: Some(&["foo.c".to_string()]),
                severity: crate::db::Severity::Medium,
                inline_review: "report",
                verified_on_sha: Some(&sha1),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let disabled_worker = BugWorker::new(
            db.clone(),
            Arc::new(DummyProvider),
            repo.to_string_lossy().to_string(),
        )
        .with_settings(crate::settings::LinuxBugSettings {
            enabled: true,
            fix_check_enabled: false,
            lease_ttl_seconds: 60,
            max_attempts: 3,
            fix_check_interval_seconds: 60,
            fix_check_batch_size: 10,
            ..Default::default()
        });
        assert_eq!(disabled_worker.check_open_bugs_upstream().await, 0);

        let worker = BugWorker::new(
            db.clone(),
            Arc::new(DummyProvider),
            repo.to_string_lossy().to_string(),
        )
        .with_settings(crate::settings::LinuxBugSettings {
            enabled: true,
            fix_check_enabled: true,
            lease_ttl_seconds: 60,
            max_attempts: 3,
            fix_check_interval_seconds: 60,
            fix_check_batch_size: 10,
            ..Default::default()
        });

        let checked = worker.check_open_bugs_upstream().await;
        assert_eq!(checked, 1);

        let updated = db.get_bug(id).await.unwrap().unwrap();
        assert_eq!(updated.verified_on_sha().as_deref(), Some(sha2.as_str()));
        assert!(!updated.is_fixed());
    }

    struct CountingStillPresentProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AiProvider for CountingStillPresentProvider {
        async fn generate_content(
            &self,
            _req: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::ai::AiResponse {
                content: Some(
                    r#"{"status":"still_present","fixing_commit_sha":null,"explanation":"Defect remains present after unrelated refactor."}"#
                        .to_string(),
                ),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
                provider_metadata: None,
            })
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "counting-mock".to_string(),
                context_window_size: 8192,
            }
        }
    }

    #[tokio::test]
    async fn check_open_bugs_upstream_advances_all_untouched_while_capping_llm_batch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

        let run_git = |args: &[&str]| {
            let out = crate::git_cmd::in_dir(repo).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        run_git(&["init", "-b", "master"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test"]);

        for i in 0..8 {
            std::fs::write(repo.join(format!("untouched_{i}.c")), "int x = 0;\n").unwrap();
        }
        for i in 0..4 {
            std::fs::write(repo.join(format!("touched_{i}.c")), "int y = 0;\n").unwrap();
        }
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "initial"]);
        let sha1 = run_git(&["rev-parse", "HEAD"]);

        for i in 0..4 {
            std::fs::write(repo.join(format!("touched_{i}.c")), "int y = 1;\n").unwrap();
        }
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "modify touched files"]);
        let sha2 = run_git(&["rev-parse", "HEAD"]);

        let db = Arc::new(
            Database::new(&crate::settings::DatabaseSettings {
                url: ":memory:".into(),
                token: String::new(),
            })
            .await
            .unwrap(),
        );
        db.migrate().await.unwrap();

        let mut untouched_ids = Vec::new();
        for i in 0..8 {
            let file = format!("untouched_{i}.c");
            let id = db
                .create_bug(&crate::db::NewBug {
                    bugid: format!("linux-untouched-{i}"),
                    title: format!("bug in {file}"),
                    lifecycle_status: crate::db::BugLifecycleStatus::New,
                    pipeline_state: crate::db::BugPipelineState::Pending,
                    assignee: None,
                    reporter: "sashiko".to_string(),
                    reported_at: 1000 + i,
                    discovered_in_patchset_id: None,
                    discovered_in_patch_id: None,
                    discovered_in_commit: Some(sha1.clone()),
                    source_ref: Some(sha1.clone()),
                    vector_json: None,
                    duplicate_of_id: None,
                    subsystems: vec![],
                })
                .await
                .unwrap();
            db.update_bug_outcome(
                id,
                crate::db::UpdateBugOutcomeParams {
                    lifecycle_status: crate::db::BugLifecycleStatus::Open,
                    problem: Some("bug"),
                    source_files: Some(std::slice::from_ref(&file)),
                    severity: crate::db::Severity::Medium,
                    inline_review: "report",
                    verified_on_sha: Some(&sha1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            untouched_ids.push(id);
        }

        let mut touched_ids = Vec::new();
        for i in 0..4 {
            let file = format!("touched_{i}.c");
            let id = db
                .create_bug(&crate::db::NewBug {
                    bugid: format!("linux-touched-{i}"),
                    title: format!("bug in {file}"),
                    lifecycle_status: crate::db::BugLifecycleStatus::New,
                    pipeline_state: crate::db::BugPipelineState::Pending,
                    assignee: None,
                    reporter: "sashiko".to_string(),
                    reported_at: 2000 + i,
                    discovered_in_patchset_id: None,
                    discovered_in_patch_id: None,
                    discovered_in_commit: Some(sha1.clone()),
                    source_ref: Some(sha1.clone()),
                    vector_json: None,
                    duplicate_of_id: None,
                    subsystems: vec![],
                })
                .await
                .unwrap();
            db.update_bug_outcome(
                id,
                crate::db::UpdateBugOutcomeParams {
                    lifecycle_status: crate::db::BugLifecycleStatus::Open,
                    problem: Some("bug"),
                    source_files: Some(std::slice::from_ref(&file)),
                    severity: crate::db::Severity::Medium,
                    inline_review: "report",
                    verified_on_sha: Some(&sha1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            touched_ids.push(id);
        }

        let provider = Arc::new(CountingStillPresentProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let worker = BugWorker::new(
            db.clone(),
            provider.clone(),
            repo.to_string_lossy().to_string(),
        )
        .with_settings(crate::settings::LinuxBugSettings {
            enabled: true,
            fix_check_enabled: true,
            lease_ttl_seconds: 60,
            max_attempts: 3,
            fix_check_interval_seconds: 60,
            fix_check_batch_size: 2,
            ..Default::default()
        });

        // Sweep 1: all 8 untouched bugs advance without LLM, plus 2 of the 4
        // touched bugs are evaluated via the LLM (capped by fix_check_batch_size = 2).
        let checked_first = worker.check_open_bugs_upstream().await;
        assert_eq!(checked_first, 10);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

        for id in &untouched_ids {
            let b = db.get_bug(*id).await.unwrap().unwrap();
            assert_eq!(b.verified_on_sha().as_deref(), Some(sha2.as_str()));
        }

        // Sweep 2: the remaining 2 touched bugs are evaluated via the LLM.
        let checked_second = worker.check_open_bugs_upstream().await;
        assert_eq!(checked_second, 2);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 4);

        for id in &touched_ids {
            let b = db.get_bug(*id).await.unwrap().unwrap();
            assert_eq!(b.verified_on_sha().as_deref(), Some(sha2.as_str()));
        }
    }

    struct GatedProvider {
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
        two_active_notify: tokio::sync::Notify,
        release_gate: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl AiProvider for GatedProvider {
        async fn generate_content(
            &self,
            _req: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let cur = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(cur, Ordering::SeqCst);
            if cur >= 2 {
                self.two_active_notify.notify_one();
            }
            self.release_gate.notified().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("simulated provider failure after gate release")
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "gated-mock".to_string(),
                context_window_size: 8192,
            }
        }
    }

    #[tokio::test]
    async fn bug_worker_run_bounds_concurrent_claims_and_analyses() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            Database::new(&crate::settings::DatabaseSettings {
                url: ":memory:".into(),
                token: String::new(),
            })
            .await
            .unwrap(),
        );
        db.migrate().await.unwrap();

        let mut bug_ids = Vec::new();
        for i in 0..5 {
            let id = db
                .create_bug(&crate::db::NewBug {
                    bugid: format!("linux-bounded-{i}"),
                    title: format!("pending bug {i}"),
                    lifecycle_status: crate::db::BugLifecycleStatus::New,
                    pipeline_state: crate::db::BugPipelineState::Pending,
                    assignee: None,
                    reporter: "sashiko".to_string(),
                    reported_at: 1000 + i,
                    discovered_in_patchset_id: None,
                    discovered_in_patch_id: None,
                    discovered_in_commit: None,
                    source_ref: None,
                    vector_json: None,
                    duplicate_of_id: None,
                    subsystems: vec![],
                })
                .await
                .unwrap();
            bug_ids.push(id);
        }

        let provider = Arc::new(GatedProvider {
            active: std::sync::atomic::AtomicUsize::new(0),
            max_active: std::sync::atomic::AtomicUsize::new(0),
            two_active_notify: tokio::sync::Notify::new(),
            release_gate: tokio::sync::Notify::new(),
        });
        let two_active = provider.two_active_notify.notified();

        let worker = BugWorker::new(
            db.clone(),
            provider.clone(),
            dir.path().to_string_lossy().to_string(),
        )
        .with_settings(crate::settings::LinuxBugSettings {
            enabled: true,
            concurrency: 2,
            fix_check_enabled: false,
            lease_ttl_seconds: 60,
            max_attempts: 1,
            fix_check_interval_seconds: 0,
            fix_check_batch_size: 10,
        });

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        tokio::time::timeout(Duration::from_secs(5), two_active)
            .await
            .expect("two worker tasks should reach generate_content");

        // Give the worker loop a moment to attempt any additional claims; because
        // both semaphore permits are held, the remaining 3 bugs must stay Pending.
        sleep(Duration::from_millis(50)).await;

        assert_eq!(provider.active.load(Ordering::SeqCst), 2);
        assert_eq!(provider.max_active.load(Ordering::SeqCst), 2);

        let mut running_count = 0;
        let mut pending_count = 0;
        for id in &bug_ids {
            let b = db.get_bug(*id).await.unwrap().unwrap();
            match b.pipeline_state {
                crate::db::BugPipelineState::Running => running_count += 1,
                crate::db::BugPipelineState::Pending => pending_count += 1,
                other => panic!("unexpected pipeline_state while gated: {other:?}"),
            }
        }
        assert_eq!(
            running_count, 2,
            "only 2 bugs should be claimed while gated"
        );
        assert_eq!(pending_count, 3, "remaining 3 bugs must remain Pending");

        handle.abort();
        provider.release_gate.notify_waiters();
    }
}
