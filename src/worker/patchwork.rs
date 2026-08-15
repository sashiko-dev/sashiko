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
use crate::email_policy::{EmailPolicyConfig, PatchworkPolicy};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};

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

    /// Resolve the Patchwork API policy for a given api_url.
    ///
    /// Credentials and request identification are deliberately resolved
    /// from the config at delivery time instead of being stored in the
    /// outbox.
    fn resolve_api_policy(&self, api_url: &str) -> Option<PatchworkPolicy> {
        let config = EmailPolicyConfig::load(&self.email_policy_path)
            .expect("Failed to load email policy for Patchwork delivery");

        // Check subsystem policies for a matching api_url
        for sub in config.subsystems.values() {
            if sub.patchwork.enabled && sub.patchwork.api_url.as_deref() == Some(api_url) {
                return Some(sub.patchwork.clone());
            }
        }

        // Fall back to defaults
        if config.defaults.patchwork.enabled
            && config.defaults.patchwork.api_url.as_deref() == Some(api_url)
        {
            return Some(config.defaults.patchwork);
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

                    let api_policy = self.resolve_api_policy(&entry.api_url);
                    let token = api_policy
                        .as_ref()
                        .and_then(|policy| policy.token.as_deref());
                    let user_agent = api_policy
                        .as_ref()
                        .and_then(|policy| policy.user_agent.as_deref());

                    match crate::patchwork::post_patchwork_check(
                        &client,
                        &entry.api_url,
                        crate::patchwork::PatchworkApiIdentity { token, user_agent },
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
