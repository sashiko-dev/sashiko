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
use crate::email_policy::EmailPolicyConfig;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

pub struct PatchworkWorker {
    db: Arc<Database>,
    email_policy_path: String,
    max_retries: u32,
}

impl PatchworkWorker {
    pub fn new(db: Arc<Database>, email_policy_path: String, max_retries: u32) -> Self {
        Self {
            db,
            email_policy_path,
            max_retries,
        }
    }

    /// Resolve the patchwork API token for a given api_url by loading
    /// the email policy config and matching against subsystem policies.
    /// Tokens are never stored in the database -- they are resolved
    /// from the config file (and SASHIKO_PATCHWORK_TOKEN env var) at
    /// delivery time.
    fn resolve_token(&self, api_url: &str) -> Option<String> {
        let config = match EmailPolicyConfig::load(&self.email_policy_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to load email policy for token resolution: {}", e);
                return None;
            }
        };

        // Check subsystem policies for a matching api_url
        for sub in config.subsystems.values() {
            if sub.patchwork.enabled && sub.patchwork.api_url.as_deref() == Some(api_url) {
                return sub.patchwork.token.clone();
            }
        }

        // Fall back to defaults
        if config.defaults.patchwork.enabled
            && config.defaults.patchwork.api_url.as_deref() == Some(api_url)
        {
            return config.defaults.patchwork.token.clone();
        }

        None
    }

    /// Compute the backoff delay in seconds based on retry count.
    fn backoff_seconds(retry_count: i64) -> i64 {
        match retry_count {
            0 => 5,
            1 => 30,
            _ => 180,
        }
    }

    pub async fn run(&self) {
        info!("Starting Patchwork Worker...");
        let client = reqwest::Client::new();
        loop {
            if let Err(e) = self.db.sweep_ghost_patchwork().await {
                error!("Failed to sweep ghost patchwork entries: {}", e);
            }

            match self.db.lock_pending_patchwork().await {
                Ok(Some(entry)) => {
                    info!(
                        "Processing patchwork check ID {} for msgid {}",
                        entry.id, entry.patch_msg_id
                    );

                    // Resolve the token from config at delivery time,
                    // not from the database row.
                    let token = self.resolve_token(&entry.api_url);

                    match crate::patchwork::post_patchwork_check(
                        &client,
                        &entry.api_url,
                        token.as_deref(),
                        &entry.patch_msg_id,
                        &entry.check_state,
                        &entry.description,
                        &entry.target_url,
                    )
                    .await
                    {
                        Ok(()) => {
                            info!("Successfully posted patchwork check ID {}", entry.id);
                            if let Err(e) = self.db.mark_patchwork_sent(entry.id).await {
                                error!("Failed to mark patchwork {} as sent: {}", entry.id, e);
                            }
                        }
                        Err(e) => {
                            error!("Patchwork check failed for ID {}: {}", entry.id, e);
                            if entry.retry_count + 1 >= self.max_retries as i64 {
                                if let Err(db_err) =
                                    self.db.mark_patchwork_failed(entry.id, &e).await
                                {
                                    error!(
                                        "Failed to mark patchwork {} as failed: {}",
                                        entry.id, db_err
                                    );
                                }
                            } else {
                                // Schedule retry with a future timestamp
                                // instead of blocking the worker loop.
                                let delay = Self::backoff_seconds(entry.retry_count);
                                let retry_at = chrono::Utc::now().timestamp() + delay;
                                if let Err(db_err) =
                                    self.db.set_patchwork_retry_at(entry.id, retry_at).await
                                {
                                    error!("Failed to schedule retry for {}: {}", entry.id, db_err);
                                }
                            }
                        }
                    }
                }
                Ok(None) => {
                    sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    error!("Database error while locking patchwork entry: {}", e);
                    sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }
}
