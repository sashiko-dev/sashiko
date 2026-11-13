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

//! Ownership of shared-repository writes across preparation cancellation.

use crate::git_cmd::GitCommandExt;
use anyhow::Result;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::{sync::oneshot, task::JoinHandle};

tokio::task_local! {
    static ACTIVE_WRITES: PendingWrites;
}

#[derive(Clone, Default)]
pub(crate) struct PendingWrites(Arc<Mutex<Vec<JoinHandle<()>>>>);

impl PendingWrites {
    #[cfg(any(feature = "server", test))]
    pub(crate) async fn scope<F: Future>(&self, future: F) -> F::Output {
        ACTIVE_WRITES.scope(self.clone(), future).await
    }

    /// The caller bounds this wait. Dropping a timed-out wait detaches the
    /// writers; their owned locks remain held until the writes actually finish.
    #[cfg(any(feature = "server", test))]
    pub(crate) async fn finish(&self) -> Result<()> {
        let tasks = std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()));
        for task in tasks {
            task.await?;
        }
        Ok(())
    }
}

/// Writes retain their mutex guards inside this task, not in the cancellable
/// caller. Register before the first await so preparation cleanup can join
/// every started write without waiting for unrelated remote activity.
pub(super) async fn finish_write<F, T>(write: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let isolated = crate::git_cmd::processes_are_isolated();
    let (send, receive) = oneshot::channel();
    let task = tokio::spawn(async move {
        let result = if isolated {
            crate::git_cmd::with_shared_writer_processes(write).await
        } else {
            write.await
        };
        let _ = send.send(result);
    });
    // Outside preparation the receiver still waits for completion normally.
    // A cancelled caller detaches the task, preserving its write ownership.
    let _ = ACTIVE_WRITES.try_with(|pending| {
        pending
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(task);
    });
    receive.await?
}

