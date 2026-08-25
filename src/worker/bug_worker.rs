use crate::ai::AiProvider;
use crate::db::Database;
use crate::pipelines::bug::BugInput;
use crate::toolbox::ToolBox;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};

pub struct BugWorker {
    db: Arc<Database>,
    provider: Arc<dyn AiProvider>,
    repo_path: String,
}

impl BugWorker {
    pub fn new(db: Arc<Database>, provider: Arc<dyn AiProvider>, repo_path: String) -> Self {
        Self {
            db,
            provider,
            repo_path,
        }
    }

    pub async fn run(&self) {
        info!("Starting Bug Worker...");
        loop {
            match self.db.lock_raw_bug().await {
                Ok(Some(bug)) => {
                    info!("Processing raw bug ID {} ({})", bug.id, bug.slug);

                    // Reconstruct BugInput exactly like the reviewer uses
                    let input = BugInput {
                        problem: bug.problem.clone(),
                        reasoning: "Resumed from database queue".to_string(),
                        locations: bug.locations.clone(),
                        subsystems: bug.subsystems.clone(),
                        source_files: bug.source_files.clone().unwrap_or_default(),
                        commit_sha: bug.discovered_in_commit.clone(),
                        patchset_id: bug.discovered_in_patchset_id,
                        patch_id: bug.discovered_in_patch_id,
                        baseline_sha: bug.discovered_in_commit.clone(),
                    };

                    // Instantiate a standalone tools box using virtual head
                    // It securely scopes strictly to this bug's discovery state
                    let mut tb = ToolBox::new(std::path::PathBuf::from(&self.repo_path), None);
                    if let Some(ref sha) = bug.discovered_in_commit {
                        tb.set_virtual_head(sha.clone());
                    }
                    let tools = Some(Arc::new(tb));

                    match crate::pipelines::bug::process_issue_worker(
                        self.provider.as_ref(),
                        tools,
                        &self.db,
                        &bug,
                        input,
                        Some("bug_worker"),
                    )
                    .await
                    {
                        Ok(outcome) => {
                            info!("Successfully processed raw bug {}: {:?}", bug.id, outcome);
                        }
                        Err(e) => {
                            error!("Failed to process raw bug {}: {}", bug.id, e);
                            let _ = self
                                .db
                                .update_bug_outcome(
                                    bug.id,
                                    "error",
                                    crate::db::Severity::Low,
                                    None,
                                    "Error during async processing",
                                    None,
                                    None,
                                    None,
                                    false,
                                    None,
                                )
                                .await;
                        }
                    }
                }
                Ok(None) => {
                    sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    error!("Database error while fetching raw bugs: {}", e);
                    sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }
}
