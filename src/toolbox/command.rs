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

//! Running git subprocesses with a bound on what they can return.
//!
//! A tool asks git a question on behalf of a model, and the model chooses the
//! question. A pattern the model expected to be narrow can match most of the
//! tree, and `Command::output` will hold every byte of that in memory before
//! the tool gets a chance to cut it down to a context budget. Capturing
//! through [`capped_output`] instead stops reading once the answer is already
//! far larger than any budget can use, and stops the command with it.

use anyhow::{Context, Result};
use std::process::{ExitStatus, Stdio};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Bytes of stdout kept from a git subprocess.
///
/// Tool budgets are tens of kilobytes, so this leaves two orders of magnitude
/// of headroom for a legitimately large answer while keeping a pathological
/// one from growing without limit.
pub const MAX_CAPTURED_STDOUT: usize = 8 * 1024 * 1024;

/// Bytes of stderr kept from a git subprocess. Only ever used for messages.
const MAX_CAPTURED_STDERR: usize = 64 * 1024;

/// What a git subprocess produced, with stdout bounded.
pub struct CappedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Set when the command produced more than [`MAX_CAPTURED_STDOUT`] and was
    /// stopped. The status then reflects the signal used to stop it, not the
    /// result of the command.
    pub stdout_capped: bool,
}

impl CappedOutput {
    /// Whether the command answered the question, either by succeeding or by
    /// producing all the output a caller can use.
    pub fn is_usable(&self) -> bool {
        self.status.success() || self.stdout_capped
    }
}

/// Runs a command and captures its output, giving up on stdout beyond
/// [`MAX_CAPTURED_STDOUT`] bytes.
///
/// A capped capture is cut back to the last complete line so callers never
/// have to reason about a partial one.
pub async fn capped_output(cmd: &mut Command) -> Result<CappedOutput> {
    capped_output_with_limit(cmd, MAX_CAPTURED_STDOUT).await
}

/// Runs a command and captures its output, giving up on stdout beyond
/// `max_stdout` bytes and terminating the child process early when the limit is
/// exceeded.
pub async fn capped_output_with_limit(
    cmd: &mut Command,
    max_stdout: usize,
) -> Result<CappedOutput> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git")?;

    let out_pipe = child.stdout.take().context("git stdout was not captured")?;
    let err_pipe = child.stderr.take().context("git stderr was not captured")?;

    // stderr has to be drained for the whole life of the child even though
    // only the first part of it is worth keeping, because a child blocked
    // writing to a pipe nobody reads never exits.
    let stderr_drain = tokio::spawn(drain_capped(err_pipe, MAX_CAPTURED_STDERR));

    // Read one byte past the limit so a capture that lands exactly on it is
    // not mistaken for a truncated one.
    let mut stdout = Vec::new();
    let mut out_reader = out_pipe.take(max_stdout as u64 + 1);
    out_reader
        .read_to_end(&mut stdout)
        .await
        .context("failed to read git stdout")?;

    let stdout_capped = stdout.len() > max_stdout;
    if stdout_capped {
        // Everything after the last newline is half a line of no use to anyone.
        let end = stdout[..max_stdout]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(max_stdout, |i| i + 1);
        stdout.truncate(end);

        // The command has nothing left to tell us, and it would otherwise sit
        // blocked on a pipe nobody is reading. This is also what lets the
        // stderr drain finish.
        let _ = child.start_kill();
    }

    let status = child.wait().await.context("failed to wait for git")?;
    let stderr = stderr_drain
        .await
        .context("stderr drain panicked")?
        .context("failed to read git stderr")?;

    Ok(CappedOutput {
        status,
        stdout,
        stderr,
        stdout_capped,
    })
}

/// Reads a pipe to its end, keeping only the first `keep` bytes.
///
/// The remainder is read and dropped rather than left unread, so the writer is
/// never blocked on a full pipe.
async fn drain_capped<R>(mut reader: R, keep: usize) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(kept);
        }
        if kept.len() < keep {
            let room = keep - kept.len();
            kept.extend_from_slice(&chunk[..n.min(room)]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Every case here must finish promptly. A capture bug shows up as a hang,
    /// and a hang is far more expensive to diagnose than a failure.
    async fn run(cmd: &mut Command) -> CappedOutput {
        tokio::time::timeout(Duration::from_secs(30), capped_output(cmd))
            .await
            .expect("capture should not block")
            .expect("capture should succeed")
    }

    #[tokio::test]
    async fn keeps_small_output_whole() {
        let mut cmd = Command::new("printf");
        cmd.arg("one\ntwo\n");

        let out = run(&mut cmd).await;

        assert!(out.status.success());
        assert!(!out.stdout_capped);
        assert!(out.is_usable());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "one\ntwo\n");
    }

    #[tokio::test]
    async fn caps_output_that_never_ends() {
        // yes(1) produces output until it is stopped, which is the bounded
        // stand-in for a grep that matches most of a tree.
        let mut cmd = Command::new("yes");
        cmd.arg("matching line of output");

        let out = run(&mut cmd).await;

        assert!(out.stdout_capped);
        assert!(out.is_usable());
        assert!(out.stdout.len() <= MAX_CAPTURED_STDOUT);
        assert!(out.stdout.ends_with(b"\n"), "should end on a line boundary");
    }

    #[tokio::test]
    async fn survives_a_child_that_floods_stderr() {
        // More stderr than is worth keeping. It still has to be read, or the
        // child blocks on a full pipe and never exits.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("head -c 200000 /dev/zero | tr '\\0' 'e' >&2; echo done");

        let out = run(&mut cmd).await;

        assert!(out.status.success());
        assert!(!out.stdout_capped);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "done\n");
        assert_eq!(out.stderr.len(), MAX_CAPTURED_STDERR);
    }

    #[tokio::test]
    async fn reports_failure_of_a_small_command() {
        let mut cmd = Command::new("false");

        let out = run(&mut cmd).await;

        assert!(!out.status.success());
        assert!(!out.stdout_capped);
        assert!(!out.is_usable());
    }
}
