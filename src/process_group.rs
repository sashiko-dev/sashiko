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

//! Shared cancellation and shutdown ownership of child process groups.

use std::sync::Mutex;
use tokio::process::{Child, Command};

/// Spawn a child in its own group. Git hooks/transports and CLI wrappers can
/// launch descendants, which must be terminated along with their parent.
/// The caller must retain a GroupGuard until it has reaped the child.
pub fn spawn(cmd: &mut Command) -> std::io::Result<Child> {
    cmd.process_group(0).kill_on_drop(true).spawn()
}

/// Signal the whole group, having spawned its leader with `process_group(0)`
/// so that the group holds the child and its descendants and nothing else.
pub(crate) fn kill_group(pgid: i32) {
    signal_group(pgid, rustix::process::Signal::KILL);
}

pub(crate) fn terminate_group(pgid: i32) {
    signal_group(pgid, rustix::process::Signal::TERM);
}

fn signal_group(pgid: i32, signal: rustix::process::Signal) {
    // Guard `pgid > 1` so `kill_process_group` (`kill(-pgid, signal)`) can
    // never target the caller's group (`0`), all user processes (`1`), or a
    // negated PID.
    if pgid > 1
        && let Some(pid) = rustix::process::Pid::from_raw(pgid)
    {
        let _ = rustix::process::kill_process_group(pid, signal);
    }
}

/// Groups still running, for a shutdown that never unwinds.
pub(crate) static LIVE_GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
/// Shared Git writers need their signal handlers to remove repository locks.
pub(crate) static SHARED_WRITER_GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Kill every child process group still running.
///
/// For a signal handler that exits the process without dropping the futures
/// holding the children. A terminal signal no longer reaches these groups,
/// and their guards never run on std::process::exit.
/// Shared Git writers receive SIGTERM so their lockfile cleanup can run.
pub fn kill_live_groups() {
    let mut groups = LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner());
    for pgid in groups.drain(..) {
        kill_group(pgid);
    }
    let mut writers = SHARED_WRITER_GROUPS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for pgid in writers.drain(..) {
        terminate_group(pgid);
    }
}

/// Kills the group on drop unless the child was reaped first.
///
/// The group must not be signalled once its leader has been reaped, because
/// the pid is then free for reuse, so `disarm` follows the wait and `kill`
/// precedes it.
pub(crate) struct GroupGuard {
    pgid: Option<i32>,
    registry: &'static Mutex<Vec<i32>>,
}

impl GroupGuard {
    pub(crate) fn new(pgid: Option<i32>) -> Self {
        Self::register(pgid, &LIVE_GROUPS)
    }

    pub(crate) fn shared_writer(pgid: Option<i32>) -> Self {
        Self::register(pgid, &SHARED_WRITER_GROUPS)
    }

    fn register(pgid: Option<i32>, registry: &'static Mutex<Vec<i32>>) -> Self {
        if let Some(pgid) = pgid {
            registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(pgid);
        }
        Self { pgid, registry }
    }

    pub(crate) fn disarm(&mut self) -> Option<i32> {
        let pgid = self.pgid.take()?;
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|g| *g != pgid);
        Some(pgid)
    }

    pub(crate) fn kill(&mut self) {
        if let Some(pgid) = self.disarm() {
            kill_group(pgid);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}
