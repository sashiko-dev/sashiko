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

use super::{BaselineAttempt, GitWorktree, ReviewContext};
use crate::db::{Database, PatchsetRow};
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::time::Instant;
use tracing::warn;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(120);
const PROGRESS_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_secs(5);

pub(super) type PatchDiff = (i64, i64, String, String, String, i64, String);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Deadline {
    Running(Instant),
    Paused,
}

/// Cleanup has its own deadline, including cleanup between baseline candidates.
/// A watch channel lets the outer attempt timer suspend without borrowing the
/// preparation state that the attempt itself is updating.
#[derive(Clone)]
struct AttemptBudget(tokio::sync::watch::Sender<Deadline>);

impl AttemptBudget {
    fn new() -> Self {
        Self(tokio::sync::watch::channel(Deadline::Paused).0)
    }

    fn start(&self, duration: Duration) -> Result<()> {
        let deadline = Instant::now()
            .checked_add(duration)
            .ok_or_else(|| anyhow!("Preparation timeout is too large"))?;
        self.0.send_replace(Deadline::Running(deadline));
        Ok(())
    }

    fn pause(&self) -> PausedBudget {
        let remaining = match self.0.send_replace(Deadline::Paused) {
            Deadline::Running(deadline) => Some(deadline.saturating_duration_since(Instant::now())),
            Deadline::Paused => None,
        };
        PausedBudget(self.clone(), remaining)
    }

    async fn expired(&self) {
        let mut updates = self.0.subscribe();
        loop {
            let state = *updates.borrow_and_update();
            match state {
                Deadline::Running(deadline) => tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        if *updates.borrow() == state {
                            return;
                        }
                    }
                    _ = updates.changed() => {}
                },
                Deadline::Paused => {
                    let _ = updates.changed().await;
                }
            }
        }
    }
}

struct PausedBudget(AttemptBudget, Option<Duration>);

