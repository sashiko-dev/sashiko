use crate::ai::AiProvider;
use crate::db::Database;
use crate::toolbox::ToolBox;
use crate::workflows::linux_bug::BugInput;
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
        if let Err(e) = self.db.recover_stale_processing_bugs().await {
            error!("Failed to recover stale processing bugs on startup: {}", e);
        }
        loop {
            match self.db.lock_raw_bug().await {
                Ok(Some(bug)) => {
                    let provider = self.provider.clone();
                    let db = self.db.clone();
                    let repo_path = self.repo_path.clone();

                    tokio::spawn(async move {
                        info!("Processing raw bug ID {} ({})", bug.id, bug.bugid);

                        let input = BugInput {
                            problem: bug.problem.clone(),
                            reasoning: bug
                                .severity_explanation
                                .clone()
                                .unwrap_or_else(|| "No reasoning provided.".to_string()),
                            locations: bug.locations.clone(),
                            subsystems: bug.subsystems.clone(),
                            source_files: bug.source_files.clone().unwrap_or_default(),
                            commit_sha: bug.discovered_in_commit.clone(),
                            patchset_id: bug.discovered_in_patchset_id,
                            patch_id: bug.discovered_in_patch_id,
                            baseline_sha: bug.discovered_in_commit.clone(),
                        };

                        let mut tb = ToolBox::new(std::path::PathBuf::from(&repo_path), None);
                        if let Some(ref sha) = bug.discovered_in_commit {
                            tb.set_virtual_head(sha.clone());
                        }
                        let tools = Some(Arc::new(tb));

                        match crate::workflows::linux_bug::process_issue_worker(
                            provider.as_ref(),
                            tools,
                            &db,
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
                                let error_msg = format!("Error during async processing: {}", e);
                                let _ = db
                                    .update_bug_outcome(
                                        bug.id,
                                        crate::db::UpdateBugOutcomeParams {
                                            status: "failed",
                                            severity_explanation: Some(&error_msg),
                                            ..Default::default()
                                        },
                                    )
                                    .await;
                            }
                        }
                    });
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
