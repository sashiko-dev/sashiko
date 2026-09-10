use super::*;
use crate::ai::{AiRequest, AiResponse, ProviderCapabilities};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordingProvider {
    requests: Mutex<Vec<AiRequest>>,
}

#[async_trait::async_trait]
impl AiProvider for RecordingProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        self.requests.lock().unwrap().push(request);
        Ok(AiResponse {
            content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        })
    }

    fn estimate_tokens(&self, _request: &AiRequest) -> usize {
        0
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: "mock".to_string(),
            context_window_size: 100_000,
        }
    }
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[tokio::test]
async fn review_requests_use_target_prefetch_or_explicit_failure() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    std::fs::write(repo.join("file.c"), "int target(void) { return 123; }\n").unwrap();
    git(repo, &["add", "file.c"]);
    git(
        repo,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "target"],
    );
    let sha = git(repo, &["rev-parse", "HEAD"]);
    let diff = git(repo, &["show", "--format=", &sha]);
    let prompts = repo.join("prompts");
    std::fs::create_dir(&prompts).unwrap();

    // Execute three real worker workflows against clean, dirty, and absent
    // working files, then verify the failure path with an unavailable commit.
    for scenario in ["clean", "dirty", "absent", "missing_commit"] {
        if scenario == "dirty" {
            std::fs::write(
                repo.join("file.c"),
                "int misleading(void) { return 999; }\n",
            )
            .unwrap();
        } else if scenario == "absent" {
            std::fs::remove_file(repo.join("file.c")).unwrap();
        }
        let target = if scenario == "missing_commit" {
            "0".repeat(40)
        } else {
            sha.clone()
        };
        let provider = Arc::new(RecordingProvider::default());
        let mut tools = ToolBox::new(repo.to_path_buf(), None);
        tools.set_virtual_head(target.clone());
        let mut worker = Worker::new(
            provider.clone(),
            Arc::new(tools),
            PromptRegistry::new(prompts.clone()),
            WorkerConfig {
                max_input_tokens: 100_000,
                max_interactions: 3,
                temperature: 0.0,
                series_range: None,
                baseline_sha: None,
                custom_prompt: None,
                stages: Some(vec!["goal".to_string()]),
            },
        );
        worker
            .run(
                json!({
                    "id": 1,
                    "patch_index": 1,
                    "patches": [{"index": 1, "commit_id": target, "diff": diff}]
                }),
                None,
            )
            .await
            .unwrap();

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let system = requests[0].system.as_deref().unwrap();
        if scenario == "missing_commit" {
            assert!(system.contains("Automatic source prefetch failed"));
            assert!(system.contains("use git_read_files and git_grep at that revision"));
            assert!(!system.contains("<pre_fetched_context>"));
        } else {
            let prefetch = system
                .split("<pre_fetched_context>")
                .nth(1)
                .unwrap()
                .split("</pre_fetched_context>")
                .next()
                .unwrap();
            assert!(prefetch.contains(&format!("Source revision: {sha}")));
            assert!(prefetch.contains("int target(void) { return 123; }"));
            assert!(!prefetch.contains("misleading"));
            assert!(!system.contains("Automatic source prefetch failed"));
        }
    }
}
