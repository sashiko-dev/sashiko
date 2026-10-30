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

//! Reporting pre-existing bugs that relate to the patch under review.
//!
//! See designs/DESIGN_RELATED_PREEXISTING_BUGS.md.

use crate::db::SeverityFloor;

/// Whether a patch review looks for related pre-existing bugs to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RelatedPreexistingMode {
    #[default]
    Disabled,
    Enabled {
        /// Bugs below this severity are never reported.
        min_severity: SeverityFloor,
    },
}

impl RelatedPreexistingMode {
    /// The mode a worker runs in given its `--related-preexisting-min-severity`.
    pub fn from_min_severity(min_severity: Option<SeverityFloor>) -> Self {
        match min_severity {
            Some(min_severity) => Self::Enabled { min_severity },
            None => Self::Disabled,
        }
    }

    pub fn min_severity(self) -> Option<SeverityFloor> {
        match self {
            Self::Enabled { min_severity } => Some(min_severity),
            Self::Disabled => None,
        }
    }
}

#[cfg(feature = "server")]
pub use gate::{GateInputs, GateSkipReason, evaluate_gate};

#[cfg(feature = "server")]
mod gate {
    use crate::db::SeverityFloor;
    use crate::email_policy::EmailPolicyConfig;
    use crate::project::ProjectId;

