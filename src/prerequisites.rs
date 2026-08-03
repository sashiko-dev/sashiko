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
use anyhow::{Context, Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::info;

const MAX_MBOX_MESSAGES: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrerequisitePatch {
    pub(crate) git_patch_id: String,
    pub(crate) message_id: String,
    pub(crate) subject: String,
    pub(crate) author: String,
    pub(crate) date: i64,
    pub(crate) diff: String,
}

pub(crate) fn parse_prerequisite_patch_ids(body: &str) -> Vec<String> {
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
            ids.push(id);
        }
    }

    ids
}

/// Calculates the stable Git patch ID for one email patch body.
pub async fn calculate_git_patch_id(diff: &str) -> Result<Option<String>> {
    if diff.trim().is_empty() {
        return Ok(None);
    }

    let mut child = Command::new("git")
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start git patch-id")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(diff.as_bytes())
            .await
            .context("failed to write patch to git patch-id")?;
    }

    let output = child
        .wait_with_output()
        .await
        .context("failed to wait for git patch-id")?;
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

    let mut patches = Vec::new();
    let mut seen = HashSet::new();
    for (metadata, patch) in parsed {
        let Some(patch) = patch else {
            continue;
        };
        let Some(git_patch_id) = calculate_git_patch_id(&patch.diff).await? else {
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
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<PrerequisitePatch>>>,
{
    let mut fetched = HashMap::new();
    let mut resolved = Vec::with_capacity(patch_ids.len());

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
                PRAGMA user_version = 1;",
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
        assert_eq!(version, 1);
        Ok(())
    }

    #[test]
    fn parses_ordered_unique_patch_ids() {
        let first = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let body = format!(
            "prerequisite-patch-id: {first}\r\nprerequisite-patch-id: {second}\r\nprerequisite-patch-id: {first}\r\n> prerequisite-patch-id: cccccccccccccccccccccccccccccccccccccccc\r\n prerequisite-patch-id: dddddddddddddddddddddddddddddddddddddddd\r\nprerequisite-patch-id: short\r\n"
        );

        assert_eq!(
            parse_prerequisite_patch_ids(&body),
            vec![first.to_ascii_lowercase(), second.to_string()]
        );
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
