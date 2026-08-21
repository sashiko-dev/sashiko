//! Integration tests verifying that the cherry-pick review pipeline
//! produces identical results given the same AI responses.
//!
//! V2 variant: uses execute_pipeline() with CherryPickReviewPipeline.

use sashiko::ai::AiProvider;
use sashiko::pipelines::cherry_pick_review::CherryPickReviewPipeline;
use sashiko::pipelines::{PipelineEnv, execute_pipeline};
use sashiko::review_kind::ReviewKind;
use sashiko::test_support::replay::*;
use sashiko::toolbox::ToolBox;
use sashiko::worker::PromptRegistry;
use serde_json::json;
use std::sync::Arc;

// ── helpers ─────────────────────────────────────────────────────────────

fn patchset() -> serde_json::Value {
    json!({
        "subject": "[PATCH] fix merge conflict in test.c",
        "conflict": {
            "resolution_sha": "ccc333ccc333ccc333ccc333ccc333ccc333ccc3",
            "original_sha": "aaa111aaa111aaa111aaa111aaa111aaa111aaa1",
            "base_sha": "bbb222bbb222bbb222bbb222bbb222bbb222bbb2"
        },
        "patches": [{
            "diff": concat!(
                "diff --git a/test.c b/test.c\n",
                "--- a/test.c\n",
                "+++ b/test.c\n",
                "@@ -10,3 +10,4 @@\n",
                " int existing_func(void) {\n",
                "+    int x = 0;\n",
                "     return 0;\n",
                " }\n"
            )
        }]
    })
}

fn pipeline() -> CherryPickReviewPipeline {
    CherryPickReviewPipeline::from_review_kind(
        &ReviewKind::CherryPick {
            original_sha: "aaa111aaa111aaa111aaa111aaa111aaa111aaa1".into(),
            base_sha: Some("bbb222bbb222bbb222bbb222bbb222bbb222bbb2".into()),
        },
        "ccc333ccc333ccc333ccc333ccc333ccc333ccc3".into(),
    )
}

async fn run_pipeline(responses: Vec<CannedResponse>) -> (serde_json::Value, Vec<RecordedCall>) {
    let provider = Arc::new(ReplayProvider::new(responses));
    let tmp = tempfile::tempdir().unwrap();
    let prompts = PromptRegistry::new(tmp.path().to_path_buf());
    let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
    let env = PipelineEnv {
        provider: provider.clone() as Arc<dyn AiProvider>,
        tools,
        prompts: &prompts,
        temperature: 0.0,
        max_interactions: 3,
        context_tag: None,
        stages: None,
        series_range: None,
    };

    let result = execute_pipeline(&pipeline(), &env, patchset(), None)
        .await
        .expect("pipeline should succeed");

    let output = result.output.unwrap_or(json!({}));
    let calls = provider.recorded_calls();
    (output, calls)
}

/// Empty parallel responses: Phase 0 (prescreen) + Planning + 7 stage responses.
/// V1 has a pre-screen and planning phase before the parallel stages.
fn empty_parallel() -> Vec<CannedResponse> {
    let mut r = Vec::new();
    // Phase 0: pre-screen (subsystem guide selection)
    r.push(CannedResponse {
        stage_id: "phase0".into(),
        content: r#"{"selected_prompts": []}"#.into(),
        ..Default::default()
    });
    // Planning phase: select which of stages 4-7 to run
    r.push(CannedResponse {
        stage_id: "planning".into(),
        content: r#"{"relevant_stages": [4, 5, 6, 7]}"#.into(),
        ..Default::default()
    });
    // Stages 1-7: empty concerns
    for n in 1..=7 {
        r.push(stage_concerns_response(
            &format!("stage_{n}"),
            vec![],
            vec![],
        ));
    }
    r
}