impl Drop for PausedBudget {
    fn drop(&mut self) {
        if let Some(remaining) = self.1 {
            let deadline = Instant::now()
                .checked_add(remaining)
                .unwrap_or_else(Instant::now);
            self.0.0.send_replace(Deadline::Running(deadline));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stage {
    LoadingInputs,
    SelectingBaselines,
    ResolvingPrerequisites,
    FetchingBaseline,
    ResolvingCommit,
    CheckingBaseline,
    CreatingWorktree,
    ApplyingPrerequisites,
    ApplyingPatches,
    RecordingBaseline,
    CleaningUp,
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Self::LoadingInputs => "Loading preparation inputs",
            Self::SelectingBaselines => "Selecting candidate baselines",
            Self::ResolvingPrerequisites => "Resolving prerequisites",
            Self::FetchingBaseline => "Fetching a baseline",
            Self::ResolvingCommit => "Resolving a baseline commit",
            Self::CheckingBaseline => "Checking a baseline",
            Self::CreatingWorktree => "Creating a worktree",
            Self::ApplyingPrerequisites => "Applying prerequisites",
            Self::ApplyingPatches => "Applying target patches",
            Self::RecordingBaseline => "Recording the selected baseline",
            Self::CleaningUp => "Cleaning up preparation resources",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Running,
    RetryWaiting,
    Ready,
    NoBaseline,
    Failed,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "Preparing",
            Self::RetryWaiting => "Retrying",
            Self::Ready => "Ready",
            Self::NoBaseline => "Failed To Apply",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Serialize)]
struct TimedOutAttempt {
    attempt: u64,
    stage: Stage,
    stage_label: &'static str,
    timed_out_at: i64,
}

#[derive(Serialize)]
struct Progress {
    status: Status,
    stage: Stage,
    stage_label: &'static str,
    attempt: u64,
    total_attempts: u64,
    timeout_seconds: u64,
    attempt_started_at: i64,
    stage_started_at: i64,
    finished_at: Option<i64>,
    retry_at: Option<i64>,
    timed_out_attempts: Vec<TimedOutAttempt>,
}

/// The worktree is owned outside the future bounded by the attempt deadline.
/// A cancelled populate/apply therefore leaves an explicitly removable checkout.
pub(super) struct Preparation {
    pub(super) worktree: Option<GitWorktree>,
    pub(super) attempts: Vec<BaselineAttempt>,
    db: Arc<Database>,
    patchset_id: i64,
    progress: Progress,
    publish: bool,
    cleanup_timeout: Duration,
    budget: AttemptBudget,
    remote_writes: crate::git_ops::PendingWrites,
}

impl Preparation {
    pub(super) fn new(ctx: &ReviewContext, patchset_id: i64) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            worktree: None,
            attempts: Vec::new(),
            db: ctx.db.clone(),
            patchset_id,
            progress: Progress {
                status: Status::Running,
                stage: Stage::LoadingInputs,
                stage_label: Stage::LoadingInputs.label(),
                attempt: 1,
                total_attempts: u64::from(ctx.settings.review.preparation_max_retries) + 1,
                timeout_seconds: ctx.settings.review.preparation_timeout_seconds.get(),
                attempt_started_at: now,
                stage_started_at: now,
                finished_at: None,
                retry_at: None,
                timed_out_attempts: Vec::new(),
            },
            publish: true,
            cleanup_timeout: CLEANUP_TIMEOUT,
            budget: AttemptBudget::new(),
            remote_writes: crate::git_ops::PendingWrites::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn unreported(ctx: &ReviewContext, patchset_id: i64) -> Self {
        let mut preparation = Self::new(ctx, patchset_id);
        preparation.publish = false;
        preparation
    }

    pub(super) async fn stage(&mut self, stage: Stage) {
        self.progress.stage = stage;
        self.progress.stage_label = stage.label();
        self.progress.stage_started_at = chrono::Utc::now().timestamp();
        self.persist().await;
    }

    pub(super) fn worktree(&self) -> Result<&GitWorktree> {
        self.worktree
            .as_ref()
            .ok_or_else(|| anyhow!("Preparation worktree is missing"))
    }

    pub(super) fn logs(&self) -> String {
        // Legacy baseline attempt entries retain their shape and position.
        let mut entries: Vec<_> = self
            .attempts
            .iter()
            .map(|attempt| serde_json::json!(attempt))
            .collect();
        if self.publish {
            entries.push(serde_json::json!({
                "baseline": "Preparation",
                "status": self.progress.status.label(),
                "log": format!("{}; attempt {} of {}.", self.progress.stage.label(),
                    self.progress.attempt, self.progress.total_attempts),
                "preparation": self.progress,
            }));
        }
        serde_json::Value::Array(entries).to_string()
    }

    async fn persist(&self) {
        if !self.publish {
            return;
        }
        let logs = self.logs();
        match tokio::time::timeout(
            PROGRESS_WRITE_TIMEOUT,
            self.db
                .update_patchset_preparation_logs(self.patchset_id, &logs),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!("Failed to save preparation progress: {error:#}"),
            Err(_) => warn!("Timed out saving preparation progress"),
        }
    }

    pub(super) async fn cleanup(&mut self) -> Result<()> {
        let _paused = self.budget.pause();
        let previous_stage = self.progress.stage;
        let previous_started = self.progress.stage_started_at;
        let result = tokio::time::timeout(self.cleanup_timeout, async {
            self.stage(Stage::CleaningUp).await;
            // No await separates taking ownership from entering remove.
            if let Some(worktree) = self.worktree.take() {
                worktree.remove().await?;
            }
            self.remote_writes.finish().await?;
            Ok(())
        })
        .await
        .map_err(|_| anyhow!("Preparation cleanup timed out"))?;
        if result.is_ok() {
            // The resumed attempt deadline must not attribute a later timeout
            // to cleanup that already completed within its separate budget.
            self.progress.stage = previous_stage;
            self.progress.stage_label = previous_stage.label();
            self.progress.stage_started_at = previous_started;
        }
        result
    }

    async fn finish(&mut self, status: Status) {
        self.progress.status = status;
        self.progress.finished_at = Some(chrono::Utc::now().timestamp());
        self.progress.retry_at = None;
        self.persist().await;
    }
}

pub(super) struct PreparedPatchset {
    pub(super) diffs: Vec<PatchDiff>,
    pub(super) found_baseline: Option<(i64, GitWorktree, String)>,
    pub(super) patch_commits: HashMap<i64, String>,
    pub(super) logs: String,
}

#[derive(Debug)]
enum Failure {
    TimedOut,
    Cleanup,
    Operation(Stage),
    Cancelled,
}

impl Failure {
    fn message(&self, state: &Preparation) -> String {
        match self {
            Self::TimedOut => format!(
                "Review preparation timed out after {} attempt{} ({}s per attempt).",
                state.progress.attempt,
                if state.progress.attempt == 1 { "" } else { "s" },
                state.progress.timeout_seconds
            ),
            Self::Cleanup => "Review preparation stopped because cleanup failed or timed out. No further attempts were started.".into(),
            Self::Operation(stage) => format!("Review preparation failed while {}.", stage.label().to_ascii_lowercase()),
            Self::Cancelled => "Review preparation was cancelled or superseded.".into(),
        }
    }
}

/// Own the attempt state outside timeout so cancellation does not drop its
/// worktree before the cleanup budget can be applied.
pub(super) async fn prepare_patchset(
    ctx: &ReviewContext,
    patchset: &PatchsetRow,
) -> Option<PreparedPatchset> {
    let mut state = Preparation::new(ctx, patchset.id);
    prepare_with_state(ctx, patchset, &mut state).await
}

async fn prepare_with_state(
    ctx: &ReviewContext,
    patchset: &PatchsetRow,
    state: &mut Preparation,
) -> Option<PreparedPatchset> {
    let writes = state.remote_writes.clone();
    match writes
        .scope(crate::git_cmd::with_isolated_processes(run_attempts(
            ctx, patchset, state,
        )))
        .await
    {
        Ok((diffs, baseline, patch_commits)) => {
            state
                .finish(if baseline.is_some() {
                    Status::Ready
                } else {
                    Status::NoBaseline
                })
                .await;
            let found_baseline =
                baseline.and_then(|(id, sha)| state.worktree.take().map(|wt| (id, wt, sha)));
            Some(PreparedPatchset {
                diffs,
                found_baseline,
                patch_commits,
                logs: state.logs(),
            })
        }
        Err(failure) => {
            if !matches!(failure, Failure::Cancelled) {
                state.finish(Status::Failed).await;
                super::Reviewer::mark_patchset_failed_unless_cancelled(
                    &ctx.db,
                    patchset.id,
                    &failure.message(state),
                )
                .await;
            } else {
                let _ = ctx.db.release_patchset_claim().await;
            }
            None
        }
    }
}

type Prepared = (Vec<PatchDiff>, Option<(i64, String)>, HashMap<i64, String>);

async fn run_attempts(
    ctx: &ReviewContext,
    patchset: &PatchsetRow,
    state: &mut Preparation,
) -> std::result::Result<Prepared, Failure> {
    let budget = Duration::from_secs(state.progress.timeout_seconds);
    loop {
        require_active(ctx, patchset.id).await?;
        state.progress.status = Status::Running;
        state.progress.retry_at = None;
        state.progress.attempt_started_at = chrono::Utc::now().timestamp();
        state
            .budget
            .start(budget)
            .map_err(|_| Failure::Operation(Stage::LoadingInputs))?;
        let timer = state.budget.clone();
        let result = tokio::select! {
            biased;
            _ = timer.expired() => None,
            result = prepare_once(ctx, patchset, state) => Some(result),
        };
        match result {
            Some(Ok(prepared)) => return Ok(prepared),
            Some(Err(error)) => {
                let stage = state.progress.stage;
                warn!("Review preparation failed at {stage:?}: {error:#}");
                if stage == Stage::CleaningUp {
                    return Err(Failure::Cleanup);
                }
                state.cleanup().await.map_err(|_| Failure::Cleanup)?;
                return Err(Failure::Operation(stage));
            }
            None => {
                let stage = state.progress.stage;
                state.progress.timed_out_attempts.push(TimedOutAttempt {
                    attempt: state.progress.attempt,
                    stage,
                    stage_label: stage.label(),
                    timed_out_at: chrono::Utc::now().timestamp(),
                });
                state.cleanup().await.map_err(|_| Failure::Cleanup)?;
                require_active(ctx, patchset.id).await?;
                if state.progress.attempt == state.progress.total_attempts {
                    return Err(Failure::TimedOut);
                }
                let delay = RETRY_DELAY * state.progress.attempt.min(6) as u32;
                state.progress.status = Status::RetryWaiting;
                state.progress.retry_at =
                    Some(chrono::Utc::now().timestamp() + delay.as_secs() as i64);
                state.persist().await;
                tokio::time::sleep(delay).await;
                state.progress.attempt += 1;
            }
        }
    }
}

async fn require_active(ctx: &ReviewContext, patchset_id: i64) -> std::result::Result<(), Failure> {
    match tokio::time::timeout(
        PROGRESS_WRITE_TIMEOUT,
        ctx.db.is_patchset_claim_active(patchset_id),
    )
    .await
    {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(Failure::Cancelled),
        _ => Err(Failure::Operation(Stage::LoadingInputs)),
    }
}

async fn prepare_once(
    ctx: &ReviewContext,
    patchset: &PatchsetRow,
    state: &mut Preparation,
) -> Result<Prepared> {
    state.stage(Stage::LoadingInputs).await;
    let diffs = ctx.db.get_patch_diffs(patchset.id).await?;
    let mid = patchset
        .message_id
        .as_deref()
        .or_else(|| diffs.first().map(|p| p.6.as_str()));
    let body = match mid {
        Some(mid) => ctx.db.get_message_body(mid).await?,
        None => None,
    };
    let forced = match patchset.baseline_id {
        Some(id) => ctx
            .db
            .get_baseline_commit(id)
            .await?
            .and_then(|sha| super::CommitId::parse(&sha)),
        None => None,
    };
    state.stage(Stage::SelectingBaselines).await;
    let candidates = if let Some(commit) = forced {
        vec![super::BaselineResolution::Commit(commit)]
    } else {
        let mut files: Vec<_> = diffs
            .iter()
            .flat_map(|p| super::extract_files_from_diff(&p.2))
            .collect();
        files.sort();
        files.dedup();
        ctx.baseline_registry
            .resolve_candidates(
                &files,
                patchset.subject.as_deref().unwrap_or("Unknown"),
                body.as_deref(),
            )
            .await
    };
    let (baseline, commits) = super::Reviewer::prepare_baseline_attempts(
        ctx,
        patchset.id,
        &candidates,
        &diffs,
        patchset.message_id.as_deref(),
        body.as_deref(),
        state,
    )
    .await?;
    Ok((diffs, baseline, commits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviewer::tests::{git_in, stale_head_context};
    use std::path::{Path, PathBuf};

    const DIFF: &str =
        "diff --git a/value b/value\n--- a/value\n+++ b/value\n@@ -1 +1 @@\n-old\n+new\n";

    struct Fixture {
        root: tempfile::TempDir,
        repo: PathBuf,
        ctx: ReviewContext,
        row: PatchsetRow,
        db: Arc<Database>,
    }

    async fn fixture(diff: &str, retries: u32) -> Result<Fixture> {
        series_fixture(&[diff], retries).await
    }

    async fn series_fixture(diffs: &[&str], retries: u32) -> Result<Fixture> {
        let root = tempfile::tempdir()?;
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo)?;
        git_in(&repo, &["init", "-q", "-b", "main"]);
        git_in(&repo, &["config", "user.name", "Test"]);
        git_in(&repo, &["config", "user.email", "test@example.com"]);
        std::fs::write(repo.join("value"), "old\n")?;
        git_in(&repo, &["add", "value"]);
        git_in(&repo, &["commit", "-q", "-s", "-m", "base"]);
        let mut ctx = stale_head_context(root.path(), &repo).await?;
        ctx.settings.review.preparation_timeout_seconds = std::num::NonZeroU64::new(60).unwrap();
        ctx.settings.review.preparation_max_retries = retries;
        let db = ctx.db.clone();
        let base = crate::git_ops::get_commit_hash(&repo, "HEAD").await?;
        let bid = db.create_baseline(None, None, Some(&base)).await?;
        let thread = db
            .create_thread("prepare@example.com", "[PATCH] test", 1_000_000_000)
            .await?;
        db.create_message(
            "prepare@example.com",
            thread,
            None,
            "Test <test@example.com>",
            "[PATCH] test",
            1_000_000_000,
            diffs[0],
            "list@example.com",
            "",
            None,
            None,
        )
        .await?;
        let id = db
            .create_patchset(
                thread,
                None,
                "prepare@example.com",
                "[PATCH] test",
                "Test <test@example.com>",
                1_000_000_000,
                diffs.len() as u32,
                1,
                "list@example.com",
                "",
                None,
                1,
                Some(bid),
                false,
                None,
                None,
            )
            .await?
            .unwrap();
        db.create_patch(id, "prepare@example.com", 1, diffs[0])
            .await?;
        for (index, diff) in diffs.iter().enumerate().skip(1) {
            let mid = format!("prepare-{}@example.com", index + 1);
            db.create_message(
                &mid,
                thread,
                Some("prepare@example.com"),
                "Test <test@example.com>",
                "[PATCH] next",
                1_000_000_000,
                diff,
                "list@example.com",
                "",
                None,
                None,
            )
            .await?;
            db.create_patch(id, &mid, (index + 1) as u32, diff).await?;
        }
        db.update_patchset_status(id, "Pending").await?;
        let row = db
            .get_pending_patchsets(10)
            .await?
            .into_iter()
            .find(|p| p.id == id)
            .unwrap();
        let generation = db.claim_patchset_for_review_with_gen(id).await?.unwrap();
        ctx.db = Arc::new(db.with_patchset_claim(id, generation));
        Ok(Fixture {
            root,
            repo,
            ctx,
            row,
            db,
        })
    }

    fn stall_hook(f: &Fixture, once: bool) {
        let hook = f.repo.join(".git/hooks/applypatch-msg");
        let condition = if once {
            format!(
                "mkdir '{}' 2>/dev/null",
                f.root.path().join("once").display()
            )
        } else {
            "true".into()
        };
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" >> '{}'\nif {condition}; then\nprintf '%s\\n' \"$$\" >> '{}'\nprintf 'PRIVATE_TOKEN\\n' >&2\nexec sleep 300\nfi\nexit 0\n",
            f.root.path().join("paths").display(),
            f.root.path().join("pids").display()
        );
        crate::test_support::write_executable_script(&hook, &script);
    }

    fn progress(logs: &str) -> serde_json::Value {
        serde_json::from_str::<Vec<serde_json::Value>>(logs)
            .unwrap()
            .pop()
            .unwrap()["preparation"]
            .clone()
    }

    async fn stored(f: &Fixture) -> Result<serde_json::Value> {
        Ok(f.db
            .get_patchset_summary(f.row.id, None, None)
            .await?
            .unwrap())
    }

    async fn wait_for_lines(path: &Path, count: usize) {
        tokio::time::timeout(Duration::from_secs(60), async {
            while std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .count()
                < count
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    fn expire(budget: &AttemptBudget) {
        budget.0.send_replace(Deadline::Running(Instant::now()));
    }

    async fn assert_hook_exited(f: &Fixture) -> Result<()> {
        let pids = std::fs::read_to_string(f.root.path().join("pids"))?;
        for pid in pids.lines() {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let status = std::fs::read_to_string(format!("/proc/{pid}/stat"));
                    if match status {
                        Err(_) => true,
                        Ok(s) => s
                            .rsplit_once(") ")
                            .is_some_and(|(_, rest)| rest.starts_with('Z')),
                    } {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn applying_patches_is_published_once_for_a_multi_patch_series() -> Result<()> {
        let second = DIFF.replace("-old", "-new").replace("+new", "+final");
        let f = series_fixture(&[DIFF, &second], 0).await?;
        f.db.execute_batch(
            "CREATE TABLE preparation_writes (logs BLOB);
             CREATE TRIGGER record_preparation_write AFTER UPDATE OF baseline_logs ON patchsets
             BEGIN INSERT INTO preparation_writes VALUES (NEW.baseline_logs); END;",
        )
        .await?;
        let prepared = prepare_patchset(&f.ctx, &f.row).await.unwrap();
        assert_eq!(prepared.patch_commits.len(), 2);
        let (_, worktree, _) = prepared.found_baseline.unwrap();
        assert_eq!(
            std::fs::read_to_string(worktree.path.join("value"))?,
            "final\n"
        );
        worktree.remove().await?;

        let mut rows =
            f.db.conn
                .query("SELECT logs FROM preparation_writes", ())
                .await?;
        let mut application_updates = 0;
        while let Some(row) = rows.next().await? {
            let logs = crate::compression::get_compressed_string(&row, 0)?;
            let p = progress(&logs);
            if p["stage"] == "applying_patches" && p["status"] == "running" {
                application_updates += 1;
            }
        }
        // A single transition keeps the stage's timestamp stable for the
        // whole application loop and avoids one database write per patch.
        assert_eq!(application_updates, 1);
        Ok(())
    }

    #[tokio::test]
    async fn timeout_retries_with_fresh_worktree_and_retains_history() -> Result<()> {
        let f = fixture(DIFF, 1).await?;
        stall_hook(&f, true);
        let mut state = Preparation::new(&f.ctx, f.row.id);
        let budget = state.budget.clone();
        let review = prepare_with_state(&f.ctx, &f.row, &mut state);
        let observe = async {
            wait_for_lines(&f.root.path().join("pids"), 1).await;
            let details = stored(&f).await?;
            let p = progress(details["baseline_logs"].as_str().unwrap());
            assert_eq!(p["stage"], "applying_patches");
            assert_eq!(p["status"], "running");
            // Progress must not wipe the explicit baseline supplied at ingestion.
            assert!(details["baseline"].is_object());
            // Expire only after observing the running hook and its progress.
            expire(&budget);
            Ok::<_, anyhow::Error>(())
        };
        let (result, observed) = tokio::join!(review, observe);
        observed?;
        let prepared = result.expect("second preparation succeeds");
        let p = progress(&prepared.logs);
        assert_eq!(p["status"], "ready", "{}", prepared.logs);
        assert_eq!(p["attempt"], 2);
        assert_eq!(p["timed_out_attempts"][0]["stage"], "applying_patches");
        assert!(!prepared.logs.contains("PRIVATE_TOKEN"));
        assert!(
            !prepared
                .logs
                .contains(&f.root.path().to_string_lossy().to_string())
        );
        let paths = std::fs::read_to_string(f.root.path().join("paths"))?;
        let paths: Vec<_> = paths.lines().collect();
        assert_eq!(paths.len(), 2);
        assert_ne!(paths[0], paths[1]);
        assert!(!Path::new(paths[0]).exists());
        let (_, worktree, _) = prepared.found_baseline.unwrap();
        assert_eq!(
            std::fs::read_to_string(worktree.path.join("value"))?,
            "new\n"
        );
        worktree.remove().await?;
        assert_hook_exited(&f).await?;
        Ok(())
    }

    #[tokio::test]
    async fn exhausted_timeouts_fail_with_a_fixed_reason() -> Result<()> {
        for retries in [0, 1] {
            let f = fixture(DIFF, retries).await?;
            stall_hook(&f, false);
            let mut state = Preparation::new(&f.ctx, f.row.id);
            let budget = state.budget.clone();
            let expire_attempts = async {
                for attempt in 1..=retries + 1 {
                    wait_for_lines(&f.root.path().join("pids"), attempt as usize).await;
                    expire(&budget);
                }
            };
            let (result, ()) = tokio::join!(
                prepare_with_state(&f.ctx, &f.row, &mut state),
                expire_attempts
            );
            assert!(result.is_none());
            let details = stored(&f).await?;
            assert_eq!(details["status"], "Failed");
            assert_eq!(
                details["failed_reason"],
                format!(
                    "Review preparation timed out after {} attempt{} (60s per attempt).",
                    retries + 1,
                    if retries == 0 { "" } else { "s" }
                )
            );
            let p = progress(details["baseline_logs"].as_str().unwrap());
            assert_eq!(p["status"], "failed");
            assert_eq!(p["attempt"], retries + 1);
            assert_eq!(
                p["timed_out_attempts"].as_array().unwrap().len(),
                (retries + 1) as usize
            );
            assert!(!details.to_string().contains("PRIVATE_TOKEN"));
            assert_eq!(
                std::fs::read_dir(f.root.path().join("worktrees"))?.count(),
                0
            );
            assert_hook_exited(&f).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_application_failure_is_not_retried() -> Result<()> {
        let f = fixture(&DIFF.replace("-old", "-mismatch"), 3).await?;
        let prepared = prepare_patchset(&f.ctx, &f.row).await.unwrap();
        assert!(prepared.found_baseline.is_none());
        let p = progress(&prepared.logs);
        assert_eq!(p["status"], "no_baseline");
        assert_eq!(p["attempt"], 1);
        assert_eq!(p["timed_out_attempts"], serde_json::json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_budget_includes_the_worktree_lock() -> Result<()> {
        let f = fixture(DIFF, 1).await?;
        let mut preparation = Preparation::unreported(&f.ctx, f.row.id);
        preparation.cleanup_timeout = Duration::from_millis(20);
        let mut worktree = GitWorktree::new(&f.repo, "HEAD", None).await?;
        let path = worktree.path.clone();
        // Finish blocking-pool deletion before testing the metadata-lock wait.
        worktree.remove_directory().await?;
        preparation.worktree = Some(worktree);
        let lock = crate::git_ops::get_worktree_lock();
        let guard = lock.lock().await;
        assert!(preparation.cleanup().await.is_err());
        assert_eq!(preparation.progress.stage, Stage::CleaningUp);
        drop(guard);
        crate::git_ops::prune_worktrees(&f.repo).await?;
        assert!(!path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn candidate_cleanup_cannot_be_interrupted_by_the_attempt_deadline() -> Result<()> {
        let f = fixture(DIFF, 1).await?;
        let mut state = Preparation::unreported(&f.ctx, f.row.id);
        state.progress.stage = Stage::ApplyingPatches;
        let mut worktree = GitWorktree::new(&f.repo, "HEAD", None).await?;
        worktree.remove_directory().await?;
        state.worktree = Some(worktree);
        let lock = crate::git_ops::get_worktree_lock();
        let guard = lock.lock().await;
        state.budget.start(Duration::from_secs(60))?;
        let timer = state.budget.clone();
        // All filesystem deletion is finished; only the controlled lock wait
        // remains when virtual time crosses the preparation deadline.
        tokio::time::pause();
        let cleanup = async {
            tokio::select! {
                biased;
                _ = timer.expired() => panic!("attempt interrupted cleanup"),
                result = state.cleanup() => result,
            }
        };
        let release = async {
            tokio::task::yield_now().await;
            assert!(matches!(*timer.0.borrow(), Deadline::Paused));
            tokio::time::advance(Duration::from_secs(61)).await;
            tokio::time::resume();
            drop(guard);
        };
        let (result, ()) = tokio::join!(cleanup, release);
        result?;
        assert_eq!(state.progress.stage, Stage::ApplyingPatches);
        assert!(state.worktree.is_none());
        assert!(
            matches!(*timer.0.borrow(), Deadline::Running(deadline) if deadline > Instant::now())
        );
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_failure_stops_timeout_retries() -> Result<()> {
        let f = fixture(DIFF, 3).await?;
        stall_hook(&f, false);
        let mut state = Preparation::new(&f.ctx, f.row.id);
        let budget = state.budget.clone();
        let review = prepare_with_state(&f.ctx, &f.row, &mut state);
        let obstruct_cleanup = async {
            wait_for_lines(&f.root.path().join("pids"), 1).await;
            let paths = std::fs::read_to_string(f.root.path().join("paths"))?;
            let path = Path::new(paths.lines().next().unwrap());
            std::fs::remove_dir_all(path)?;
            std::fs::write(path, b"not a directory")?;
            expire(&budget);
            Ok::<_, anyhow::Error>(())
        };
        let (prepared, obstructed) = tokio::join!(review, obstruct_cleanup);
        obstructed?;
        assert!(prepared.is_none());
        let details = stored(&f).await?;
        assert_eq!(details["status"], "Failed");
        assert_eq!(
            details["failed_reason"],
            Failure::Cleanup.message(&Preparation::new(&f.ctx, f.row.id))
        );
        let p = progress(details["baseline_logs"].as_str().unwrap());
        assert_eq!(p["status"], "failed");
        assert_eq!(p["attempt"], 1);
        assert_eq!(
            std::fs::read_to_string(f.root.path().join("paths"))?
                .lines()
                .count(),
            1
        );
        assert_hook_exited(&f).await?;
        Ok(())
    }

    #[tokio::test]
    async fn baseline_recording_error_is_a_preparation_failure() -> Result<()> {
        let f = fixture(DIFF, 3).await?;
        f.db.execute_batch(
            "CREATE TRIGGER reject_baseline BEFORE INSERT ON baselines
            BEGIN SELECT RAISE(FAIL, 'PRIVATE_DATABASE_DIAGNOSTIC'); END;",
        )
        .await?;
        assert!(prepare_patchset(&f.ctx, &f.row).await.is_none());
        let details = stored(&f).await?;
        assert_eq!(details["status"], "Failed");
        assert_eq!(
            details["failed_reason"],
            "Review preparation failed while recording the selected baseline."
        );
        assert!(!details.to_string().contains("PRIVATE_DATABASE_DIAGNOSTIC"));
        assert_eq!(
            std::fs::read_dir(f.root.path().join("worktrees"))?.count(),
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_and_replacement_prevent_retries_and_stale_progress() -> Result<()> {
        for replace in [false, true] {
            let f = fixture(DIFF, 1).await?;
            stall_hook(&f, false);
            let mut state = Preparation::new(&f.ctx, f.row.id);
            let budget = state.budget.clone();
            let review = prepare_with_state(&f.ctx, &f.row, &mut state);
            let cancel = async {
                wait_for_lines(&f.root.path().join("pids"), 1).await;
                if replace {
                    f.db.rerun_patchset(f.row.id).await?;
                    let generation =
                        f.db.claim_patchset_for_review_with_gen(f.row.id)
                            .await?
                            .unwrap();
                    f.db.with_patchset_claim(f.row.id, generation)
                        .update_patchset_preparation_logs(f.row.id, "[]")
                        .await?;
                } else {
                    f.db.cancel_patchset(f.row.id, true).await?;
                }
                expire(&budget);
                Ok::<_, anyhow::Error>(())
            };
            let (result, cancelled) = tokio::join!(review, cancel);
            cancelled?;
            assert!(result.is_none());
            let details = stored(&f).await?;
            assert_eq!(
                details["status"],
                if replace { "In Review" } else { "Cancelled" }
            );
            if replace {
                assert_eq!(details["baseline_logs"], "[]");
            }
            assert_eq!(
                std::fs::read_to_string(f.root.path().join("paths"))?
                    .lines()
                    .count(),
                1
            );
            assert_hook_exited(&f).await?;
        }
        Ok(())
    }
}
