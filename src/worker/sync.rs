use crate::git_ops::{BackoffError, FetchOutcome, refresh_remote};
use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;
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
                error!(
                    "GitSyncWorker failed during sync cycle: {}",
                    crate::utils::redact_secret(&e.to_string())
                );
            }
            // Sleep for 1 hour before checking again.
            // refresh_remote applies the per-remote fetch interval, the
            // hour of backoff after a failure, and the day-long fallback
            // window on the local ref.
            sleep(Duration::from_secs(3600)).await;
        }
    }

    async fn sync_all_remotes(&self) -> Result<()> {
        info!("GitSyncWorker: Starting sync cycle.");

        // Enumerate all configured remotes
        let output = crate::git_cmd::in_dir_async(&self.repo_path)
            .args(["remote"])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = crate::utils::redact_secret(&String::from_utf8_lossy(&output.stderr));
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
            let url_output = crate::git_cmd::in_dir_async(&self.repo_path)
                .args(["remote", "get-url", remote])
                .output()
                .await?;

            if !url_output.status.success() {
                let stderr =
                    crate::utils::redact_secret(&String::from_utf8_lossy(&url_output.stderr));
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
            // force_fetch=false so the fetch interval in refresh_remote holds
            match refresh_remote(&self.repo_path, remote, &url, false).await {
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
                    let message = crate::utils::redact_secret(&e.to_string());
                    error!(
                        "GitSyncWorker: Failed to sync remote {}: {}",
                        remote, message
                    );
                    failed += 1;
                    record_failure(&message);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber;

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = BufferWriter;

        fn make_writer(&'a self) -> Self::Writer {
            BufferWriter(Arc::clone(&self.0))
        }
    }

    #[tokio::test]
    async fn test_sync_all_remotes_redacts_secrets_in_logs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo_path = dir.path().to_path_buf();

        let init = crate::git_cmd::in_dir_async(&repo_path)
            .args(["init"])
            .output()
            .await?;
        assert!(init.status.success(), "git init failed");

        let secret_url =
            "https://myuser:secret_token_123@127.0.0.1:0/repo.git?token=api_secret_456";
        let add = crate::git_cmd::in_dir_async(&repo_path)
            .args(["remote", "add", "secret-remote", secret_url])
            .output()
            .await?;
        assert!(add.status.success(), "git remote add failed");

        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish();

        let worker = GitSyncWorker::new(repo_path);
        worker
            .sync_all_remotes()
            .with_subscriber(subscriber)
            .await?;

        let logs = String::from_utf8_lossy(&buffer.0.lock().unwrap()).into_owned();
        assert!(
            logs.contains("GitSyncWorker: Failed to sync remote secret-remote:"),
            "expected failure log in: {logs}"
        );
        assert!(
            logs.contains("[REDACTED]"),
            "expected redacted marker in: {logs}"
        );
        assert!(
            !logs.contains("secret_token_123"),
            "secret password leaked in logs: {logs}"
        );
        assert!(
            !logs.contains("myuser"),
            "secret username leaked in logs: {logs}"
        );
        assert!(
            !logs.contains("api_secret_456"),
            "secret token parameter leaked in logs: {logs}"
        );

        // Also verify redaction when `git remote` itself fails with a secret in stderr.
        // Writing a non-`gitdir:` content into `.git` triggers `invalid gitfile format: <path>`,
        // which echoes the `.git` file path across all Git versions (including Git 2.55+, where
        // a nonexistent `gitdir:` target prints `not a git repository: (null)`).
        let bad_dir = tempfile::tempdir()?;
        let sub_dir = bad_dir.path().join("token=api_secret_456");
        std::fs::create_dir(&sub_dir)?;
        std::fs::write(sub_dir.join(".git"), "invalid-gitfile\n")?;
        let bad_buffer = LogBuffer::default();
        let bad_subscriber = tracing_subscriber::fmt()
            .with_writer(bad_buffer.clone())
            .with_ansi(false)
            .finish();
        let bad_worker = GitSyncWorker::new(sub_dir);
        let res = bad_worker
            .sync_all_remotes()
            .with_subscriber(bad_subscriber)
            .await;
        assert!(res.is_err());

        let bad_logs = String::from_utf8_lossy(&bad_buffer.0.lock().unwrap()).into_owned();
        assert!(
            bad_logs.contains("GitSyncWorker: Failed to list git remotes:"),
            "expected list remotes failure log in: {bad_logs}"
        );
        assert!(
            bad_logs.contains("[REDACTED]"),
            "expected redacted marker in: {bad_logs}"
        );
        assert!(
            !bad_logs.contains("api_secret_456"),
            "secret token parameter leaked in list remotes logs: {bad_logs}"
        );

        Ok(())
    }
}
