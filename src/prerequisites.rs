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

use crate::db::Database;
use crate::mbox::{LoreMboxClient, split_mbox};
use crate::patch::parse_email;
use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tracing::info;

const MAX_MBOX_MESSAGES: usize = 500;
const MAX_PREREQUISITE_PATCH_IDS: usize = 128;
const MAX_LORE_SEARCHES: usize = 8;
const MAX_CONCURRENT_GIT_PATCH_IDS: usize = 8;
const GIT_PATCH_ID_TIMEOUT: Duration = Duration::from_secs(30);

static GIT_PATCH_ID_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrerequisitePatch {
    pub(crate) git_patch_id: String,
    pub(crate) message_id: String,
    pub(crate) subject: String,
    pub(crate) author: String,
    pub(crate) date: i64,
    pub(crate) diff: String,
}

pub(crate) fn parse_prerequisite_patch_ids(body: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();

    for line in body.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("prerequisite-patch-id") {
            continue;
        }

        let id = value.trim();
        if id.len() != 40 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }

        let id = id.to_ascii_lowercase();
        if seen.insert(id.clone()) {
            if ids.len() == MAX_PREREQUISITE_PATCH_IDS {
                bail!(
                    "b4 metadata contains more than {MAX_PREREQUISITE_PATCH_IDS} unique prerequisite patch IDs"
                );
            }
            ids.push(id);
        }
    }

    Ok(ids)
}

async fn run_git_patch_id_command(
    mut command: tokio::process::Command,
    input: &[u8],
    time_limit: Duration,
) -> Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start git patch-id")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("git patch-id stdin was not piped"))?;

    let operation = async move {
        let write_stdin = async move {
            let result = stdin.write_all(input).await;
            drop(stdin);
            result
        };
        let wait_for_output = async move {
            child
                .wait_with_output()
                .await
                .context("failed to wait for git patch-id")
        };

        let (write_result, output_result) = tokio::join!(write_stdin, wait_for_output);
        let output = output_result?;
        if let Err(error) = write_result
            && (error.kind() != std::io::ErrorKind::BrokenPipe || output.status.success())
        {
            return Err(error).context("failed to write patch to git patch-id");
        }
        Ok::<_, anyhow::Error>(output)
    };

    tokio::time::timeout(time_limit, operation)
        .await
        .map_err(|_| anyhow!("git patch-id timed out after {time_limit:?}"))?
}

