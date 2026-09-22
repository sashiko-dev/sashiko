use crate::ai::AiProvider;
use crate::db::Database;
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
    mut deadline: tokio::time::Instant,
) {
    loop {
        let renewal = tokio::time::timeout_at(deadline, async {
            sleep(Duration::from_secs(BUG_LEASE_RENEW_INTERVAL_SECONDS)).await;
            let started = tokio::time::Instant::now();
            (
                started,
                db.renew_bug_lease(bug_id, owner, BUG_LEASE_TTL_SECONDS)
                    .await,
            )
        })
        .await;
        match renewal {
            Ok((started, Ok(true))) => deadline = lease_deadline(started),
            Ok((_, Ok(false))) | Err(_) => return,
            Ok((_, Err(e))) => error!("Failed to renew the lease on bug {}: {}", bug_id, e),
        }
    }
}

fn lease_deadline(started: tokio::time::Instant) -> tokio::time::Instant {
    // SQLite expiry is measured in whole seconds. Stop conservatively before
    // the stored expiry even when the claim starts near a second boundary.
    started + Duration::from_secs((BUG_LEASE_TTL_SECONDS - 1) as u64)
}

fn new_claim_id(worker_id: &str) -> String {
    format!("{}:{:032x}", worker_id, fastrand::u128(..))
}

pub struct BugWorker {
    db: Arc<Database>,
    provider: Arc<dyn AiProvider>,
    repo_path: String,
    /// Identifies this worker in the lease it takes, so that a lease which
    /// never gets released can be traced back to a process.
    worker_id: String,
}

impl BugWorker {
    pub fn new(db: Arc<Database>, provider: Arc<dyn AiProvider>, repo_path: String) -> Self {
        Self {
            db,
            provider,
            repo_path,
            worker_id: format!(
                "{}:{}",
                std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string()),
                std::process::id()
            ),
        }
    }

    pub async fn run(&self) {
        info!(
            "Starting Bug Worker as {} (lease {}s renewed every {}s, {} attempts max)...",
            self.worker_id,
            BUG_LEASE_TTL_SECONDS,
            BUG_LEASE_RENEW_INTERVAL_SECONDS,
            BUG_MAX_ATTEMPTS
        );
        if let Err(e) = self.db.recover_stale_running_bugs().await {
            error!(
                "Failed to requeue interrupted bug analyses on startup: {}",
                e
            );
        }
        loop {
            let claim_id = new_claim_id(&self.worker_id);
            let claim_started = tokio::time::Instant::now();
            match self
                .db
                .claim_pending_bug(&claim_id, BUG_LEASE_TTL_SECONDS, BUG_MAX_ATTEMPTS)
                .await
            {
                Ok(Some(bug)) => {
                    let provider = self.provider.clone();
                    let db = self.db.clone();
                    let repo_path = self.repo_path.clone();

                    tokio::spawn(async move {
                        let actor = if !bug.reporter.is_empty() {
                            bug.reporter.as_str()
                        } else {
                            "sashiko"
                        };
                        let db = db.with_bug_claim(bug.id, &claim_id).with_bug_actor(
                            actor,
                            "sashiko:linux_bug",
                            Some(provider.get_capabilities().model_name),
                        );
                        info!("Processing raw bug ID {} ({})", bug.id, bug.bugid);

                        let input = crate::workflows::linux_bug::reconstruct_bug_input(&bug);

                        let mut tb = ToolBox::new(std::path::PathBuf::from(&repo_path), None);
                        if let Some(ref sha) = bug.discovered_in_commit {
                            tb.set_virtual_head(sha.clone());
                        }
                        let tools = Some(Arc::new(tb));

                        let analysis = run_while_leased(
                            crate::workflows::linux_bug::process_issue_worker(
                                provider.as_ref(),
                                tools,
                                &db,
                                &bug,
                                input,
                                Some("bug_worker"),
                            ),
                            maintain_lease(&db, bug.id, &claim_id, lease_deadline(claim_started)),
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
                    // Nothing left to claim, so this is the cheapest moment to
                    // retire the bugs that have run out of attempts.
                    if let Err(e) = self.db.abandon_exhausted_bugs(BUG_MAX_ATTEMPTS).await {
                        error!("Failed to abandon exhausted bugs: {}", e);
                    }
                    sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
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
            maintain_lease(&db, 1, "owner", tokio::time::Instant::now()),
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
}
