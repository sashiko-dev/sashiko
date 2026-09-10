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

use crate::git_ops::repack_repository;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};

/// How long the worker waits between passes.  What accumulates is
/// loose objects, and they arrive with every sync cycle rather than
/// in proportion to what any one fetch carries, so the interval is
/// what bounds them.  Six hours holds the backlog to a handful of
/// cycles and leaves the rest of the day without a pass over the
/// object store.
const REPACK_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// How long the worker waits before its first pass.  Startup already
/// has a commit-graph walk over this object store, and the two cost
/// more together than apart.  Waiting a whole interval instead would
/// leave a service that restarts more often than that never packing
/// at all, and nothing else packs it now that auto-maintenance is
/// off.
const FIRST_PASS_DELAY: Duration = Duration::from_secs(15 * 60);

pub struct RepackWorker {
    repo_path: PathBuf,
}

impl RepackWorker {
    pub fn new(repo_path: PathBuf) -> Self {
        Self { repo_path }
    }

    pub async fn run(&self) {
        info!(
            "RepackWorker started. Will repack every {} hours.",
            REPACK_INTERVAL.as_secs() / 3600
        );

        let mut delay = FIRST_PASS_DELAY;

        loop {
            sleep(delay).await;
            delay = REPACK_INTERVAL;

            if let Err(e) = repack_repository(&self.repo_path).await {
                error!("RepackWorker failed to repack: {}", e);
            }
        }
    }
}
