use super::*;
use crate::ai::{
    AiProvider, AiRequest, AiResponse, AiRole, AiUsage, ProviderCapabilities, ToolCall,
};
use crate::toolbox::ToolBox;
use crate::workflow::engine::WorkflowEngine;
use crate::workflow::graph::WorkflowStep;
use crate::workflows::linux_patch_review::{
    build_linux_patch_review_workflow_with_options, report_stage,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Barrier, Notify};

const MARKERS: [&str; 3] = ["conditional-path", "excluded-condition", "unresolved-path"];

fn findings() -> Vec<Value> {
    MARKERS.iter().map(|marker| json!({
        "problem": marker,
        "severity": "High",
        "severity_explanation": "Original technical reasoning must survive the check.",
        "preexisting": false,
        "locations": [{"file": "driver.c", "function_or_symbol": "flag", "line": 1,
                       "code_snippet": "int flag = 0;", "why_this_location_matters": "The controlling state"}]
    })).collect()
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = crate::git_cmd::in_dir(path).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn fixture() -> (tempfile::TempDir, LinuxPatchReviewState) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    let mut target = String::new();
    for flag in [0, 1] {
        std::fs::write(dir.path().join("driver.c"), format!("int flag = {flag};\n")).unwrap();
        git(dir.path(), &["add", "driver.c"]);
        git(
            dir.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "change flag",
            ],
        );
        if flag == 0 {
            target = git(dir.path(), &["rev-parse", "HEAD"]);
        }
    }
    for (name, text) in [
        ("false-positive-guide.md", "false-positive-guide-marker"),
        ("severity.md", "severity-guide-marker"),
        ("subsystem/example.md", "subsystem-guide-marker"),
        ("patterns/example.md", "pattern-guide-marker"),
    ] {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let state = LinuxPatchReviewState {
        target_commit_sha: target,
        baseline_sha: "unrelated-baseline-marker".into(),
        target_commit_diff: "commit-message-marker".into(),
        prefetched_context: "prefetch-marker".into(),
        selected_guides: vec!["example.md".into()],
        custom_prompt: Some("custom-prompt-marker".into()),
        follow_up_series_context: Some("follow-up-series-marker".into()),
        patch_concerns: MARKERS
            .iter()
            .map(|marker| {
                json!({
                    "type": "Bug", "description": marker, "reasoning": "candidate reasoning",
                    "preexisting": false, "locations": []
                })
            })
            .collect(),
        findings: findings(),
        ..Default::default()
    };
    (dir, state)
}

struct Provider {
    target: String,
    seen: Mutex<Vec<AiRequest>>,
    completed: Mutex<Vec<usize>>,
    first_turn: Barrier,
    third_finished: Notify,
    fail_second: bool,
    reject_all: bool,
    unreachable_all: bool,
    verified_findings: Vec<Value>,
}

impl Provider {
    fn new(target: String) -> Self {
        Self {
            target,
            seen: Mutex::new(Vec::new()),
            completed: Mutex::new(Vec::new()),
            first_turn: Barrier::new(3),
            third_finished: Notify::new(),
            fail_second: false,
            reject_all: false,
            unreachable_all: false,
            verified_findings: findings(),
        }
    }
}

fn response(content: Option<String>, tool_calls: Option<Vec<ToolCall>>) -> AiResponse {
    AiResponse {
        content,
        thought: None,
        thought_signature: None,
        tool_calls,
        truncated: false,
        usage: Some(AiUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: Some(2),
        }),
    }
}