/// Discover the default branch without holding the shared configuration lock
/// over network I/O. Some transports advertise only an object ID for HEAD.
pub(super) async fn discover_head(repo: &Path, name: &str) -> Option<OsString> {
    tokio::time::timeout(std::time::Duration::from_secs(300), async {
        // Most transports name the symbolic HEAD directly. Only fetch the
        // branch advertisement when its object ID needs to be matched.
        for refs in [&["HEAD"][..], &["HEAD", "refs/heads/*"][..]] {
            let mut command = crate::git_cmd::in_dir_async(repo);
            command
                .args(super::GIT_PROTOCOL_RESTRICTIONS)
                .args(["ls-remote", "--symref", "--", name])
                .args(refs);
            let output = crate::git_cmd::output_limited(&mut command, 1024 * 1024)
                .await
                .ok()?;
            if !output.status.success() {
                return None;
            }
            if let Some(head) = advertised_head(&output.stdout) {
                return Some(head.to_owned());
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

const MAX_HEAD_RECORDS: usize = 16_384;

fn advertised_head(refs: &[u8]) -> Option<&OsStr> {
    if refs
        .split(|byte| *byte == b'\n')
        .take(MAX_HEAD_RECORDS + 1)
        .count()
        > MAX_HEAD_RECORDS
    {
        return None;
    }
    let entries = || {
        refs.split(|byte| *byte == b'\n').filter_map(|line| {
            let mut fields = line.splitn(2, |byte| *byte == b'\t');
            Some((fields.next()?, fields.next()?))
        })
    };
    if let Some(branch) = entries().find_map(|(value, name)| {
        (name == b"HEAD")
            .then(|| value.strip_prefix(b"ref: refs/heads/"))
            .flatten()
    }) {
        return Some(OsStr::from_bytes(branch));
    }
    let (head, _) = entries().find(|(_, name)| *name == b"HEAD")?;
    let mut matches = entries().filter_map(|(value, name)| {
        (value == head)
            .then(|| name.strip_prefix(b"refs/heads/"))
            .flatten()
    });
    let branch = matches.next()?;
    // Ambiguous advertisements fall back to the existing common-name order.
    if matches.next().is_some() {
        return None;
    }
    Some(OsStr::from_bytes(branch))
}

async fn ref_exists(repo: &Path, reference: impl AsRef<OsStr>) -> bool {
    crate::git_cmd::in_dir_async(repo)
        .args(["show-ref", "--verify", "-q"])
        .arg(reference)
        .output_cancellable()
        .await
        .is_ok_and(|output| output.status.success())
}

/// The caller owns both configuration and remote locks in a shielded task.
/// Explicit set-head can also write config when followRemoteHEAD is "always".
pub(super) async fn update_refs(repo: &Path, name: &str, head: Option<&OsStr>) -> Result<()> {
    let mut resolved = false;
    for branch in head
        .into_iter()
        .chain(["master", "main", "for-next", "for-linus"].map(OsStr::new))
    {
        let mut reference = OsString::from(format!("refs/remotes/{name}/"));
        reference.push(branch);
        if !ref_exists(repo, reference).await {
            continue;
        }
        let result = crate::git_cmd::in_dir_async(repo)
            .args(["remote", "set-head", "--", name])
            .arg(branch)
            .output_cancellable()
            .await?;
        if result.status.success() {
            resolved = true;
            break;
        }
    }
    if !resolved {
        tracing::debug!("Could not set default HEAD for remote {}", name);
    }
    if name == "origin" {
        for branch in ["master", "main"] {
            let remote = format!("refs/remotes/origin/{branch}");
            let local = format!("refs/heads/{branch}");
            if ref_exists(repo, &remote).await && ref_exists(repo, &local).await {
                let _ = crate::git_cmd::in_dir_async(repo)
                    .args(["update-ref", &local, &remote])
                    .output_cancellable()
                    .await;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn remote_head_advertisements() {
        assert_eq!(
            advertised_head(b"ref: refs/heads/topic\tHEAD\nabc\tHEAD\nabc\trefs/heads/topic\n"),
            Some(OsStr::new("topic"))
        );
        assert_eq!(
            advertised_head(b"abc\tHEAD\nabc\trefs/heads/topic\n"),
            Some(OsStr::new("topic"))
        );
        assert_eq!(
            advertised_head(b"abc\tHEAD\nabc\trefs/heads/one\nabc\trefs/heads/two\n"),
            None
        );
        assert_eq!(advertised_head(b"abc\trefs/heads/main\n"), None);
        assert_eq!(advertised_head(b""), None);
        assert_eq!(
            advertised_head(b"ref: refs/heads/topic\tHEAD\nabc\trefs/heads/\xff\n"),
            Some(OsStr::new("topic"))
        );
        assert_eq!(
            advertised_head(b"abc\tHEAD\nabc\trefs/heads/topic\ndef\trefs/heads/\xff\n"),
            Some(OsStr::new("topic"))
        );
        assert_eq!(
            advertised_head(&b"abc\trefs/heads/topic\n".repeat(MAX_HEAD_RECORDS + 1)),
            None
        );
    }

    #[tokio::test]
    async fn cancelled_caller_retains_only_its_own_writes_and_locks() -> Result<()> {
        let pending = PendingWrites::default();
        let unrelated = PendingWrites::default();
        let mutex = Arc::new(tokio::sync::Mutex::new(()));
        let guard = mutex.clone().lock_owned().await;
        let (started, ready) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let caller_pending = pending.clone();
        let caller = tokio::spawn(async move {
            caller_pending
                .scope(crate::git_cmd::with_isolated_processes(finish_write(
                    async move {
                        let _guard = guard;
                        assert!(crate::git_cmd::processes_are_isolated());
                        started.send(()).unwrap();
                        released.await?;
                        Ok(())
                    },
                )))
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), ready).await??;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(mutex.try_lock().is_err());
        tokio::time::timeout(Duration::from_secs(1), unrelated.finish()).await??;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), pending.finish())
                .await
                .is_err()
        );
        // A cleanup timeout detaches the writer, but must not release its lock.
        assert!(mutex.try_lock().is_err());
        release.send(()).unwrap();
        let _guard = tokio::time::timeout(Duration::from_secs(10), mutex.lock()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_joins_cancelled_writers() -> Result<()> {
        let pending = PendingWrites::default();
        let (started, ready) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let caller_pending = pending.clone();
        let caller = tokio::spawn(async move {
            caller_pending
                .scope(finish_write(async move {
                    started.send(()).unwrap();
                    released.await?;
                    Ok(())
                }))
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), ready).await??;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let cleanup = pending.finish();
        tokio::pin!(cleanup);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut cleanup)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), cleanup).await??;
        Ok(())
    }
}