// ═══════════════════════════════════════════════════════════════════════
// Scenario 1: Clean merge — all stages return zero concerns
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_1_no_concerns() {
    let (output, calls) = run_pipeline(empty_parallel()).await;

    // Phase0 + Planning + 7 parallel = 9 calls
    assert_eq!(calls.len(), 9, "phase0 + planning + 7 parallel stages");

    let findings = output["findings"].as_array().expect("findings array");
    assert!(findings.is_empty());
    assert_eq!(output["concerns_count"], 0);
    assert_eq!(output["dismissed_concerns_count"], 0);
    assert_eq!(output["review_inline"], "No issues found.");
}

// ═══════════════════════════════════════════════════════════════════════
// Scenario 2: One concern, stage 8 dedup merges to zero
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_2_exit_after_dedup() {
    let mut responses = Vec::new();

    // Phase 0 + Planning
    responses.push(CannedResponse {
        stage_id: "phase0".into(),
        content: r#"{"selected_prompts": []}"#.into(),
        ..Default::default()
    });
    responses.push(CannedResponse {
        stage_id: "planning".into(),
        content: r#"{"relevant_stages": [4, 5, 6, 7]}"#.into(),
        ..Default::default()
    });

    // Stage 1: one concern; stages 2-7: empty
    responses.push(stage_concerns_response(
        "stage_1",
        vec![concern("logic", "dropped null check", "high")],
        vec![],
    ));
    for n in 2..=7 {
        responses.push(stage_concerns_response(
            &format!("stage_{n}"),
            vec![],
            vec![],
        ));
    }

    // Stage 8: dedup to zero
    responses.push(stage_concerns_response("stage_8", vec![], vec![]));

    let (output, calls) = run_pipeline(responses).await;

    // Phase0 + Planning + 7 + 1 dedup = 10 calls
    assert_eq!(calls.len(), 10, "should exit after stage 8 dedup");
    assert!(output["findings"].as_array().unwrap().is_empty());
    assert_eq!(output["concerns_count"], 1);
}