#[async_trait]
impl AiProvider for Provider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        self.seen.lock().unwrap().push(request.clone());
        let prompt = request.messages[0].content.as_deref().unwrap();
        if prompt.starts_with("# Verification and severity estimation") {
            return Ok(response(
                Some(json!({"findings": self.verified_findings}).to_string()),
                None,
            ));
        }
        if prompt.starts_with("# LKML-friendly report generation") {
            return Ok(response(Some("commit test\nAuthor: Test\nThe remaining paths need attention.\n> int flag = 0;\nPlease check this condition.\n".into()), None));
        }
        assert!(prompt.starts_with("# Reachability check"));
        let index = MARKERS
            .iter()
            .position(|marker| prompt.contains(marker))
            .unwrap();
        if !request.messages.iter().any(|m| m.role == AiRole::Tool) {
            // All three first requests must overlap; serial checks would time out.
            self.first_turn.wait().await;
            return Ok(response(
                None,
                Some(vec![ToolCall {
                    id: format!("read-{index}"),
                    function_name: "git_read_files".into(),
                    arguments: json!({"revision": self.target, "files": [{"path": "driver.c"}]}),
                    thought_signature: None,
                }]),
            ));
        }
        for tool in request.messages.iter().filter(|m| m.role == AiRole::Tool) {
            let text = tool.content.as_deref().unwrap();
            assert!(
                text.contains("int flag = 0;"),
                "must read frozen target, not HEAD: {text}"
            );
            assert!(!text.contains("Duplicate tool call blocked"));
        }
        if index == 1 && self.fail_second {
            anyhow::bail!("model unavailable");
        }
        if index == 0 {
            self.third_finished.notified().await;
        }
        if index == 2 {
            self.third_finished.notify_one();
        }
        self.completed.lock().unwrap().push(index);
        let text = if self.unreachable_all {
            "Current driver.c callers supply flag=0; the existing faulty branch requires flag=1. A future or out-of-tree caller supplying flag=1 could expose the same defect.\ncurrently_unreachable: true\nDecision: keep"
        } else if self.reject_all || index == 1 {
            "driver.c:1 flag is fixed at zero; the required nonzero state is excluded on the only entry path.\ncurrently_unreachable: false\nDecision: reject"
        } else if index == 0 {
            "driver.c:1 flag is zero on the reported path. The specified configuration can reach it.\ncurrently_unreachable: false\nDecision: keep"
        } else {
            "Only one path has been inspected; the alternate configuration still needs evidence."
        };
        let text = if !self.unreachable_all && self.verified_findings[index]["preexisting"] == true
        {
            text.replace("\ncurrently_unreachable: false", "")
        } else {
            text.into()
        };
        Ok(response(Some(text), None))
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: "mock".into(),
            context_window_size: 100_000,
        }
    }
}

fn environment(path: &Path, provider: Arc<Provider>) -> WorkflowEnv<'_> {
    WorkflowEnv {
        provider,
        tools: Arc::new(ToolBox::new(path.to_path_buf(), None)),
        base_dir: path,
        context_tag: Some("review-context".into()),
    }
}

#[test]
fn rejection_requires_an_explained_unambiguous_final_decision() {
    assert_eq!(
        parse_audit(
            "driver.c:init excludes the necessary state on every relevant path.\nDecision: reject\n"
        )
        .decision,
        Decision::Reject
    );
    for text in [
        "",
        "Decision: reject",
        "Needs evidence.\nDecision: keep",
        "Not enough evidence.",
        "Evidence\nDecision: reject\nBut the other caller remains unchecked.",
        "Decision: keep\nEvidence\nDecision: reject",
        "Evidence\n> Decision: reject",
        "```\nDecision: reject\n```",
        "{\"decision\":\"reject\"}",
    ] {
        assert_eq!(
            parse_audit(text).decision,
            Decision::Keep,
            "unsafe rejection: {text}"
        );
    }
}

