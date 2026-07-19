use anyhow::{Result, bail, ensure};
use async_trait::async_trait;
use sashiko::ai::{AiProvider, AiRequest, AiResponse, AiRole, ProviderCapabilities, ToolCall};
use sashiko::toolbox::ToolBox;
use sashiko::worker::{PromptRegistry, Worker, WorkerConfig};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const CORE_PATH: &str = "drivers/media/v4l2-core/v4l2-subdev.c";
const DRIVER_PATH: &str = "drivers/media/platform/test/formatter.c";
const CORE_MARKER: &str = "CORE_WRAPPER_PROOF_MARKER";

#[derive(Debug, Deserialize)]
struct Expectations {
    cases: Vec<ExpectedCase>,
}

#[derive(Debug, Deserialize)]
struct ExpectedCase {
    function: String,
    expected_finding: bool,
    evidence: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct PipelineEvidence {
    phase0_saw_media_index_and_patch: bool,
    stage3_received_media_guide: bool,
    stage3_initial_context_excluded_core_file: bool,
    stage3_tool_retrieved_core_file: bool,
    stage10_received_media_guide: bool,
}

struct PipelineProvider {
    evidence: Mutex<PipelineEvidence>,
}

impl PipelineProvider {
    fn new() -> Self {
        Self {
            evidence: Mutex::new(PipelineEvidence::default()),
        }
    }

    fn evidence(&self) -> PipelineEvidence {
        self.evidence.lock().unwrap().clone()
    }
}

#[async_trait]
impl AiProvider for PipelineProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let tag = request.context_tag.as_deref().unwrap_or_default();

        if tag.contains("s:0]") {
            let prompt = request_text(&request);
            ensure!(
                prompt.contains("| Media/V4L2 |")
                    && prompt.contains("formatter_subdev_enum_mbus_code"),
                "Phase 0 did not receive both the media index entry and driver patch"
            );
            ensure!(request.temperature == Some(0.0));
            self.evidence
                .lock()
                .unwrap()
                .phase0_saw_media_index_and_patch = true;
            return Ok(json_response(json!({"selected_prompts": ["media.md"]})));
        }

        if tag.contains("s:3]") {
            let system = request.system.as_deref().unwrap_or_default();
            ensure!(
                system.contains("# Media/V4L2 Subsystem Details"),
                "Phase 0 selection did not add media.md to Stage 3"
            );
            ensure!(request.temperature == Some(0.0));

            let tool_result = request.messages.iter().find_map(|message| {
                (message.role == AiRole::Tool)
                    .then_some(message.content.as_deref())
                    .flatten()
            });

            if let Some(tool_result) = tool_result {
                ensure!(
                    tool_result.contains(CORE_MARKER)
                        && tool_result.contains("call_enum_mbus_code")
                        && tool_result.contains("check_state")
                        && tool_result.contains("call_set_fmt")
                        && tool_result.contains("check_format"),
                    "Stage 3 tool result did not contain the media-core proof"
                );
                self.evidence
                    .lock()
                    .unwrap()
                    .stage3_tool_retrieved_core_file = true;
                return Ok(json_response(stage3_output()));
            }

            ensure!(
                !system.contains(CORE_MARKER),
                "unmodified media-core file was unexpectedly present before tool retrieval"
            );
            ensure!(
                request.tools.as_ref().is_some_and(|tools| {
                    tools.iter().any(|tool| tool.name == "git_read_files")
                }),
                "Stage 3 did not expose git_read_files"
            );
            let mut evidence = self.evidence.lock().unwrap();
            evidence.stage3_received_media_guide = true;
            evidence.stage3_initial_context_excluded_core_file = true;
            return Ok(tool_response(ToolCall {
                id: "read-media-core".to_string(),
                function_name: "git_read_files".to_string(),
                arguments: json!({
                    "revision": "HEAD",
                    "files": [{"path": CORE_PATH}]
                }),
                thought_signature: None,
            }));
        }

        if tag.contains("s:8]") {
            return Ok(json_response(json!({
                "concerns": unsafe_concerns(),
                "dismissed_concerns": guarded_dismissal()
            })));
        }

        if tag.contains("s:9]") {
            return Ok(json_response(json!({"concerns": unsafe_concerns()})));
        }

        if tag.contains("s:10]") {
            let system = request.system.as_deref().unwrap_or_default();
            ensure!(
                system.contains("# Media/V4L2 Subsystem Details"),
                "media.md did not reach Stage 10"
            );
            self.evidence.lock().unwrap().stage10_received_media_guide = true;
            return Ok(json_response(json!({"findings": verified_findings()})));
        }

