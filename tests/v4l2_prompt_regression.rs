use sashiko::worker::prompts::PromptRegistry;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Fixture {
    category: String,
    cases: Vec<FixtureCase>,
}

#[derive(Deserialize)]
struct FixtureCase {
    function: String,
    expected_finding: bool,
    dismissal_evidence: Option<String>,
    reason: Option<String>,
}

fn manifest_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

#[tokio::test]
async fn media_guide_is_selected_and_assembled_as_review_context() {
    let prompts = PromptRegistry::new(manifest_path("third_party/prompts/kernel"));
    let selected = vec!["media.md".to_string()];
    let (context, _) = prompts.build_context(Some(&selected)).await.unwrap();

    assert!(context.contains("# Media/V4L2 Subsystem Details"));
    assert!(context.contains("call_enum_mbus_code()"));
    assert!(context.contains("call_set_fmt()"));
    assert!(context.contains("check_state()"));
    assert!(context.contains("CONFIG_VIDEO_V4L2_SUBDEV_API"));
    assert!(context.contains("same `state`, `pad`, and"));
    assert!(context.contains("`stream` without another NULL check"));
}

#[test]
fn media_guide_requires_proof_and_preserves_unsafe_findings() {
    let guide = std::fs::read_to_string(manifest_path(
        "third_party/prompts/kernel/subsystem/media.md",
    ))
    .unwrap();

    for required_boundary in [
        "every real caller",
        "second caller bypasses the guard",
        "different state/pad/stream combination",
        "source context is insufficient",
        "must survive consolidation and final verification",
        "do not silently drop the concern",
        "v4l2_subdev_state_get_opposite_stream_format()",
    ] {
        assert!(
            guide.contains(required_boundary),
            "media guide omitted precision boundary: {required_boundary}"
        );
    }

    assert!(guide.contains("can return `NULL`"));
    assert!(!guide.contains("format pointers are always non-NULL"));
}

#[test]
fn issue_334_fixture_covers_safe_unsafe_bypass_mismatch_and_missing_context() {
    let source =
        std::fs::read_to_string(manifest_path("tests/fixtures/issue_334_v4l2_callbacks.c"))
            .unwrap();
    let fixture: Fixture = serde_json::from_str(
        &std::fs::read_to_string(manifest_path(
            "tests/fixtures/issue_334_v4l2_expectations.json",
        ))
        .unwrap(),
    )
    .unwrap();

    assert_eq!(fixture.category, "NULL Pointer Dereference");
    assert_eq!(fixture.cases.len(), 5);

    for case in &fixture.cases {
        assert!(source.contains(&case.function));
        if case.expected_finding {
            assert!(
                case.reason
                    .as_deref()
                    .is_some_and(|reason| !reason.is_empty())
            );
        } else {
            assert!(
                case.dismissal_evidence
                    .as_deref()
                    .is_some_and(|evidence| evidence.contains("check_state"))
            );
        }
    }

    assert!(source.contains("sd->ops->pad->enum_mbus_code(sd, state, request)"));
    assert!(source.contains(".enum_mbus_code = guarded_safe_callback"));
    assert!(source.contains(".flags = V4L2_SUBDEV_FL_STREAMS"));
    assert!(source.contains("return bypassable_callback(sd, state, request);"));
    assert!(source.contains("request->pad + 1"));
}