#[test]
fn reachability_attribute_is_separate_from_decision() {
    assert_eq!(
        parse_audit(
            "Current callers exclude the faulty state; a future caller could supply it.\ncurrently_unreachable: true\nDecision: keep"
        ),
        AuditResult {
            decision: Decision::Keep,
            currently_unreachable: Some(true)
        }
    );
    assert_eq!(
        parse_audit(
            "The supported configuration reaches this path.\ncurrently_unreachable: false\nDecision: keep"
        ),
        AuditResult {
            decision: Decision::Keep,
            currently_unreachable: Some(false)
        }
    );
    assert_eq!(
        parse_audit(
            "The callee rejects the state before the faulty operation.\ncurrently_unreachable: false\nDecision: reject"
        ),
        AuditResult {
            decision: Decision::Reject,
            currently_unreachable: Some(false)
        }
    );
    for response in [
        "currently_unreachable: true",
        "currently_unreachable: true\nDecision: keep",
        "currently_unreachable: false\nDecision: reject",
        "Decision: currently_unreachable",
        "Evidence.\nDecision: currently_unreachable",
        "The other caller remains unexamined.\nDecision: keep",
        "Evidence.\n> currently_unreachable: true\nDecision: keep",
        "```\nEvidence.\ncurrently_unreachable: true\nDecision: keep\n```",
        "Evidence.\ncurrently_unreachable: true\nDecision: keep\nAnother caller still needs checking.",
        "Evidence.\ncurrently_unreachable: true\nAnother caller still needs checking.\nDecision: keep",
        "Evidence.\ncurrently_unreachable: true\ncurrently_unreachable: false\nDecision: reject",
        "Evidence.\ncurrently_unreachable: uncertain\nDecision: reject",
        "Evidence.\ncurrently_unreachable: true\nDecision: reject\nDecision: keep",
        "Evidence.\ncurrently_unreachable: true\nDecision: currently_unreachable",
        "{\"currently_unreachable\":true}",
    ] {
        assert_eq!(
            parse_audit(response),
            AuditResult::default(),
            "unsafe classification: {response}"
        );
    }
}