        if tag.contains("s:11]") {
            return Ok(text_response(
                "Commit fixture-head\nAuthor: Regression Test\nSubject: V4L2 callback validation\n\n> fmt = v4l2_subdev_state_get_format(...);\n\nFour deliberately unsafe variants remain findings; both guarded callbacks are dismissed using media-core check_state evidence.",
            ));
        }

        bail!("unexpected pipeline request with context tag {tag:?}")
    }

    fn estimate_tokens(&self, _request: &AiRequest) -> usize {
        0
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: "deterministic-v4l2-pipeline".to_string(),
            context_window_size: 1_000_000,
        }
    }
}

fn request_text(request: &AiRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|message| message.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

fn json_response(value: Value) -> AiResponse {
    text_response(&value.to_string())
}

fn text_response(content: &str) -> AiResponse {
    AiResponse {
        content: Some(content.to_string()),
        thought: None,
        thought_signature: None,
        tool_calls: None,
        usage: None,
        truncated: false,
    }
}

fn tool_response(call: ToolCall) -> AiResponse {
    AiResponse {
        content: None,
        thought: None,
        thought_signature: None,
        tool_calls: Some(vec![call]),
        usage: None,
        truncated: false,
    }
}

fn concern(function: &str, reasoning: &str) -> Value {
    json!({
        "type": "NULL Pointer Dereference",
        "description": format!("Unchecked format lookup in {function}"),
        "reasoning": reasoning,
        "preexisting": false,
        "locations": [{
            "file": DRIVER_PATH,
            "function_or_symbol": function,
            "line_range": null,
            "why_this_location_matters": "The callback dereferences the lookup result here."
        }]
    })
}

fn unsafe_concerns() -> Vec<Value> {
    vec![
        concern(
            "formatter_case_alpha",
            "The only shown caller invokes the callback directly without check_state().",
        ),
        concern(
            "formatter_case_beta",
            "invoke_case_beta_direct() bypasses the guarded media-core path.",
        ),
        concern(
            "formatter_case_gamma",
            "The wrapper validates code->pad while the callback looks up code->pad + 1.",
        ),
        concern(
            "formatter_case_delta",
            "Scenario 4 invokes the callback directly without check_state().",
        ),
    ]
}

fn stage3_output() -> Value {
    json!({
        "concerns": unsafe_concerns(),
        "dismissed_concerns": guarded_dismissal()
    })
}

fn guarded_dismissal() -> Vec<Value> {
    vec![
        json!({
            "type": "NULL Pointer Dereference",
            "description": "Possible unchecked format lookup in formatter_subdev_enum_mbus_code",
            "reasoning": format!(
                "Stage 3 retrieved {CORE_PATH} through git_read_files. {CORE_MARKER} identifies check_state(), which validates the exact state, code->pad, and code->stream before call_enum_mbus_code() invokes the registered callback."
            ),
            "locations": [
                {
                    "file": CORE_PATH,
                    "function_or_symbol": "check_state",
                    "line_range": null,
                    "why_this_location_matters": "This is the concrete caller-side non-NULL proof."
                },
                {
                    "file": DRIVER_PATH,
                    "function_or_symbol": "formatter_subdev_enum_mbus_code",
                    "line_range": null,
                    "why_this_location_matters": "The callback reuses the exact checked lookup."
                }
            ]
        }),
        json!({
            "type": "NULL Pointer Dereference",
            "description": "Possible unchecked format lookup in formatter_subdev_set_fmt",
            "reasoning": format!(
                "Stage 3 retrieved {CORE_PATH} through git_read_files. {CORE_MARKER} identifies check_format() and check_state(), which validate the exact state, format->pad, and format->stream before call_set_fmt() invokes the registered callback."
            ),
            "locations": [
                {
                    "file": CORE_PATH,
                    "function_or_symbol": "check_format",
                    "line_range": null,
                    "why_this_location_matters": "This passes the exact lookup values to check_state."
                },
                {
                    "file": DRIVER_PATH,
                    "function_or_symbol": "formatter_subdev_set_fmt",
                    "line_range": null,
                    "why_this_location_matters": "The callback reuses the exact checked lookup."
                }
            ]
        }),
    ]
}

fn verified_findings() -> Vec<Value> {
    unsafe_concerns()
        .into_iter()
        .map(|concern| {
            json!({
                "problem": concern["description"],
                "severity": "High",
                "severity_explanation": concern["reasoning"],
                "preexisting": false,
                "locations": concern["locations"].as_array().unwrap().iter().map(|location| {
                    json!({
                        "file": location["file"],
                        "function_or_symbol": location["function_or_symbol"],
                        "line": null,
                        "code_snippet": "fmt->code",
                        "why_this_location_matters": location["why_this_location_matters"]
                    })
                }).collect::<Vec<_>>()
            })
        })
        .collect()
}

fn manifest_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn create_fixture_repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let core = temp.path().join(CORE_PATH);
    let driver = temp.path().join(DRIVER_PATH);
    fs::create_dir_all(core.parent().unwrap()).unwrap();
    fs::create_dir_all(driver.parent().unwrap()).unwrap();
    fs::copy(
        manifest_path("tests/fixtures/issue_334_pipeline/v4l2-subdev.c"),
        &core,
    )
    .unwrap();
    fs::copy(
        manifest_path("tests/fixtures/issue_334_pipeline/formatter-base.c"),
        &driver,
    )
    .unwrap();

    git(temp.path(), &["init", "-q"]);
    git(temp.path(), &["config", "user.name", "Regression Test"]);
    git(
        temp.path(),
        &["config", "user.email", "regression@example.com"],
    );
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-q", "-m", "fixture baseline"]);

    fs::copy(
        manifest_path("tests/fixtures/issue_334_pipeline/formatter-patched.c"),
        &driver,
    )
    .unwrap();
    git(temp.path(), &["add", DRIVER_PATH]);
    git(
        temp.path(),
        &["commit", "-q", "-m", "media: add formatter callbacks"],
    );
    temp
}