    /// Why a patchset does not get related pre-existing bugs reported.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GateSkipReason {
        /// Only the Linux review workflow implements the extra stages.
        UnsupportedProject,
        /// A mailing list the patchset went to turned the feature off.
        DisabledByPolicy,
        /// Requests for comments usually discuss a design, not final code.
        Rfc,
        /// No earlier posting is known, but the version tag says there were.
        NotFirstVersion,
        /// An earlier non-RFC posting of the series is known.
        FollowsNonRfcPosting,
    }

    /// What the gate needs to know about a patchset.
    #[derive(Debug, Clone)]
    pub struct GateInputs<'a> {
        pub project: ProjectId,
        /// The stored patchset subject.
        pub subject: &'a str,
        /// The pull request title, for patchsets that came from a forge.
        pub mr_title: Option<&'a str>,
        pub effective_version: u32,
        /// Titles of the earlier postings of the series (see
        /// `Database::get_patchset_predecessor_titles`).
        pub predecessor_titles: &'a [String],
        /// To and Cc addresses of the patchset.
        pub recipients: &'a [String],
    }

    /// Decides whether a patchset's review reports related pre-existing
    /// bugs, returning the severity floor to apply when it does.
    ///
    /// Only the first non-RFC posting of a series is eligible. RFCs never
    /// report, so a posting that follows nothing but RFCs is eligible
    /// whatever its version tag; one with no known predecessor must also be
    /// tagged as a first version, since a later tag means earlier rounds
    /// exist that were not linked.
    pub fn evaluate_gate(
        inputs: &GateInputs<'_>,
        policy: &EmailPolicyConfig,
    ) -> Result<SeverityFloor, GateSkipReason> {
        if inputs.project != ProjectId::Linux {
            return Err(GateSkipReason::UnsupportedProject);
        }
        let resolved = policy.related_preexisting_for(inputs.recipients.iter().map(String::as_str));
        if !resolved.enabled {
            return Err(GateSkipReason::DisabledByPolicy);
        }
        let title = inputs.mr_title.unwrap_or(inputs.subject);
        if crate::patch::is_rfc_subject(title) || crate::patch::is_rfc_subject(inputs.subject) {
            return Err(GateSkipReason::Rfc);
        }
        if inputs
            .predecessor_titles
            .iter()
            .any(|t| !crate::patch::is_rfc_subject(t))
        {
            return Err(GateSkipReason::FollowsNonRfcPosting);
        }
        if inputs.predecessor_titles.is_empty() && inputs.effective_version != 1 {
            return Err(GateSkipReason::NotFirstVersion);
        }
        Ok(resolved.min_severity)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn inputs<'a>(subject: &'a str, recipients: &'a [String]) -> GateInputs<'a> {
            GateInputs {
                project: ProjectId::Linux,
                subject,
                mr_title: None,
                effective_version: 1,
                predecessor_titles: &[],
                recipients,
            }
        }

        fn policy() -> EmailPolicyConfig {
            EmailPolicyConfig::parse_content(
                r#"
                ["netdev@vger.kernel.org".related_preexisting]
                enabled = false

                ["linux-mm@kvack.org".related_preexisting]
                min_severity = "critical"
                "#,
            )
            .unwrap()
        }

        #[test]
        fn test_first_version_is_eligible_with_the_resolved_floor() {
            let none: Vec<String> = Vec::new();
            assert_eq!(
                evaluate_gate(&inputs("[PATCH] mm: fix a leak", &none), &policy()),
                Ok(SeverityFloor::High)
            );
            let mm = vec!["linux-mm@kvack.org".to_string()];
            assert_eq!(
                evaluate_gate(&inputs("[PATCH v1] mm: fix a leak", &mm), &policy()),
                Ok(SeverityFloor::Critical)
            );
        }

        #[test]
        fn test_each_skip_reason() {
            let none: Vec<String> = Vec::new();
            let net = vec!["Netdev <netdev@vger.kernel.org>".to_string()];
            let policy = policy();

            let mut sashiko = inputs("[PATCH] fix", &none);
            sashiko.project = ProjectId::Sashiko;
            assert_eq!(
                evaluate_gate(&sashiko, &policy),
                Err(GateSkipReason::UnsupportedProject)
            );
            assert_eq!(
                evaluate_gate(&inputs("[PATCH] net: fix", &net), &policy),
                Err(GateSkipReason::DisabledByPolicy)
            );
            assert_eq!(
                evaluate_gate(&inputs("[RFC PATCH] mm: idea", &none), &policy),
                Err(GateSkipReason::Rfc)
            );
            let mut v2 = inputs("[PATCH v2] mm: fix", &none);
            v2.effective_version = 2;
            assert_eq!(
                evaluate_gate(&v2, &policy),
                Err(GateSkipReason::NotFirstVersion)
            );
            let earlier = vec!["[PATCH] mm: fix".to_string()];
            let mut resend = inputs("[PATCH v1] mm: fix", &none);
            resend.predecessor_titles = &earlier;
            assert_eq!(
                evaluate_gate(&resend, &policy),
                Err(GateSkipReason::FollowsNonRfcPosting)
            );
        }

        #[test]
        fn test_first_posting_after_rfcs_is_eligible() {
            let none: Vec<String> = Vec::new();
            let rfcs = vec![
                "[RFC v2] mm: idea".to_string(),
                "[RFC] mm: idea".to_string(),
            ];
            let mut after_rfc = inputs("[PATCH] mm: idea", &none);
            after_rfc.predecessor_titles = &rfcs;
            assert_eq!(
                evaluate_gate(&after_rfc, &policy()),
                Ok(SeverityFloor::High)
            );

            // The tag does not matter after RFCs: RFC v3 -> PATCH v4.
            let mut renumbered = inputs("[PATCH v4] mm: idea", &none);
            renumbered.effective_version = 4;
            renumbered.predecessor_titles = &rfcs;
            assert_eq!(
                evaluate_gate(&renumbered, &policy()),
                Ok(SeverityFloor::High)
            );

            // A non-RFC posting anywhere earlier in the chain disqualifies it.
            let mixed = vec![
                "[RFC v2] mm: idea".to_string(),
                "[PATCH] mm: idea".to_string(),
            ];
            let mut later = inputs("[PATCH v2] mm: idea", &none);
            later.effective_version = 2;
            later.predecessor_titles = &mixed;
            assert_eq!(
                evaluate_gate(&later, &policy()),
                Err(GateSkipReason::FollowsNonRfcPosting)
            );
        }

        #[test]
        fn test_forge_title_marks_an_rfc() {
            let none: Vec<String> = Vec::new();
            let mut pr = inputs("#513: RFC: rework the queue", &none);
            pr.mr_title = Some("RFC: rework the queue");
            assert_eq!(evaluate_gate(&pr, &policy()), Err(GateSkipReason::Rfc));

            let mut pr = inputs("#514: fix the queue", &none);
            pr.mr_title = Some("fix the queue");
            assert_eq!(evaluate_gate(&pr, &policy()), Ok(SeverityFloor::High));
        }
    }
}