#[tokio::test]
async fn parallel_checks_isolate_context_keep_wording_and_record_rejections() {
    let (dir, mut state) = fixture();
    let original = state.findings.clone();
    let provider = Arc::new(Provider::new(state.target_commit_sha.clone()));
    let env = environment(dir.path(), provider.clone());
    let events = Mutex::new(Vec::new());
    let callback = |event| events.lock().unwrap().push(event);
    let stage = ReachabilityStage::new(5, 0.2);
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        stage.execute(&env, &mut state, Some(&callback)),
    )
    .await
    .expect("checks must be concurrent")
    .unwrap();
    let mut reachable = original[0].clone();
    reachable["currently_unreachable"] = json!(false);
    assert_eq!(state.findings, vec![reachable, original[2].clone()]);
    assert_eq!(state.reachability_checks.len(), 3);
    assert_eq!(state.reachability_checks[1]["finding"], original[1]);
    assert_eq!(state.reachability_checks[1]["rejected"], true);
    assert_eq!(state.reachability_checks[2]["rejected"], false);
    assert!(state.reachability_checks[2]["currently_unreachable"].is_null());
    assert_eq!(state.reachability_checks[2]["policy_filtered"], false);
    assert_eq!(
        (outcome.tokens_in, outcome.tokens_out, outcome.tokens_cached),
        (60, 30, 12)
    );
    for request in provider.seen.lock().unwrap().iter() {
        assert!(request.response_format.is_none());
        assert_eq!(request.temperature, Some(0.2));
        let index = MARKERS
            .iter()
            .position(|marker| {
                request.messages[0]
                    .content
                    .as_deref()
                    .unwrap()
                    .contains(marker)
            })
            .unwrap();
        assert_eq!(
            request.context_tag.as_deref(),
            Some(format!("review-context [s:reachability finding:{index}]").as_str())
        );
        let system = request.system.as_deref().unwrap();
        assert!(system.contains(&state.target_commit_sha));
        for absent in [
            "Stage 10",
            "Stage 11",
            "review_check.audit",
            "JSON",
            "parent",
            "schema",
            "severity-guide-marker",
            "subsystem-guide-marker",
            "pattern-guide-marker",
            "false-positive-guide-marker",
            "prefetch-marker",
            "custom-prompt-marker",
            "follow-up-series-marker",
        ] {
            assert!(
                !system.contains(absent),
                "unexpected instruction/context: {absent}"
            );
        }
        for message in &request.messages {
            let content = message.content.as_deref().unwrap_or("");
            for (other, marker) in MARKERS.iter().enumerate() {
                if index != other {
                    assert!(!content.contains(marker));
                }
            }
            assert!(!content.contains("Original prior-stage conversation"));
        }
    }
    let history_inputs: Vec<_> = outcome
        .history
        .iter()
        .filter(|m| m.role == AiRole::User)
        .filter_map(|m| {
            MARKERS
                .iter()
                .position(|marker| m.content.as_deref().unwrap_or("").contains(marker))
        })
        .collect();
    assert_eq!(history_inputs, vec![0, 1, 2]);
    assert_eq!(
        outcome
            .history
            .iter()
            .filter(|m| m.role == AiRole::System)
            .count(),
        3
    );
    let completed = provider.completed.lock().unwrap();
    assert!(completed.iter().position(|i| *i == 2) < completed.iter().position(|i| *i == 0));
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, WorkflowEvent::StageStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, WorkflowEvent::StageFinished { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn original_verification_precedes_audit_and_report_sees_only_survivors() {
    for reject_all in [false, true] {
        let (dir, mut state) = fixture();
        state.findings.clear();
        let mut mock = Provider::new(state.target_commit_sha.clone());
        mock.reject_all = reject_all;
        let provider = Arc::new(mock);
        let env = environment(dir.path(), provider.clone());
        let mut workflow = build_linux_patch_review_workflow_with_options(5, 0.0);
        let names: Vec<_> = workflow
            .steps
            .iter()
            .filter_map(|step| match step {
                WorkflowStep::Stage(stage) => Some(stage.name()),
                _ => None,
            })
            .collect();
        assert!(names.ends_with(&["verification", "reachability", "report"]));
        let start = workflow
            .steps
            .iter()
            .position(
                |step| matches!(step, WorkflowStep::Stage(stage) if stage.name() == "verification"),
            )
            .unwrap();
        workflow.steps.drain(..start);
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            WorkflowEngine::execute(&workflow, &env, &mut state, None),
        )
        .await
        .unwrap()
        .unwrap();
        let requests = provider.seen.lock().unwrap();
        let verification: Vec<_> = requests
            .iter()
            .filter(|r| {
                r.messages[0]
                    .content
                    .as_deref()
                    .unwrap()
                    .starts_with("# Verification and severity estimation")
            })
            .collect();
        assert_eq!(
            verification.len(),
            1,
            "upstream verification receives the full list once"
        );
        let prompt = verification[0].messages[0].content.as_deref().unwrap();
        for marker in MARKERS {
            assert!(prompt.contains(marker));
        }
        assert!(prompt.contains("severity-guide-marker"));
        assert!(prompt.contains("false-positive-guide-marker"));
        assert!(prompt.contains("follow-up-series-marker"));
        assert_eq!(state.reachability_checks.len(), 3);
        let reports: Vec<_> = requests
            .iter()
            .filter(|r| {
                r.messages[0]
                    .content
                    .as_deref()
                    .unwrap()
                    .starts_with("# LKML-friendly report generation")
            })
            .collect();
        assert_eq!(reports.len(), usize::from(!reject_all));
        if reject_all {
            assert!(outcome.early_exit);
            assert!(state.findings.is_empty());
            assert!(state.review_inline.is_empty());
        } else {
            let prompt = reports[0].messages[0].content.as_deref().unwrap();
            assert!(prompt.contains(MARKERS[0]));
            assert!(!prompt.contains(MARKERS[1]));
            assert!(prompt.contains(MARKERS[2]));
        }
    }
}

