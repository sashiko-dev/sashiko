//! Live capture test: runs a real cherry-pick review against Gemini
//! and saves the request/response pairs as a fixture.
//!
//! Run with:
//!   GEMINI_API_KEY=... cargo test --test live_capture capture -- --ignored --nocapture

use sashiko::ai::{self, AiProvider};
use sashiko::test_support::replay::*;
use sashiko::toolbox::ToolBox;
use sashiko::worker::{PromptRegistry, Worker, WorkerConfig};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// Capture a live review of the cherry-pick fixture against Gemini.
#[tokio::test]
#[ignore]
async fn capture_cherry_pick_replay() {
    // Check for API key
    let _api_key = std::env::var("GEMINI_API_KEY").expect("Set GEMINI_API_KEY to run this test");

    let tmp = tempfile::tempdir().unwrap();
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replays/sample_cache");
    let info = setup_synthetic_git_repo(&fixture_dir.join("repo"), tmp.path())
        .expect("setup synthetic git repo");

    // Create real Gemini provider
    // Load settings from Settings.toml (uses the real defaults)
    let settings_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Settings.toml");
    let settings = sashiko::settings::Settings::from_file(&settings_path)
        .expect("failed to load Settings.toml");

    let inner = ai::create_provider(&settings).expect("failed to create AI provider");

    // Wrap with recording
    let recording = Arc::new(RecordingProvider::new(inner));

    let prompts_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let prompts = PromptRegistry::new(prompts_dir);
    let tools = Arc::new(ToolBox::new(info.repo_path, None));

    let config = WorkerConfig {
        max_input_tokens: settings.ai.max_input_tokens,
        max_interactions: settings.ai.max_interactions,
        temperature: settings.ai.temperature,
        custom_prompt: None,
        series_range: None,
        baseline_sha: None,
        stages: None,
    };

    // Build patchset with conflict context
    let patchset = json!({
        "id": 99999,
        "subject": "fs: sample_cache: implement buffer cache pool and purge",
        "conflict": {
            "resolution_sha": info.resolution_sha,
            "original_sha": info.original_sha,
            "base_sha": info.base_sha
        },
        "patches": [{
            "diff": info.resolution_diff
        }]
    });

    let mut worker = Worker::new(
        recording.clone() as Arc<dyn AiProvider>,
        tools,
        prompts,
        config,
    );

    eprintln!("Starting live review...");
    let result = worker
        .run(
            patchset,
            Some(&|evt| {
                eprintln!("  Progress: {:?}", evt);
            }),
        )
        .await;

    match &result {
        Ok(r) => {
            eprintln!("Review completed successfully");
            // Save golden output
            let output = r.output.as_ref().unwrap();
            let golden_path = fixture_dir.join("golden.json");
            std::fs::write(&golden_path, serde_json::to_string_pretty(output).unwrap()).unwrap();
            eprintln!("Saved golden output to {:?}", golden_path);
        }
        Err(e) => {
            eprintln!("Review failed: {:?}", e);
        }
    }

    // Save recording regardless
    let recording_path = fixture_dir.join("replay.json");
    recording.save_to_file(recording_path.to_str().unwrap());
    eprintln!(
        "Saved {} recorded exchanges to {:?}",
        recording.recordings.lock().unwrap().len(),
        recording_path
    );

    result.expect("review should succeed");
}
