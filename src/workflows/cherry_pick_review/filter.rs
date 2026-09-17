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
                    f.get("problem")
                        .or_else(|| f.get("description"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                );
                return false;
            }

            // Drop all findings pre-existing in the target base branch.
            if origin == "base_preexisting" {
                tracing::info!(
                    "Filtering out base-preexisting finding: {}",
                    f.get("problem")
                        .or_else(|| f.get("description"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                );
                return false;
            }

            // Drop findings pre-existing in the original patch unless critical.
            if origin == "original_patch_preexisting" && severity != "critical" {
                tracing::info!(
                    "Filtering out original-patch-preexisting ({}) finding: {}",
                    severity,
                    f.get("problem")
                        .or_else(|| f.get("description"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
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
}