// ═══════════════════════════════════════════════════════════════════════
// Scenario 3: Full pipeline — concerns survive to final report
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
#[allow(clippy::vec_init_then_push)]
async fn scenario_3_full_pipeline() {
    let mut responses = Vec::new();

    // Phase 0 + Planning
    responses.push(CannedResponse {
        stage_id: "phase0".into(),
        content: r#"{"selected_prompts": []}"#.into(),
        ..Default::default()
    });
    responses.push(CannedResponse {
        stage_id: "planning".into(),
        content: r#"{"relevant_stages": [4, 5, 6, 7]}"#.into(),
        ..Default::default()
    });

    // Stages 1-2: produce concerns; 3-7: empty
    responses.push(stage_concerns_response(
        "stage_1",
        vec![concern("semantic", "intent changed", "high")],
        vec![],
    ));
    responses.push(stage_concerns_response(
        "stage_2",
        vec![concern("dropped", "missing null check", "high")],
        vec![],
    ));
    for n in 3..=7 {
        responses.push(stage_concerns_response(
            &format!("stage_{n}"),
            vec![],
            vec![],
        ));
    }

    // Stage 8: dedup keeps both
    responses.push(stage_concerns_response(
        "stage_8",
        vec![
            concern("semantic", "intent changed", "high"),
            concern("dropped", "missing null check", "high"),
        ],
        vec![],
    ));

    // Stage 9: keeps both
    responses.push(stage_concerns_response(
        "stage_9",
        vec![
            concern("semantic", "intent changed", "high"),
            concern("dropped", "missing null check", "high"),
        ],
        vec![],
    ));

    // Stage 10: 2 findings
    let f1 = finding("intent changed in merge", "high", "resolution_introduced");
    let f2 = finding("null check dropped", "high", "resolution_introduced");
    responses.push(stage_findings_response(
        "stage_10",
        vec![f1.clone(), f2.clone()],
    ));

    // Origin classification
    responses.push(stage_findings_response(
        "origin",
        vec![f1.clone(), f2.clone()],
    ));

    // Stage 11 conflict report
    responses.push(stage_inline_response(
        "report",
        "commit ccc333ccc333ccc333ccc333ccc333ccc333ccc3\nAuthor: Test Author\nSubject: [PATCH] fix merge conflict in test.c\n\n> int x = 0;\n\nFinding: intent changed in merge\nSeverity: high\n",
    ));

    let (output, calls) = run_pipeline(responses).await;

    // Phase0 + Planning + 7 + 5 = 14
    assert_eq!(calls.len(), 14, "full pipeline should make 14 calls");
    let findings = output["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 2);
    assert_eq!(output["concerns_count"], 2);
}

// ═══════════════════════════════════════════════════════════════════════
// Scenario 4: Mixed concerns + dismissed concerns
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_4_mixed_dismissed() {
    let mut responses = Vec::new();

    responses.push(CannedResponse {
        stage_id: "phase0".into(),
        content: r#"{"selected_prompts": []}"#.into(),
        ..Default::default()
    });
    responses.push(CannedResponse {
        stage_id: "planning".into(),
        content: r#"{"relevant_stages": [4, 5, 6, 7]}"#.into(),
        ..Default::default()
    });

    // Stage 1: one concern + one dismissed
    responses.push(stage_concerns_response(
        "stage_1",
        vec![concern("semantic", "variable renamed", "high")],
        vec![dismissed_concern("semantic", "whitespace only change")],
    ));
    for n in 2..=7 {
        responses.push(stage_concerns_response(
            &format!("stage_{n}"),
            vec![],
            vec![],
        ));
    }

    // Stage 8: keeps both
    responses.push(stage_concerns_response(
        "stage_8",
        vec![concern("semantic", "variable renamed", "high")],
        vec![dismissed_concern("semantic", "whitespace only change")],
    ));

    // Stage 9: keeps concern and dismissed
    responses.push(stage_concerns_response(
        "stage_9",
        vec![concern("semantic", "variable renamed", "high")],
        vec![dismissed_concern("semantic", "whitespace only change")],
    ));

    // Stage 10
    let f = finding(
        "variable renamed may break ABI",
        "high",
        "resolution_introduced",
    );
    responses.push(stage_findings_response("stage_10", vec![f.clone()]));

    // Origin classification
    responses.push(stage_findings_response("origin", vec![f.clone()]));

    // Report
    responses.push(stage_inline_response(
        "report",
        "commit ccc333ccc333ccc333ccc333ccc333ccc333ccc3\nAuthor: Test Author\nSubject: [PATCH] fix merge conflict in test.c\n\n> int x = 0;\n\nFinding: variable renamed may break ABI\nSeverity: high\n",
    ));

    let (output, calls) = run_pipeline(responses).await;

    assert_eq!(calls.len(), 14);
    let dismissed = output["dismissed_concerns_count"].as_u64().unwrap_or(0);
    assert!(dismissed >= 1, "should track dismissed concerns");
}

// ═══════════════════════════════════════════════════════════════════════
// Scenario 5: Stress test — many concerns
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_5_many_concerns() {
    let mut responses = Vec::new();

    responses.push(CannedResponse {
        stage_id: "phase0".into(),
        content: r#"{"selected_prompts": []}"#.into(),
        ..Default::default()
    });
    responses.push(CannedResponse {
        stage_id: "planning".into(),
        content: r#"{"relevant_stages": [4, 5, 6, 7]}"#.into(),
        ..Default::default()
    });

    // Stages 1-7: each produces 3 concerns + 1 dismissed
    for n in 1..=7 {
        responses.push(stage_concerns_response(
            &format!("stage_{n}"),
            vec![
                concern("type_a", &format!("concern A from stage {n}"), "high"),
                concern("type_b", &format!("concern B from stage {n}"), "medium"),
                concern("type_c", &format!("concern C from stage {n}"), "low"),
            ],
            vec![dismissed_concern("noise", &format!("noise from stage {n}"))],
        ));
    }

    // Stage 8: dedup to 5
    let deduped: Vec<_> = (1..=5)
        .map(|i| concern("merged", &format!("deduped concern {i}"), "high"))
        .collect();
    responses.push(stage_concerns_response("stage_8", deduped, vec![]));

    // Stage 9: keeps 3
    let resolved: Vec<_> = (1..=3)
        .map(|i| concern("resolved", &format!("resolved concern {i}"), "high"))
        .collect();
    responses.push(stage_concerns_response("stage_9", resolved, vec![]));

    // Stage 10: 2 findings
    let f1 = finding("real bug 1", "high", "resolution_introduced");
    let f2 = finding("real bug 2", "critical", "resolution_introduced");
    responses.push(stage_findings_response(
        "stage_10",
        vec![f1.clone(), f2.clone()],
    ));

    // Origin
    responses.push(stage_findings_response(
        "origin",
        vec![f1.clone(), f2.clone()],
    ));

    // Report
    responses.push(stage_inline_response(
        "report",
        "commit ccc333ccc333ccc333ccc333ccc333ccc333ccc3\nAuthor: Test Author\nSubject: [PATCH] fix merge conflict in test.c\n\n> int x = 0;\n\nFinding: real bug 1\nSeverity: high\n",
    ));

    let (output, calls) = run_pipeline(responses).await;

    assert_eq!(calls.len(), 14);
    assert_eq!(output["concerns_count"], 21, "7 stages * 3 concerns each");
}

// ═══════════════════════════════════════════════════════════════════════
// Scenario 6: Origin filtering
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_6_origin_filtering() {
    let mut responses = Vec::new();

    responses.push(CannedResponse {
        stage_id: "phase0".into(),
        content: r#"{"selected_prompts": []}"#.into(),
        ..Default::default()
    });
    responses.push(CannedResponse {
        stage_id: "planning".into(),
        content: r#"{"relevant_stages": [4, 5, 6, 7]}"#.into(),
        ..Default::default()
    });

    // Stage 1: one concern
    responses.push(stage_concerns_response(
        "stage_1",
        vec![concern("logic", "possible issue", "high")],
        vec![],
    ));
    for n in 2..=7 {
        responses.push(stage_concerns_response(
            &format!("stage_{n}"),
            vec![],
            vec![],
        ));
    }

    // Stage 8: keeps it
    responses.push(stage_concerns_response(
        "stage_8",
        vec![concern("logic", "possible issue", "high")],
        vec![],
    ));

    // Stage 9: keeps it
    responses.push(stage_concerns_response(
        "stage_9",
        vec![concern("logic", "possible issue", "high")],
        vec![],
    ));

    // Stage 10: 3 findings with different origins
    responses.push(stage_findings_response(
        "stage_10",
        vec![
            finding("resolution bug", "high", "resolution_introduced"),
            finding("pre-existing in base", "high", "base_preexisting"),
            finding("pre-existing low sev", "low", "original_patch_preexisting"),
        ],
    ));

    // Origin classification: all 3
    responses.push(stage_findings_response(
        "origin",
        vec![
            finding("resolution bug", "high", "resolution_introduced"),
            finding("pre-existing in base", "high", "base_preexisting"),
            finding("pre-existing low sev", "low", "original_patch_preexisting"),
        ],
    ));

    // After V1 filter_conflict_findings: only resolution_introduced + high/critical
    // Report runs with 1 finding
    responses.push(stage_inline_response(
        "report",
        "commit ccc333ccc333ccc333ccc333ccc333ccc333ccc3\nAuthor: Test Author\nSubject: [PATCH] fix merge conflict in test.c\n\n> int x = 0;\n\nFinding: resolution bug\nSeverity: high\n",
    ));

    let (output, calls) = run_pipeline(responses).await;

    assert_eq!(calls.len(), 14);
    let findings = output["findings"].as_array().unwrap();
    assert!(
        !findings.is_empty(),
        "at least the resolution_introduced finding"
    );
}