#[tokio::test]
async fn unreachable_findings_are_qualified_and_filtered_before_reporting() {
    for (severities, retained_indices) in [
        (["Low", "High", "Critical"], vec![1, 2]),
        (["Medium", "high", "critical"], vec![1, 2]),
        (["Low", "Medium", "Low"], vec![]),
    ] {
        let (dir, mut state) = fixture();
        let mut original = findings();
        for (finding, severity) in original.iter_mut().zip(severities) {
            finding["severity"] = json!(severity);
        }
        let mut mock = Provider::new(state.target_commit_sha.clone());
        mock.unreachable_all = true;
        mock.verified_findings = original.clone();
        let provider = Arc::new(mock);
        let env = environment(dir.path(), provider.clone());
        let mut workflow = build_linux_patch_review_workflow_with_options(5, 0.0);
        let start = workflow
            .steps
            .iter()
            .position(
                |step| matches!(step, WorkflowStep::Stage(stage) if stage.name() == "verification"),
            )
            .unwrap();
        workflow.steps.drain(..start);
        state.findings.clear();
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            WorkflowEngine::execute(&workflow, &env, &mut state, None),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(state.findings.len(), retained_indices.len());
        for (finding, index) in state.findings.iter().zip(&retained_indices) {
            assert_eq!(finding["currently_unreachable"], true);
            for key in [
                "problem",
                "severity",
                "severity_explanation",
                "preexisting",
                "locations",
            ] {
                assert_eq!(finding[key], original[*index][key]);
            }
        }
        for (index, check) in state.reachability_checks.iter().enumerate() {
            assert_eq!(check["finding"], original[index]);
            assert_eq!(check["currently_unreachable"], true);
            assert_eq!(
                check["rejected"], false,
                "reporting policy is not a technical refutation"
            );
            assert_eq!(check["policy_filtered"], !retained_indices.contains(&index));
        }
        let requests = provider.seen.lock().unwrap();
        let reports: Vec<_> = requests
            .iter()
            .filter(|request| {
                request.messages[0]
                    .content
                    .as_deref()
                    .unwrap()
                    .starts_with("# LKML-friendly report generation")
            })
            .collect();
        if retained_indices.is_empty() {
            assert!(outcome.early_exit);
            assert!(reports.is_empty());
        } else {
            assert_eq!(reports.len(), 1);
            let prompt = reports[0].messages[0].content.as_deref().unwrap();
            assert!(prompt.contains("Otherwise, if `\"currently_unreachable\": true`"));
            assert!(prompt.contains("Reachability evidence for currently unreachable findings:"));
            assert!(prompt.contains("Current driver.c callers supply flag=0"));
            assert!(
                !prompt.contains(MARKERS[0]),
                "filtered findings must not reach the report"
            );
            assert!(prompt.contains("\"currently_unreachable\": true"));
        }
    }
}

