//! Per-patchset review dispatch kind.
//!
//! Persisted as JSON in the `patchsets.review_context` column. A `NULL` column
//! (deserialized as `None`) selects the default patch-review pipeline, so
//! patch-only deployments store nothing extra. A present value selects an
//! alternate pipeline (currently only cherry-pick review).

use serde::{Deserialize, Serialize};

/// Selects which review pipeline processes a patchset.
///
/// Serialized to JSON, e.g. `{"type":"cherry-pick","original_sha":"..."}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ReviewKind {
    /// Review of an automated cherry-pick / merge-conflict resolution commit.
    #[serde(rename = "cherry-pick")]
    CherryPick {
        /// The original patch that was being ported.
        original_sha: String,
        /// The target base the patch was applied onto. Defaults to
        /// `<resolution>~1` when absent.
        #[serde(default)]
        base_sha: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cherry_pick_round_trips_through_json() {
        let k = ReviewKind::CherryPick {
            original_sha: "abc123".to_string(),
            base_sha: Some("def456".to_string()),
        };
        let json = serde_json::to_string(&k).unwrap();
        assert_eq!(
            json,
            r#"{"type":"cherry-pick","original_sha":"abc123","base_sha":"def456"}"#
        );
        assert_eq!(serde_json::from_str::<ReviewKind>(&json).unwrap(), k);
    }

    #[test]
    fn base_sha_defaults_to_none_when_absent() {
        let k: ReviewKind =
            serde_json::from_str(r#"{"type":"cherry-pick","original_sha":"abc"}"#).unwrap();
        assert_eq!(
            k,
            ReviewKind::CherryPick {
                original_sha: "abc".to_string(),
                base_sha: None
            }
        );
    }
}