/// Calculates the stable Git patch ID for one email patch body.
pub async fn calculate_git_patch_id(diff: &str) -> Result<Option<String>> {
    if diff.trim().is_empty() {
        return Ok(None);
    }

    let _permit = GIT_PATCH_ID_SEMAPHORE
        .get_or_init(|| Semaphore::new(MAX_CONCURRENT_GIT_PATCH_IDS))
        .acquire()
        .await
        .map_err(|_| anyhow!("git patch-id concurrency limiter closed"))?;

    let mut command = crate::git_cmd::detached_async();
    command.args(["patch-id", "--stable"]);
    let output = run_git_patch_id_command(command, diff.as_bytes(), GIT_PATCH_ID_TIMEOUT).await?;
    if !output.status.success() {
        return Err(anyhow!(
            "git patch-id failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let Some(id) = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
    else {
        return Ok(None);
    };

    if id.len() == 40 && id.bytes().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(id))
    } else {
        Err(anyhow!("git patch-id returned an invalid patch ID"))
    }
}

async fn calculate_optional_git_patch_id(diff: Option<&str>) -> Result<Option<String>> {
    match diff {
        Some(diff) => calculate_git_patch_id(diff).await,
        None => Ok(None),
    }
}

/// Calculates stable IDs for a batch while preserving its input order.
///
/// Empty slots let callers align results with records that have no patch.
/// At most eight subprocesses run at once across the process.
pub async fn calculate_git_patch_id_batch(diffs: Vec<Option<&str>>) -> Vec<Result<Option<String>>> {
    let calculations = diffs.into_iter().map(calculate_optional_git_patch_id);
    futures::stream::iter(calculations)
        .buffered(MAX_CONCURRENT_GIT_PATCH_IDS)
        .collect()
        .await
}

async fn patches_from_mbox(raw: Vec<u8>) -> Result<Vec<PrerequisitePatch>> {
    let parsed = tokio::task::spawn_blocking(move || {
        let messages = split_mbox(&raw);
        if messages.len() > MAX_MBOX_MESSAGES {
            return Err(anyhow!(
                "lore mbox contains {} messages, exceeding the limit of {}",
                messages.len(),
                MAX_MBOX_MESSAGES
            ));
        }

        Ok::<_, anyhow::Error>(
            messages
                .into_iter()
                .filter_map(|message| parse_email(&message).ok())
                .filter(|(metadata, patch)| metadata.is_patch_or_cover && patch.is_some())
                .collect::<Vec<_>>(),
        )
    })
    .await
    .context("lore mbox parsing task failed")??;

    let patch_ids = calculate_git_patch_id_batch(
        parsed
            .iter()
            .map(|(_, patch)| patch.as_ref().map(|patch| patch.diff.as_str()))
            .collect(),
    )
    .await;
    let mut patches = Vec::new();
    let mut seen = HashSet::new();
    for ((metadata, patch), git_patch_id) in parsed.into_iter().zip(patch_ids) {
        let Some(patch) = patch else {
            continue;
        };
        let Some(git_patch_id) = git_patch_id? else {
            continue;
        };
        if !seen.insert(git_patch_id.clone()) {
            continue;
        }

        patches.push(PrerequisitePatch {
            git_patch_id,
            message_id: patch.message_id,
            subject: metadata.subject,
            author: metadata.author,
            date: metadata.date,
            diff: patch.diff,
        });
    }
    Ok(patches)
}

async fn resolve_prerequisite_patches<F, Fut>(
    db: &Database,
    patch_ids: &[String],
    mut fetch: F,
) -> Result<Vec<PrerequisitePatch>>
where
    F: FnMut(String) -> Fut + Send,
    Fut: Future<Output = Result<Vec<PrerequisitePatch>>> + Send,
{
    if patch_ids.len() > MAX_PREREQUISITE_PATCH_IDS {
        bail!("cannot resolve more than {MAX_PREREQUISITE_PATCH_IDS} prerequisite patch IDs");
    }

    let mut fetched = HashMap::new();
    let mut resolved = Vec::with_capacity(patch_ids.len());
    let mut lore_searches = 0;

    for patch_id in patch_ids {
        if let Some(patch) = fetched.get(patch_id).cloned() {
            resolved.push(patch);
            continue;
        }

        if let Some((message_id, diff, subject, author, date)) =
            db.get_patch_by_git_patch_id(patch_id).await?
        {
            info!(
                "Resolved prerequisite patch {} from local message {}",
                patch_id, message_id
            );
            resolved.push(PrerequisitePatch {
                git_patch_id: patch_id.clone(),
                message_id,
                subject,
                author,
                date,
                diff,
            });
            continue;
        }

        if lore_searches == MAX_LORE_SEARCHES {
            bail!(
                "resolving b4 prerequisites requires more than {MAX_LORE_SEARCHES} lore searches"
            );
        }
        lore_searches += 1;
        let lore_patches = fetch(patch_id.clone())
            .await
            .with_context(|| format!("failed to fetch prerequisite patch {patch_id} from lore"))?;
        for patch in lore_patches {
            fetched.entry(patch.git_patch_id.clone()).or_insert(patch);
        }

        let patch = fetched
            .get(patch_id)
            .cloned()
            .ok_or_else(|| anyhow!("lore did not return prerequisite patch ID {patch_id}"))?;
        resolved.push(patch);
    }

    Ok(resolved)
}

pub(crate) async fn resolve_prerequisite_patches_from_lore(
    db: &Database,
    patch_ids: &[String],
) -> Result<Vec<PrerequisitePatch>> {
    let client = LoreMboxClient::new()?;
    resolve_prerequisite_patches(db, patch_ids, move |patch_id| {
        let client = client.clone();
        async move {
            info!("Fetching prerequisite patch {} from lore", patch_id);
            let raw = client.search_patch_id(&patch_id).await?;
            patches_from_mbox(raw).await
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DatabaseSettings;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample_patch(id: &str, message_id: &str) -> PrerequisitePatch {
        PrerequisitePatch {
            git_patch_id: id.to_string(),
            message_id: message_id.to_string(),
            subject: "[PATCH] prerequisite".to_string(),
            author: "Author <author@example.com>".to_string(),
            date: 1_700_000_000,
            diff: "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n".to_string(),
        }
    }

    async fn memory_db() -> Result<Database> {
        let settings = DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await?;
        db.migrate().await?;
        Ok(db)
    }

    async fn insert_local_patch(
        db: &Database,
        message_id: &str,
        diff: &str,
        patch_id: &str,
    ) -> Result<i64> {
        let thread_id = db.create_thread("root", "subject", 1).await?;
        db.create_message(
            message_id,
            thread_id,
            None,
            "Author <author@example.com>",
            "[PATCH] local",
            1,
            "body",
            "",
            "",
            None,
            None,
        )
        .await?;
        let patchset_id = db
            .create_patchset(
                thread_id, None, "root", "subject", "author", 1, 1, 1, "", "", None, 1, None,
                false, None, None,
            )
            .await?
            .ok_or_else(|| anyhow!("test patchset was not created"))?;
        db.create_patch_with_git_patch_id(patchset_id, message_id, 1, diff, Some(patch_id))
            .await?;
        Ok(patchset_id)
    }

    #[tokio::test]
    async fn migration_adds_patch_id_column_and_index_to_existing_database() -> Result<()> {
        let settings = DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await?;
        db.conn
            .execute_batch(
                "CREATE TABLE patches (
                    id INTEGER PRIMARY KEY,
                    patchset_id INTEGER NOT NULL,
                    message_id TEXT NOT NULL UNIQUE,
                    part_index INTEGER,
                    diff TEXT
                );
                PRAGMA user_version = 11;",
            )
            .await?;

        db.migrate().await?;

        let mut columns = db.conn.query("PRAGMA table_info(patches)", ()).await?;
        let mut found_column = false;
        while let Some(row) = columns.next().await? {
            let name: String = row.get(1)?;
            found_column |= name == "git_patch_id";
        }
        assert!(found_column);

        let mut indexes = db
            .conn
            .query(
                "SELECT 1 FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_patches_git_patch_id'",
                (),
            )
            .await?;
        assert!(indexes.next().await?.is_some());

        let mut version = db.conn.query("PRAGMA user_version", ()).await?;
        let version: u32 = version.next().await?.expect("user version row").get(0)?;
        assert_eq!(version, 12);
        Ok(())
    }

    #[test]
    fn parses_ordered_unique_patch_ids() -> Result<()> {
        let first = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let body = format!(
            "prerequisite-patch-id: {first}\r\nprerequisite-patch-id: {second}\r\nprerequisite-patch-id: {first}\r\n> prerequisite-patch-id: cccccccccccccccccccccccccccccccccccccccc\r\n prerequisite-patch-id: dddddddddddddddddddddddddddddddddddddddd\r\nprerequisite-patch-id: short\r\n"
        );

        assert_eq!(
            parse_prerequisite_patch_ids(&body)?,
            vec![first.to_ascii_lowercase(), second.to_string()]
        );
        Ok(())
    }

    #[test]
    fn rejects_too_many_prerequisite_patch_ids() {
        let body = (0..=MAX_PREREQUISITE_PATCH_IDS)
            .map(|index| format!("prerequisite-patch-id: {index:040x}"))
            .collect::<Vec<_>>()
            .join("\n");

        let error = parse_prerequisite_patch_ids(&body)
            .expect_err("metadata above the prerequisite limit should fail");
        assert!(error.to_string().contains("more than 128"));
    }

    #[tokio::test]
    async fn rejects_resolution_above_prerequisite_limit() -> Result<()> {
        let db = memory_db().await?;
        let patch_ids = (0..=MAX_PREREQUISITE_PATCH_IDS)
            .map(|index| format!("{index:040x}"))
            .collect::<Vec<_>>();

        let error = resolve_prerequisite_patches(&db, &patch_ids, |_| async {
            panic!("lore fetch should not run above the prerequisite limit")
        })
        .await
        .expect_err("resolution above the prerequisite limit should fail");

        assert!(error.to_string().contains("cannot resolve more than 128"));
        Ok(())
    }

    #[tokio::test]
    async fn calculates_stable_git_patch_id() -> Result<()> {
        let diff = "diff --git a/file b/file\n\
                    --- a/file\n\
                    +++ b/file\n\
                    @@ -1 +1 @@\n\
                    -old\n\
                    +new\n";
        let id = calculate_git_patch_id(diff).await?;
        assert!(id.is_some());
        assert_eq!(id.unwrap().len(), 40);
        assert_eq!(calculate_git_patch_id("").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn batch_patch_ids_preserve_empty_slots() -> Result<()> {
        let diff = "diff --git a/file b/file\n\
                    --- a/file\n\
                    +++ b/file\n\
                    @@ -1 +1 @@\n\
                    -old\n\
                    +new\n";

        let ids = calculate_git_patch_id_batch(vec![None, Some(diff), None])
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], None);
        assert!(ids[1].is_some());
        assert_eq!(ids[2], None);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drains_patch_id_output_while_writing_input() -> Result<()> {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "head -c 131072 /dev/zero; cat >/dev/null"]);
        let input = vec![b'x'; 131_072];

        let output = run_git_patch_id_command(command, &input, Duration::from_secs(5)).await?;

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 131_072);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_child_failure_after_broken_stdin_pipe() -> Result<()> {
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "exec 0<&-; printf 'specific patch-id failure' >&2; exit 42",
        ]);
        let input = vec![b'x'; 1024 * 1024];

        let output = run_git_patch_id_command(command, &input, Duration::from_secs(5)).await?;

        assert_eq!(output.status.code(), Some(42));
        assert_eq!(output.stderr, b"specific patch-id failure");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn times_out_patch_id_command() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "cat >/dev/null; exec sleep 60"]);

        let error = run_git_patch_id_command(command, b"patch", Duration::from_millis(50))
            .await
            .expect_err("long-running patch-id command should time out");

        assert!(error.to_string().contains("timed out after 50ms"));
    }

    #[tokio::test]
    async fn parses_lore_mbox() -> Result<()> {
        let body = "Patch body\n\n---\n file | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n\ndiff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n";
        let patch_id = calculate_git_patch_id(body)
            .await?
            .expect("test patch should have a stable ID");
        let raw_mbox = format!(
            "From mboxrd@z Thu Jan  1 00:00:00 1970\nFrom: Author <author@example.com>\nDate: Tue, 14 Nov 2023 22:13:20 +0000\nMessage-ID: <patch@example.com>\nSubject: [PATCH] prerequisite\nContent-Type: text/plain; charset=utf-8\n\n{body}"
        );
        let patches = patches_from_mbox(raw_mbox.into_bytes()).await?;

        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].git_patch_id, patch_id);
        assert_eq!(patches[0].message_id, "patch@example.com");
        Ok(())
    }

    #[tokio::test]
    async fn resolves_local_patch_without_fetching_lore() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        insert_local_patch(&db, "local@example.com", "local diff", patch_id).await?;

        let resolved = resolve_prerequisite_patches(&db, &[patch_id.to_string()], |_| async {
            panic!("lore fetch should not run for a local hit")
        })
        .await?;

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].message_id, "local@example.com");
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_ingestion_preserves_existing_patch_id() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "local@example.com";
        let diff = "local diff";
        let patchset_id = insert_local_patch(&db, message_id, diff, patch_id).await?;

        db.create_patch(patchset_id, message_id, 1, diff).await?;

        let stored = db.get_patch_by_git_patch_id(patch_id).await?;
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().0, message_id);
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_ingestion_compares_decompressed_diff() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "compressed@example.com";
        let diff = "x".repeat(2048);
        let patchset_id = insert_local_patch(&db, message_id, &diff, patch_id).await?;

        // Simulate an uncompressed row that the background compressor has not
        // processed yet, then re-ingest it through the compressed write path.
        db.conn
            .execute(
                "UPDATE patches SET diff = ? WHERE message_id = ?",
                libsql::params![diff.clone(), message_id],
            )
            .await?;
        db.create_patch(patchset_id, message_id, 1, &diff).await?;

        let stored = db
            .get_patch_by_git_patch_id(patch_id)
            .await?
            .expect("stable patch ID should be preserved");
        assert_eq!(stored.1, diff);
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_ingestion_rejects_invalid_stored_patch_id_type() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "invalid-id@example.com";
        let diff = "local diff";
        let patchset_id = insert_local_patch(&db, message_id, diff, patch_id).await?;
        db.conn
            .execute(
                "UPDATE patches SET git_patch_id = ? WHERE message_id = ?",
                libsql::params![libsql::Value::Blob(vec![0xff]), message_id],
            )
            .await?;

        assert!(
            db.create_patch(patchset_id, message_id, 1, diff)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn patch_lookup_rejects_invalid_message_metadata_type() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "invalid-subject@example.com";
        insert_local_patch(&db, message_id, "local diff", patch_id).await?;
        db.conn
            .execute(
                "UPDATE messages SET subject = ? WHERE message_id = ?",
                libsql::params![libsql::Value::Blob(vec![0xff]), message_id],
            )
            .await?;

        assert!(db.get_patch_by_git_patch_id(patch_id).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn patch_reingestion_rolls_back_all_updates_on_error() -> Result<()> {
        let db = memory_db().await?;
        let old_patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let new_patch_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let message_id = "rollback@example.com";
        let old_diff = "old diff";
        let patchset_id = insert_local_patch(&db, message_id, old_diff, old_patch_id).await?;
        db.conn
            .execute(
                "UPDATE patchsets SET status = 'Incomplete' WHERE id = ?",
                [patchset_id],
            )
            .await?;
        db.conn
            .execute_batch(
                "CREATE TRIGGER reject_patchset_update
                 BEFORE UPDATE ON patchsets BEGIN
                     SELECT RAISE(ABORT, 'test patchset update failure');
                 END;",
            )
            .await?;

        assert!(
            db.create_patch_with_git_patch_id(
                patchset_id,
                message_id,
                1,
                "new diff",
                Some(new_patch_id),
            )
            .await
            .is_err()
        );
        db.conn
            .execute("DROP TRIGGER reject_patchset_update", ())
            .await?;

        let stored = db
            .get_patch_by_git_patch_id(old_patch_id)
            .await?
            .expect("failed re-ingestion should preserve the old patch");
        assert_eq!(stored.1, old_diff);
        assert!(db.get_patch_by_git_patch_id(new_patch_id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn changed_duplicate_without_patch_id_clears_stale_id() -> Result<()> {
        let db = memory_db().await?;
        let patch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let message_id = "local@example.com";
        let patchset_id = insert_local_patch(&db, message_id, "old diff", patch_id).await?;

        db.create_patch(patchset_id, message_id, 1, "changed diff")
            .await?;

        assert!(db.get_patch_by_git_patch_id(patch_id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn lore_result_is_cached_for_later_patch_ids() -> Result<()> {
        let db = memory_db().await?;
        let first = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = calls.clone();

        let resolved = resolve_prerequisite_patches(
            &db,
            &[first.to_string(), second.to_string()],
            move |_| {
                let fetch_calls = fetch_calls.clone();
                async move {
                    fetch_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![
                        sample_patch(first, "first@example.com"),
                        sample_patch(second, "second@example.com"),
                    ])
                }
            },
        )
        .await?;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolved[0].git_patch_id, first);
        assert_eq!(resolved[1].git_patch_id, second);
        Ok(())
    }

    #[tokio::test]
    async fn bounds_lore_searches_per_resolution() -> Result<()> {
        let db = memory_db().await?;
        let patch_ids = (0..=MAX_LORE_SEARCHES)
            .map(|index| format!("{index:040x}"))
            .collect::<Vec<_>>();
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = calls.clone();

        let error = resolve_prerequisite_patches(&db, &patch_ids, move |patch_id| {
            let fetch_calls = fetch_calls.clone();
            async move {
                fetch_calls.fetch_add(1, Ordering::SeqCst);
                let message_id = format!("{patch_id}@example.com");
                Ok(vec![sample_patch(&patch_id, &message_id)])
            }
        })
        .await
        .expect_err("resolution above the lore search limit should fail");

        assert!(error.to_string().contains("more than 8 lore searches"));
        assert_eq!(calls.load(Ordering::SeqCst), MAX_LORE_SEARCHES);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_lore_result_without_exact_patch_id() -> Result<()> {
        let db = memory_db().await?;
        let wanted = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let error =
            resolve_prerequisite_patches(&db, &[wanted.to_string()], |_| async { Ok(Vec::new()) })
                .await
                .unwrap_err();
        assert!(error.to_string().contains(wanted));
        Ok(())
    }
}