#[tokio::test]
async fn preexisting_findings_are_audited_without_a_reachability_attribute() {
    for unreachable_all in [false, true] {
        let (dir, mut state) = fixture();
        state.findings[0]["preexisting"] = json!(true);
        state.findings[1]["preexisting"] = json!(true);
        // A stale attribute must not survive, even when the model also returns it.
        state.findings[0]["currently_unreachable"] = json!(true);
        let original = state.findings.clone();
        let mut mock = Provider::new(state.target_commit_sha.clone());
        mock.verified_findings = original.clone();
        mock.unreachable_all = unreachable_all;
        let provider = Arc::new(mock);
        let env = environment(dir.path(), provider.clone());
        tokio::time::timeout(
            Duration::from_secs(10),
            ReachabilityStage::new(5, 0.0).execute(&env, &mut state, None),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(provider.completed.lock().unwrap().len(), 3);
        assert_eq!(state.reachability_checks.len(), 3);
        for index in [0, 1] {
            let check = &state.reachability_checks[index];
            assert_eq!(check["finding"], original[index]);
            assert!(check["currently_unreachable"].is_null());
            assert_eq!(check["policy_filtered"], false);
        }
        assert_eq!(state.reachability_checks[0]["rejected"], false);
        assert_eq!(state.reachability_checks[1]["rejected"], !unreachable_all);
        assert_eq!(state.findings.len(), if unreachable_all { 3 } else { 2 });
        assert_eq!(state.findings[0]["problem"], original[0]["problem"]);
        assert_eq!(state.findings[0]["preexisting"], true);
        for finding in &state.findings {
            if finding["preexisting"] == true {
                assert!(finding.get("currently_unreachable").is_none());
            }
        }
        if unreachable_all {
            assert_eq!(state.findings[2]["preexisting"], false);
            assert_eq!(state.findings[2]["currently_unreachable"], true);
            assert!(
                state.reachability_checks[0]["response"]
                    .as_str()
                    .unwrap()
                    .contains("currently_unreachable: true")
            );
        } else {
            assert_eq!(state.findings[1], original[2]);
        }
    }
}

#[tokio::test]
async fn report_uses_only_retained_nonpreexisting_unreachable_evidence() {
    let (dir, mut state) = fixture();
    let original = state.findings.clone();
    state.findings.truncate(1);
    state.findings[0]["currently_unreachable"] = json!(true);
    state.reachability_checks = original
        .iter()
        .enumerate()
        .map(|(index, finding)| {
            json!({
                "finding": finding,
                "currently_unreachable": true,
                "rejected": index == 1,
                "policy_filtered": index == 2,
                "response": format!("Reachability evidence for {}", MARKERS[index]),
            })
        })
        .collect();
    let mut preexisting = original[0].clone();
    preexisting["preexisting"] = json!(true);
    preexisting["problem"] = json!("preexisting-finding");
    state.findings.push(preexisting.clone());
    // Old audit records may contain both attributes; do not report both qualifiers.
    state.reachability_checks.push(json!({
        "finding": preexisting,
        "currently_unreachable": true,
        "rejected": false,
        "policy_filtered": false,
        "response": "preexisting-audit-only",
    }));
    let provider = Arc::new(Provider::new(state.target_commit_sha.clone()));
    let env = environment(dir.path(), provider.clone());
    report_stage(5, 0.0)
        .execute(&env, &mut state, None)
        .await
        .unwrap();
    let requests = provider.seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let prompt = requests[0].messages[0].content.as_deref().unwrap();
    assert!(prompt.contains("Reachability evidence for conditional-path"));
    assert!(!prompt.contains(MARKERS[1]));
    assert!(!prompt.contains(MARKERS[2]));
    assert!(prompt.contains("preexisting-finding"));
    assert!(!prompt.contains("preexisting-audit-only"));
}

#[tokio::test]
async fn model_failure_does_not_apply_partial_filtering() {
    let (dir, mut state) = fixture();
    let original = state.findings.clone();
    let mut mock = Provider::new(state.target_commit_sha.clone());
    mock.fail_second = true;
    let env = environment(dir.path(), Arc::new(mock));
    let stage = ReachabilityStage::new(5, 0.0);
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        stage.execute(&env, &mut state, None),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(format!("{error:#}").contains("model unavailable"));
    assert_eq!(state.findings, original);
    assert!(state.reachability_checks.is_empty());
}

#[tokio::test]
async fn invalid_target_cannot_fall_back_to_head_and_empty_input_needs_no_model() {
    let (dir, mut state) = fixture();
    let provider = Arc::new(Provider::new(state.target_commit_sha.clone()));
    let env = environment(dir.path(), provider.clone());
    let stage = ReachabilityStage::new(5, 0.0);
    for revision in ["HEAD", "--help", "ffffffffffffffffffffffffffffffffffffffff"] {
        state.target_commit_sha = revision.into();
        assert!(stage.execute(&env, &mut state, None).await.is_err());
        assert_eq!(state.findings, findings());
    }
    state.findings.clear();
    let outcome = stage.execute(&env, &mut state, None).await.unwrap();
    assert!(outcome.history.is_empty());
    assert!(provider.seen.lock().unwrap().is_empty());
}
