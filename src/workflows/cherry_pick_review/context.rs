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

pub type CherryPickReviewContext = CherryPickContext;

#[cfg(test)]
mod tests {
    use super::*;

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
