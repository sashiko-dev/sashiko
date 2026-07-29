// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Cherry-pick review: context type and finding filter.
//!
//! Ported from the original merge-conflict-resolution hack (`d95dac6`), adapted
//! to the principled pipeline framework. The persisted dispatch payload lives in
//! `crate::review_kind::ReviewKind::CherryPick`; this module holds the richer
//! runtime context hydrated from git during the pipeline's `build_context`.

use serde::{Deserialize, Serialize};

/// Hydrated context for a cherry-pick / merge-conflict resolution review.
///
/// Semantics of the three commits involved:
/// - `original_*`: the upstream patch being ported.
/// - `base_*`: the target branch HEAD the patch was applied ONTO. Bugs already
///   present here are NOT resolution-introduced.
/// - `resolution_*`: what the automated agent produced. Only defects introduced
///   here by the merge/resolution itself are in scope.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct CherryPickContext {
    /// SHA of the automated resolution commit under review.
    pub resolution_sha: String,
    /// SHA of the original upstream patch being ported.
    pub original_sha: String,
    /// SHA of the target base the patch was applied onto.
    pub base_sha: String,
    /// Subject line of the resolution commit.
    #[serde(default)]
    pub resolution_subject: Option<String>,
    /// Subject line of the original patch.
    #[serde(default)]
    pub original_subject: Option<String>,
    /// Subject line of the base commit.
    #[serde(default)]
    pub base_subject: Option<String>,
    /// Full `git show`/diff of the original patch, for direct comparison against
    /// the resolution diff.
    #[serde(default)]
    pub original_diff: Option<String>,
}

/// Filter raw cherry-pick findings down to the ones worth surfacing.
///
/// Rules (by `severity` and `origin`):
/// - DROP all `low` severity.
/// - DROP everything `base_preexisting` (already in the target branch).
/// - DROP `original_patch_preexisting` unless `critical`.
/// - KEEP `resolution_introduced` at medium+ severity.
///
/// Non-array input yields an empty array.
pub fn filter_cherry_pick_findings(findings: &serde_json::Value) -> serde_json::Value {
    let arr = match findings.as_array() {
        Some(a) => a,
        None => return serde_json::json!([]),
    };

    let filtered: Vec<serde_json::Value> = arr
        .iter()
        .filter(|f| {
            let severity = f
                .get("severity")
                .and_then(|v| v.as_str())
                .unwrap_or("low")
                .to_lowercase();
            let origin = f
                .get("origin")
                .and_then(|v| v.as_str())
                .unwrap_or("resolution_introduced")
                .to_lowercase();

            // Drop all low severity.
            if severity == "low" {
                tracing::info!(
                    "Filtering out low-severity finding: {}",
                    f.get("description").and_then(|v| v.as_str()).unwrap_or("?")
                );
                return false;
            }

            // Drop all findings pre-existing in the target base branch.
            if origin == "base_preexisting" {
                tracing::info!(
                    "Filtering out base-preexisting finding: {}",
                    f.get("description").and_then(|v| v.as_str()).unwrap_or("?")
                );
                return false;
            }

            // Drop findings pre-existing in the original patch unless critical.
            if origin == "original_patch_preexisting" && severity != "critical" {
                tracing::info!(
                    "Filtering out original-patch-preexisting ({}) finding: {}",
                    severity,
                    f.get("description").and_then(|v| v.as_str()).unwrap_or("?")
                );
                return false;
            }

            true
        })
        .cloned()
        .collect();

    serde_json::json!(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn descriptions(v: &serde_json::Value) -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|f| f["description"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn keeps_resolution_introduced_medium_plus() {
        let findings = json!([
            {"description": "keep-me", "severity": "high", "origin": "resolution_introduced"},
            {"description": "keep-me-2", "severity": "medium", "origin": "resolution_introduced"},
        ]);
        let out = filter_cherry_pick_findings(&findings);
        assert_eq!(descriptions(&out), vec!["keep-me", "keep-me-2"]);
    }

    #[test]
    fn drops_low_severity_regardless_of_origin() {
        let findings = json!([
            {"description": "low", "severity": "low", "origin": "resolution_introduced"},
        ]);
        assert_eq!(
            filter_cherry_pick_findings(&findings)
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn drops_base_preexisting_even_when_critical() {
        let findings = json!([
            {"description": "base", "severity": "critical", "origin": "base_preexisting"},
        ]);
        assert_eq!(
            filter_cherry_pick_findings(&findings)
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn drops_original_preexisting_unless_critical() {
        let findings = json!([
            {"description": "orig-high", "severity": "high", "origin": "original_patch_preexisting"},
            {"description": "orig-crit", "severity": "critical", "origin": "original_patch_preexisting"},
        ]);
        assert_eq!(
            descriptions(&filter_cherry_pick_findings(&findings)),
            vec!["orig-crit"]
        );
    }

    #[test]
    fn defaults_missing_origin_to_resolution_introduced() {
        let findings = json!([
            {"description": "no-origin", "severity": "high"},
        ]);
        assert_eq!(
            descriptions(&filter_cherry_pick_findings(&findings)),
            vec!["no-origin"]
        );
    }

    #[test]
    fn non_array_yields_empty() {
        assert_eq!(filter_cherry_pick_findings(&json!({"x": 1})), json!([]));
    }

    #[test]
    fn context_round_trips_through_json() {
        let ctx = CherryPickContext {
            resolution_sha: "r".into(),
            original_sha: "o".into(),
            base_sha: "b".into(),
            resolution_subject: Some("rs".into()),
            original_subject: None,
            base_subject: None,
            original_diff: None,
        };
        let s = serde_json::to_string(&ctx).unwrap();
        assert_eq!(serde_json::from_str::<CherryPickContext>(&s).unwrap(), ctx);
    }
}
