use crate::git_ops::{BackoffError, FetchOutcome, ensure_remote};
use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{error, info, warn};

pub struct GitSyncWorker {
    repo_path: PathBuf,
}

impl GitSyncWorker {
    pub fn new(repo_path: PathBuf) -> Self {
        Self { repo_path }
    }

    pub async fn run(&self) {
        info!("GitSyncWorker started. Will sync remotes periodically.");
        loop {
            if let Err(e) = self.sync_all_remotes().await {
                error!("GitSyncWorker failed during sync cycle: {}", e);
            }
            // Sleep for 1 hour before checking again.
            // ensure_remote handles the fine-grained 4h/24h timestamp logic.
            sleep(Duration::from_secs(3600)).await;
        }
    }

    async fn sync_all_remotes(&self) -> Result<()> {
        info!("GitSyncWorker: Starting sync cycle.");

        // Enumerate all configured remotes
        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["remote"])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("GitSyncWorker: Failed to list git remotes: {}", stderr);
            return Err(anyhow::anyhow!("Failed to list remotes"));
        }

        let remotes_str = String::from_utf8_lossy(&output.stdout);
        let remotes: Vec<&str> = remotes_str
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with("fetcher-"))
            .collect();

        info!("GitSyncWorker: Found {} remotes to check.", remotes.len());

        let mut fetched = 0usize;
        let mut skipped = 0usize;
        let mut backed_off = 0usize;
        let mut stale = 0usize;
        let mut failed = 0usize;
        let mut first_failure = String::new();
        // Git's stderr can carry the remote URL, credentials and
        // all, and runs to several lines.
        let mut record_failure = |message: &str| {
            if first_failure.is_empty() {
                first_failure = crate::utils::redact_secret(message)
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
            }
        };

        for remote in remotes {
            // Get URL for the remote
            let url_output = Command::new("git")
                .current_dir(&self.repo_path)
                .args(["remote", "get-url", remote])
                .output()
                .await?;

            if !url_output.status.success() {
                let stderr = String::from_utf8_lossy(&url_output.stderr);
                let message = format!("Failed to get URL for remote {}: {}", remote, stderr.trim());
                warn!("GitSyncWorker: {}", message);
                failed += 1;
                record_failure(&message);
                continue;
            }

            let url = String::from_utf8_lossy(&url_output.stdout)
                .trim()
                .to_string();

            // Check if it's time to fetch and fetch if necessary
            // force_fetch=false so we respect the 4h/24h intervals in ensure_remote
            match ensure_remote(&self.repo_path, remote, &url, false).await {
                Ok(FetchOutcome::Fetched) => fetched += 1,
                Ok(FetchOutcome::Skipped) => skipped += 1,
                Ok(FetchOutcome::BackedOff) => backed_off += 1,
                Ok(FetchOutcome::StaleLocalRef(message)) => {
                    stale += 1;
                    record_failure(&message);
                }
                Err(e) if e.downcast_ref::<BackoffError>().is_some() => {
                    info!("GitSyncWorker: {}", e);
                    backed_off += 1;
                }
                Err(e) => {
                    error!("GitSyncWorker: Failed to sync remote {}: {}", remote, e);
                    failed += 1;
                    record_failure(&e.to_string());
                }
            }
        }

        // A cycle that reaches no remote it tried is a condition of
        // the repository or the network rather than of any one
        // remote, and the per-remote errors above do not say so
        // anywhere.  A cycle that tried nothing because every remote
        // is backing off is the same outage, still unresolved.  The
        // fetches that failed were not this worker's own.  Those are
        // an hour old by its next cycle, past the backoff window, so
        // a review or a baseline lookup failed them within the hour.
        // That cycle has no failure to quote and the network may have
        // recovered since, so it logs at warn.
        let tried = fetched + stale + failed;
        if fetched == 0 && tried > 0 {
            error!(
                "GitSyncWorker: Sync cycle fetched nothing: {} skipped, {} backing off, {} stale, {} failed. First failure: {}",
                skipped, backed_off, stale, failed, first_failure
            );
        } else if fetched == 0 && backed_off > 0 {
            warn!(
                "GitSyncWorker: Sync cycle fetched nothing: {} skipped, {} backing off.",
                skipped, backed_off
            );
        } else {
            info!(
                "GitSyncWorker: Sync cycle complete: {} fetched, {} skipped, {} backing off, {} stale, {} failed.",
                fetched, skipped, backed_off, stale, failed
            );
        }
        Ok(())
    }
}
