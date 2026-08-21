//! Replay test runner: discovers and runs all replay fixtures in tests/fixtures/replays/

use sashiko::ai::AiProvider;
use sashiko::pipelines::cherry_pick_review::CherryPickReviewPipeline;
use sashiko::pipelines::{PipelineEnv, execute_pipeline};
use sashiko::review_kind::ReviewKind;
use sashiko::test_support::replay::*;
use sashiko::toolbox::ToolBox;
use sashiko::worker::PromptRegistry;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, serde::Deserialize)]
struct FixtureConfig {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: String,
    repo: RepoConfig,
    patchset: PatchsetConfig,
    #[serde(default)]
    options: OptionConfig,
}

#[derive(Debug, serde::Deserialize)]
struct RepoConfig {
    #[serde(rename = "type")]
    repo_type: String,
    #[serde(default = "default_source_dir")]
    source_dir: String,
}

fn default_source_dir() -> String {
    "repo".to_string()
}

#[derive(Debug, serde::Deserialize)]
struct PatchsetConfig {
    #[serde(default = "default_patchset_id")]
    id: i64,
    subject: String,
    #[serde(default)]
    conflict: Option<ConflictConfig>,
}

fn default_patchset_id() -> i64 {
    99999
}

#[derive(Debug, serde::Deserialize)]
struct ConflictConfig {
    original_sha: Option<String>,
    base_sha: Option<String>,
    resolution_sha: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct OptionConfig {
    #[serde(default = "default_max_interactions")]
    max_interactions: usize,
    #[serde(default)]
    temperature: f32,
}

fn default_max_interactions() -> usize {
    50
}

async fn run_replay_test_case(fixture_dir: &Path) {
    let config_path = fixture_dir.join("config.json");
    assert!(
        config_path.exists(),
        "config.json missing in {:?}",
        fixture_dir
    );
    let config_raw = std::fs::read_to_string(&config_path).expect("read config.json");
    let config: FixtureConfig = serde_json::from_str(&config_raw).expect("valid config.json");

    let replay_path = fixture_dir.join("replay.json");
    let responses = load_fixture(replay_path.to_str().unwrap());

    let golden_path = fixture_dir.join("golden.json");
    let golden_logs_path = fixture_dir.join("logs_golden.txt");

    let tmp = tempfile::tempdir().unwrap();
    let prompts_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let prompts = PromptRegistry::new(prompts_dir);

    let (repo_path, original_sha, base_sha, resolution_sha, resolution_diff) =
        match config.repo.repo_type.as_str() {
            "synthetic" => {
                let source_dir = fixture_dir.join(&config.repo.source_dir);
                let info = setup_synthetic_git_repo(&source_dir, tmp.path())
                    .expect("synthetic repo setup should succeed");
                (
                    info.repo_path,
                    info.original_sha,
                    Some(info.base_sha),
                    info.resolution_sha,
                    info.resolution_diff,
                )
            }
            "configured" => {
                let settings_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Settings.toml");
                let settings = sashiko::settings::Settings::from_file(&settings_path)
                    .expect("failed to load Settings.toml");
                let p = match std::env::var("LINUX_REPO") {
                    Ok(r) => PathBuf::from(r),
                    Err(_) => {
                        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(settings.git.repository_path)
                    }
                };
                if !p.exists() {
                    eprintln!(
                        "Skipping configured repo test {}: {:?} does not exist",
                        config.name, p
                    );
                    return;
                }
                let c = config
                    .patchset
                    .conflict
                    .as_ref()
                    .expect("conflict config required for configured repo");
                (
                    p,
                    c.original_sha.clone().expect("original_sha"),
                    c.base_sha.clone(),
                    c.resolution_sha.clone().expect("resolution_sha"),
                    String::new(),
                )
            }
            other => panic!("unknown repo type: {}", other),
        };

    let tools = Arc::new(ToolBox::new(repo_path, None));
    let provider = Arc::new(ReplayProvider::new(responses));

    let pipeline = CherryPickReviewPipeline::from_review_kind(
        &ReviewKind::CherryPick {
            original_sha: original_sha.clone(),
            base_sha: base_sha.clone(),
        },
        resolution_sha.clone(),
    );

    let env = PipelineEnv {
        provider: provider.clone() as Arc<dyn AiProvider>,
        tools,
        prompts: &prompts,
        temperature: config.options.temperature,
        max_interactions: config.options.max_interactions,
        context_tag: None,
        stages: None,
        series_range: None,
    };

    let patchset = json!({
        "id": config.patchset.id,
        "subject": config.patchset.subject,
        "conflict": {
            "resolution_sha": resolution_sha,
            "original_sha": original_sha,
            "base_sha": base_sha
        },
        "patches": [{
            "diff": resolution_diff
        }]
    });

    let result = execute_pipeline(&pipeline, &env, patchset, None)
        .await
        .expect("replay should succeed");

    fn normalize_date(s: &str) -> String {
        let marker = "the current date is ";
        if let Some(i) = s.find(marker) {
            let after = i + marker.len();
            if let Some(rel) = s[after..].find('.') {
                let j = after + rel;
                return format!("{}<DATE>{}", &s[..after], &s[j..]);
            }
        }
        s.to_string()
    }

    let mut hist = serde_json::to_value(&result.history).unwrap();
    sashiko::ai::scrub_thought_signatures(&mut hist);
    let logs = serde_json::to_string_pretty(&hist).unwrap();
    let logs_norm = normalize_date(&logs);
    let golden_logs = std::fs::read_to_string(&golden_logs_path).expect("read golden logs");
    assert_eq!(
        logs_norm, golden_logs,
        "review logs diverged in fixture {:?}",
        config.name
    );

    let output = result.output.expect("should have output");
    let golden: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).expect("read golden json"))
            .expect("valid golden JSON");

    assert_eq!(output["concerns_count"], golden["concerns_count"]);
    assert_eq!(
        output["dismissed_concerns_count"],
        golden["dismissed_concerns_count"]
    );
    assert_eq!(output["review_inline"], golden["review_inline"]);

    let output_findings = output["findings"].as_array().expect("findings array");
    let golden_findings = golden["findings"]
        .as_array()
        .expect("golden findings array");
    assert_eq!(output_findings.len(), golden_findings.len());
}

#[tokio::test]
async fn replay_all_fixtures() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replays");
    assert!(base.exists(), "fixtures/replays directory missing");
    let mut entries: Vec<_> = std::fs::read_dir(&base)
        .expect("read fixtures/replays")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        eprintln!("Running replay test for fixture: {:?}", entry.file_name());
        run_replay_test_case(&entry.path()).await;
    }
}