fn output_functions(items: &[Value]) -> BTreeSet<String> {
    items
        .iter()
        .flat_map(|item| item["locations"].as_array().into_iter().flatten())
        .filter_map(|location| location["function_or_symbol"].as_str())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn issue_334_runs_through_phase0_tools_and_verification_stages() {
    let repo = create_fixture_repo();
    let diff = git(repo.path(), &["diff", "HEAD~1..HEAD", "--", DRIVER_PATH]);
    let git_show = git(repo.path(), &["show", "--format=fuller", "--patch", "HEAD"]);
    let head = git(repo.path(), &["rev-parse", "HEAD"]).trim().to_string();

    assert!(diff.contains("formatter_subdev_enum_mbus_code"));
    assert!(!diff.contains(CORE_PATH));
    assert!(!diff.contains(CORE_MARKER));

    let provider = Arc::new(PipelineProvider::new());
    let tools = Arc::new(ToolBox::new(repo.path().to_path_buf(), None));
    let prompts = PromptRegistry::new(manifest_path("third_party/prompts/kernel"));
    let mut worker = Worker::new(
        provider.clone(),
        tools,
        prompts,
        WorkerConfig {
            max_input_tokens: 1_000_000,
            max_interactions: 4,
            temperature: 0.0,
            custom_prompt: None,
            series_range: None,
            stages: Some(vec![3]),
        },
    );

    let result = worker
        .run(
            json!({
                "id": 334,
                "patch_index": 1,
                "patches": [{
                    "index": 1,
                    "diff": diff,
                    "git_show": git_show,
                    "commit_id": head
                }]
            }),
            None,
        )
        .await
        .unwrap();

    let evidence = provider.evidence();
    assert!(evidence.phase0_saw_media_index_and_patch);
    assert!(evidence.stage3_received_media_guide);
    assert!(evidence.stage3_initial_context_excluded_core_file);
    assert!(evidence.stage3_tool_retrieved_core_file);
    assert!(evidence.stage10_received_media_guide);
    assert!(result.history.iter().any(|message| {
        message.role == AiRole::Tool
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.contains(CORE_MARKER))
    }));

    let output = result.output.unwrap();
    let findings = output["findings"].as_array().unwrap();
    let dismissed = output["dismissed_concerns"].as_array().unwrap();
    let actual_findings = output_functions(findings);
    let actual_dismissed = output_functions(dismissed);
    let expectations: Expectations = serde_json::from_str(
        &fs::read_to_string(manifest_path(
            "tests/fixtures/issue_334_pipeline/expectations.json",
        ))
        .unwrap(),
    )
    .unwrap();

    for case in expectations.cases {
        if case.expected_finding {
            assert!(
                actual_findings.contains(&case.function),
                "expected finding for {}",
                case.function
            );
        } else {
            assert!(!actual_findings.contains(&case.function));
            assert!(actual_dismissed.contains(&case.function));
            let evidence = case.evidence.expect("safe case must name its evidence");
            assert!(evidence.contains(CORE_PATH));
            assert!(dismissed.iter().any(|item| {
                item["reasoning"]
                    .as_str()
                    .is_some_and(|reasoning| reasoning.contains(CORE_MARKER))
            }));
        }
    }
    assert_eq!(findings.len(), 4);
    assert_eq!(dismissed.len(), 2);
}
