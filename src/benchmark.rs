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

use crate::ai::{AiProvider, LlmSession, SessionRunner, ValidationError, create_provider};
use crate::baseline::CommitId;
use crate::db::{AttributedSubsystem, Database, Finding, Severity};
use crate::events::Event;
use crate::fetcher::FetchAgent;
use crate::project::ProjectId;
use crate::reviewer::{Reviewer, WorkerSubprocessOverrides};
use crate::settings::Settings;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use flate2::read::GzDecoder;
use futures_util::stream::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

const DEFAULT_KERNEL_REPO_URL: &str =
    "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git";
const MAX_MBOX_ARCHIVE_DECOMPRESSED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_INLINE_REVIEW_PROMPT_CHARS: usize = 8_192;

// The two patterns below are constant literals using only basic RE2 syntax,
// so Regex::new cannot fail on them; test_judge_regexes_compile forces both
// to compile in the test suite to keep that invariant checked.
static JUDGE_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(DETECTED|PARTIALLY_DETECTED|MISSED)\b").expect("valid regex")
});
static FINDING_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[F(\d+)\]").expect("valid regex"));

fn default_part_index() -> u32 {
    1
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkExpectedBehavior {
    #[default]
    DetectBug,
    DetectWithAttribution,
    DetectRegression,
    SuppressFalsePositive,
}

impl BenchmarkExpectedBehavior {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DetectBug => "detect_bug",
            Self::DetectWithAttribution => "detect_with_attribution",
            Self::DetectRegression => "detect_regression",
            Self::SuppressFalsePositive => "suppress_false_positive",
        }
    }

    pub fn is_suppression(&self) -> bool {
        matches!(self, Self::SuppressFalsePositive)
    }

    pub fn expects_attribution(&self) -> bool {
        matches!(self, Self::DetectWithAttribution)
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "PascalCase")]
pub enum BenchmarkAncestorStatus {
    #[default]
    #[serde(alias = "reviewed")]
    Reviewed,
    #[serde(alias = "cancelled", alias = "canceled")]
    Cancelled,
}

impl BenchmarkAncestorStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reviewed => "Reviewed",
            Self::Cancelled => "Cancelled",
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct BenchmarkSeededFinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_message_id: Option<String>,
    #[serde(default = "default_part_index")]
    pub part_index: u32,
    #[serde(default)]
    pub severity: Severity,
    pub problem: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity_explanation: Option<String>,
    #[serde(default)]
    pub preexisting: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct BenchmarkAncestorMbox {
    pub version: u32,
    pub mbox_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_commit: Option<String>,
    #[serde(default)]
    pub patchset_status: BenchmarkAncestorStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prior_findings: Vec<BenchmarkSeededFinding>,
}

#[derive(Debug, Clone)]
pub struct BenchmarkOptions {
    pub file: PathBuf,
    pub repo: Option<String>,
    pub db: Option<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub analyze_only: bool,
    pub concurrency: Option<usize>,
    pub report_preexisting: bool,
    pub project: ProjectId,
    pub prompts: PathBuf,
    pub settings_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct BenchmarkEntry {
    #[serde(rename = "Commit")]
    pub commit: String,
    #[serde(rename = "Fixed-by", default, skip_serializing_if = "Option::is_none")]
    pub fixed_by: Option<String>,
    #[serde(rename = "subsystem", default, skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    #[serde(
        rename = "problem_description",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub problem_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbox_archive: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestor_mboxes: Vec<BenchmarkAncestorMbox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_mbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_commit: Option<String>,
    #[serde(default)]
    pub expected_behavior: BenchmarkExpectedBehavior,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_reviewer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_prior_version: Option<u32>,
    /// Every acceptable Message-ID (without angle brackets) of a message in
    /// `expected_prior_version` where `expected_reviewer` raised the issue.
    /// For a seeded Sashiko finding this is the Message-ID of the ancestor
    /// patch email the seeded review is attached to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_prior_message_ids: Vec<String>,
    /// Other reviewers ("Name <email>") who independently raised the same
    /// issue in `expected_prior_version`. Crediting any of them is correct;
    /// their messages belong in `expected_prior_message_ids` too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_co_reviewers: Vec<String>,
    /// Credit that is acceptable but not required: earlier messages that
    /// arguably raised the issue. An attribution matching one of these is
    /// scored `acceptable`, which is neither correct nor wrong.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptable_attributions: Vec<AcceptableAttribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lore_urls: Vec<String>,
}

/// One optional credit on a benchmark entry: a reviewer, the version in
/// which their messages arguably raised the issue, and those Message-IDs.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct AcceptableAttribution {
    pub reviewer: String,
    pub version: u32,
    pub message_ids: Vec<String>,
}

impl BenchmarkEntry {
    pub fn is_versioned(&self) -> bool {
        self.target_mbox.is_some() || !self.ancestor_mboxes.is_empty()
    }

    fn label(&self) -> &str {
        self.case_id.as_deref().unwrap_or(&self.commit)
    }

    /// Checks that the attribution ground truth is internally consistent.
    ///
    /// `detect_with_attribution` entries must name the reviewer, the earliest
    /// version, and the Message-IDs that the strict scorer accepts. Entries
    /// that expect a fresh detection (`detect_bug`) must not name a prior
    /// reviewer or version, because any attribution on them is scored wrong.
    pub fn validate_attribution_labels(&self) -> Result<()> {
        let label = self.label();
        let has_reviewer = self
            .expected_reviewer
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        for mid in &self.expected_prior_message_ids {
            let trimmed = mid.trim();
            if trimmed.is_empty() || trimmed.starts_with('<') || trimmed.ends_with('>') {
                bail!(
                    "case {label}: expected_prior_message_ids entry {mid:?} must be a non-empty Message-ID without angle brackets"
                );
            }
        }
        let unique: BTreeSet<&str> = self
            .expected_prior_message_ids
            .iter()
            .map(|s| s.trim())
            .collect();
        if unique.len() != self.expected_prior_message_ids.len() {
            bail!("case {label}: expected_prior_message_ids contains duplicates");
        }

        match self.expected_behavior {
            BenchmarkExpectedBehavior::DetectWithAttribution => {
                let Some(reviewer) = self.expected_reviewer.as_deref().filter(|_| has_reviewer)
                else {
                    bail!("case {label}: detect_with_attribution requires expected_reviewer");
                };
                if canonical_reviewer_identity(reviewer).is_none() {
                    bail!(
                        "case {label}: expected_reviewer {reviewer:?} must be \"Name <email>\" or \"Sashiko\""
                    );
                }
                let Some(version) = self.expected_prior_version else {
                    bail!("case {label}: detect_with_attribution requires expected_prior_version");
                };
                if self.expected_prior_message_ids.is_empty() {
                    bail!(
                        "case {label}: detect_with_attribution requires expected_prior_message_ids"
                    );
                }
                if let Some(bad) = self
                    .expected_co_reviewers
                    .iter()
                    .find(|r| canonical_reviewer_identity(r).is_none())
                {
                    bail!(
                        "case {label}: expected_co_reviewers entry {bad:?} must be \"Name <email>\" or \"Sashiko\""
                    );
                }
                if self.is_versioned() && !self.ancestor_mboxes.iter().any(|a| a.version == version)
                {
                    bail!(
                        "case {label}: expected_prior_version {version} is not one of the ancestor_mboxes versions"
                    );
                }
            }
            BenchmarkExpectedBehavior::DetectBug => {
                if has_reviewer
                    || self.expected_prior_version.is_some()
                    || !self.expected_prior_message_ids.is_empty()
                {
                    bail!(
                        "case {label}: detect_bug expects no prior credit; expected_reviewer, expected_prior_version and expected_prior_message_ids must be empty"
                    );
                }
            }
            BenchmarkExpectedBehavior::DetectRegression
            | BenchmarkExpectedBehavior::SuppressFalsePositive => {
                if !self.expected_prior_message_ids.is_empty() {
                    bail!(
                        "case {label}: expected_prior_message_ids is only valid for detect_with_attribution"
                    );
                }
            }
        }
        if !self.expected_behavior.expects_attribution() && !self.expected_co_reviewers.is_empty() {
            bail!("case {label}: expected_co_reviewers is only valid for detect_with_attribution");
        }
        self.validate_acceptable_attributions()
    }

    fn validate_acceptable_attributions(&self) -> Result<()> {
        let label = self.label();
        if self.acceptable_attributions.is_empty() {
            return Ok(());
        }
        if !matches!(
            self.expected_behavior,
            BenchmarkExpectedBehavior::DetectBug | BenchmarkExpectedBehavior::DetectWithAttribution
        ) {
            bail!(
                "case {label}: acceptable_attributions is only valid for detect_bug and detect_with_attribution"
            );
        }
        for acc in &self.acceptable_attributions {
            if canonical_reviewer_identity(&acc.reviewer).is_none() {
                bail!(
                    "case {label}: acceptable_attributions reviewer {:?} must be \"Name <email>\" or \"Sashiko\"",
                    acc.reviewer
                );
            }
            if acc.message_ids.is_empty() {
                bail!(
                    "case {label}: acceptable_attributions entry for {:?} needs message_ids",
                    acc.reviewer
                );
            }
            if let Some(bad) = acc.message_ids.iter().find(|m| {
                let t = m.trim();
                t.is_empty() || t.starts_with('<') || t.ends_with('>')
            }) {
                bail!(
                    "case {label}: acceptable_attributions message id {bad:?} must be non-empty and without angle brackets"
                );
            }
            if self.is_versioned()
                && !self
                    .ancestor_mboxes
                    .iter()
                    .any(|a| a.version == acc.version)
            {
                bail!(
                    "case {label}: acceptable_attributions version {} is not one of the ancestor_mboxes versions",
                    acc.version
                );
            }
        }
        Ok(())
    }
}

/// Validates the attribution ground truth of every entry in a suite.
pub fn validate_benchmark_entries(entries: &[BenchmarkEntry]) -> Result<()> {
    let errors: Vec<String> = entries
        .iter()
        .filter_map(|e| e.validate_attribution_labels().err())
        .map(|e| e.to_string())
        .collect();
    if !errors.is_empty() {
        bail!(
            "Benchmark ground truth is invalid ({} entries):\n{}",
            errors.len(),
            errors.join("\n")
        );
    }
    Ok(())
}

/// Canonical identity for an expected reviewer, compared exactly against the
/// `raised_by_identities` of a finding's structured prior attribution.
///
/// "Sashiko" maps to `sashiko`; "Name <email>" maps to the lowercased email.
fn canonical_reviewer_identity(reviewer: &str) -> Option<String> {
    let trimmed = reviewer.trim();
    if trimmed.eq_ignore_ascii_case("sashiko") {
        return Some("sashiko".to_string());
    }
    canonical_identity(trimmed)
}

/// Lowercased email of an identity given as "Name <email>", "<email>", or a
/// bare email; `sashiko` stays `sashiko`. Anything else has no identity.
fn canonical_identity(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("sashiko") {
        return Some("sashiko".to_string());
    }
    let email = match (trimmed.rfind('<'), trimmed.rfind('>')) {
        (Some(start), Some(end)) if end > start => trimmed[start + 1..end].trim(),
        (None, None) => trimmed,
        _ => return None,
    };
    let valid = !email.is_empty()
        && !email.chars().any(char::is_whitespace)
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        });
    valid.then(|| email.to_ascii_lowercase())
}

fn normalize_message_id(raw: &str) -> &str {
    raw.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
}

/// Outcome of strictly scoring the prior attribution of the matched finding.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttributionOutcome {
    /// Expected identity, earliest version and an accepted Message-ID.
    Correct,
    /// Attribution is present but not correct, or present on an entry that
    /// expects no prior credit.
    Wrong,
    /// Attribution was expected but the matched finding carries none (this
    /// includes entries whose bug was not matched at all).
    Missing,
    /// Attribution matches one of the entry's `acceptable_attributions`:
    /// defensible but not required credit, neither correct nor wrong.
    Acceptable,
}

/// The fields of a finding's `prior_attribution` that the strict scorer
/// reads. Every field defaults so that older review outputs still parse;
/// such outputs lack identities and evidence and therefore score as wrong.
#[derive(Debug, Deserialize, Default)]
struct ScoredPriorAttribution {
    #[serde(default)]
    originally_raised_in_version: Option<u32>,
    #[serde(default)]
    raised_by_identities: Vec<String>,
    #[serde(default)]
    evidence: Vec<ScoredAttributionEvidence>,
}

#[derive(Debug, Deserialize, Default)]
struct ScoredAttributionEvidence {
    #[serde(default)]
    message_id: Option<String>,
}

/// Strictly scores the structured prior attribution of the matched finding.
///
/// Returns `None` for suppression entries, and for entries that expect no
/// prior credit when the matched finding carries none. No free-text, name
/// token, or judge opinion is consulted.
pub(crate) fn score_attribution(
    entry: &BenchmarkEntry,
    matched_finding: Option<&BenchmarkFindingRecord>,
) -> Option<AttributionOutcome> {
    if entry.expected_behavior.is_suppression() {
        return None;
    }
    let attribution = matched_finding
        .and_then(|f| f.prior_attribution.as_ref())
        .filter(|v| !v.is_null());
    let Some(raw) = attribution else {
        return entry
            .expected_behavior
            .expects_attribution()
            .then_some(AttributionOutcome::Missing);
    };
    if entry.expected_behavior.expects_attribution() && attribution_matches_ground_truth(entry, raw)
    {
        return Some(AttributionOutcome::Correct);
    }
    if attribution_is_acceptable(entry, raw) {
        return Some(AttributionOutcome::Acceptable);
    }
    Some(AttributionOutcome::Wrong)
}

/// An attribution is acceptable when every credited identity is covered by
/// an `acceptable_attributions` item with the same version as
/// `originally_raised_in_version`, and, when evidence is present, at least
/// one evidence Message-ID belongs to one of those covering items.
fn attribution_is_acceptable(entry: &BenchmarkEntry, raw: &Value) -> bool {
    if entry.acceptable_attributions.is_empty() {
        return false;
    }
    let Ok(parsed) = serde_json::from_value::<ScoredPriorAttribution>(raw.clone()) else {
        return false;
    };
    let Some(version) = parsed.originally_raised_in_version else {
        return false;
    };
    if parsed.raised_by_identities.is_empty() {
        return false;
    }
    let mut covering: Vec<&AcceptableAttribution> = Vec::new();
    for raw_id in &parsed.raised_by_identities {
        let Some(id) = canonical_identity(raw_id) else {
            return false;
        };
        let matches: Vec<&AcceptableAttribution> = entry
            .acceptable_attributions
            .iter()
            .filter(|acc| {
                acc.version == version
                    && canonical_reviewer_identity(&acc.reviewer).as_deref() == Some(id.as_str())
            })
            .collect();
        if matches.is_empty() {
            return false;
        }
        covering.extend(matches);
    }
    if parsed.evidence.is_empty() {
        return true;
    }
    let accepted: BTreeSet<&str> = covering
        .iter()
        .flat_map(|acc| acc.message_ids.iter())
        .map(|m| normalize_message_id(m))
        .collect();
    parsed.evidence.iter().any(|ev| {
        ev.message_id
            .as_deref()
            .map(normalize_message_id)
            .is_some_and(|mid| accepted.contains(mid))
    })
}

fn attribution_matches_ground_truth(entry: &BenchmarkEntry, raw: &Value) -> bool {
    let Ok(parsed) = serde_json::from_value::<ScoredPriorAttribution>(raw.clone()) else {
        return false;
    };
    let accepted_identities: BTreeSet<String> = entry
        .expected_reviewer
        .iter()
        .chain(entry.expected_co_reviewers.iter())
        .filter_map(|r| canonical_reviewer_identity(r))
        .collect();
    if accepted_identities.is_empty() {
        return false;
    }
    let Some(expected_version) = entry.expected_prior_version else {
        return false;
    };
    let identity_ok = parsed
        .raised_by_identities
        .iter()
        .filter_map(|id| canonical_identity(id))
        .any(|id| accepted_identities.contains(&id));
    let version_ok = parsed.originally_raised_in_version == Some(expected_version);
    let accepted: BTreeSet<&str> = entry
        .expected_prior_message_ids
        .iter()
        .map(|s| normalize_message_id(s))
        .filter(|s| !s.is_empty())
        .collect();
    let evidence_ok = parsed.evidence.iter().any(|ev| {
        ev.message_id
            .as_deref()
            .map(normalize_message_id)
            .is_some_and(|mid| accepted.contains(mid))
    });
    identity_ok && version_ok && evidence_ok
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkRunPaths {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub db_path: PathBuf,
    pub worktree_dir: PathBuf,
    pub results_json_path: PathBuf,
    pub summary_md_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BenchmarkRunMetadata {
    pub run_id: String,
    pub started_at: String,
    pub finished_at: String,
    pub wall_clock_secs: u64,
    pub sashiko_version: String,
    pub sashiko_git_sha: String,
    pub sashiko_git_dirty: bool,
    pub prompts_git_hash: String,
    pub benchmark_file: String,
    pub total_entries: usize,
    pub unique_commits: usize,
    pub project: String,
    pub ai_provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub concurrency: usize,
    pub report_preexisting: bool,
    pub analyze_only: bool,
    pub database_path: String,
    pub results_json_path: String,
    pub summary_md_path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct MetricDistribution {
    pub total: u64,
    pub mean: f64,
    pub p50: u64,
    pub p90: u64,
    pub max: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct SeverityCounts {
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct SubsystemBenchmarkSummary {
    pub subsystem: String,
    pub total: usize,
    pub detected: usize,
    pub partially_detected: usize,
    pub missed: usize,
    pub detection_rate_pct: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct CategoryBenchmarkSummary {
    pub category: String,
    pub total: usize,
    pub passed: usize,
    pub detected: usize,
    pub partially_detected: usize,
    pub missed: usize,
    pub suppressed: usize,
    pub false_positives: usize,
    pub attribution_expected: usize,
    /// Entries whose matched finding carries a strictly correct attribution.
    pub attributed: usize,
    #[serde(default)]
    pub attribution_wrong: usize,
    #[serde(default)]
    pub attribution_missing: usize,
    /// Attributions matching an entry's optional `acceptable_attributions`.
    #[serde(default)]
    pub attribution_acceptable: usize,
    /// Entries without a completed review or judge verdict, scored as failures.
    #[serde(default)]
    pub failed: usize,
    /// Strict passes: exact detection (never partial), correct or absent
    /// attribution as the entry expects, or suppression.
    pub pass_rate_pct: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BenchmarkSummaryStats {
    pub total_entries: usize,
    pub evaluated_entries: usize,
    pub unique_commits: usize,
    pub reviewed_commits: usize,
    pub detected: usize,
    pub partially_detected: usize,
    pub missed: usize,
    pub not_reviewed: usize,
    /// Entries whose judge evaluation failed (`UNKNOWN`), scored as failures.
    #[serde(default)]
    pub evaluation_failed: usize,
    pub skipped: usize,
    /// Detection rates use every non-skipped detection entry as the
    /// denominator, so unreviewed and unevaluated entries count as failures.
    pub exact_detection_rate_pct: f64,
    pub total_detection_rate_pct: f64,
    #[serde(default)]
    pub suppression_evaluated_entries: usize,
    #[serde(default)]
    pub suppressed: usize,
    #[serde(default)]
    pub false_positives: usize,
    #[serde(default)]
    pub false_positive_suppression_rate_pct: f64,
    /// Suppression passes where the review reported no findings at all; such
    /// passes do not show that the false claim was examined and rejected.
    #[serde(default)]
    pub suppressed_with_no_findings: usize,
    #[serde(default)]
    pub attribution_expected_entries: usize,
    /// Strictly correct attributions on `detect_with_attribution` entries.
    #[serde(default)]
    pub attributed: usize,
    #[serde(default)]
    pub attribution_wrong: usize,
    #[serde(default)]
    pub attribution_missing: usize,
    /// Attributions matching an entry's optional `acceptable_attributions`.
    /// They are neither correct nor wrong and are left out of the
    /// attribution rate and precision denominators.
    #[serde(default)]
    pub attribution_acceptable: usize,
    /// Denominator of `attribution_rate_pct`: attribution-expected entries
    /// minus those whose attribution was scored acceptable.
    #[serde(default)]
    pub attribution_scored_entries: usize,
    /// Attributions on matched findings of entries that expect no prior
    /// credit (`detect_bug`, `detect_regression`); all of them are wrong.
    #[serde(default)]
    pub unexpected_attributions: usize,
    #[serde(default)]
    pub attribution_rate_pct: f64,
    /// Correct attributions over all attributions the reviewer emitted on
    /// matched findings.
    #[serde(default)]
    pub attribution_precision_pct: f64,
    pub missed_not_raised: usize,
    pub missed_wrongly_dismissed: usize,
    pub missed_preexisting_filtered: usize,
    pub missed_review_failed: usize,
    pub total_patch_findings: usize,
    pub total_preexisting_findings: usize,
    pub matched_findings: usize,
    pub unmatched_extra_findings: usize,
    pub avg_patch_findings_per_commit: f64,
    pub clean_target_only_commits: usize,
    pub findings_by_severity: SeverityCounts,
    pub total_concerns_raised: usize,
    pub total_concerns_dismissed: usize,
    pub concern_to_finding_conversion_pct: f64,
    pub detections_by_stage: BTreeMap<String, usize>,
    pub false_dismissals_by_stage: BTreeMap<String, usize>,
    pub by_subsystem: Vec<SubsystemBenchmarkSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_category: Vec<CategoryBenchmarkSummary>,
    pub tokens_in: MetricDistribution,
    pub tokens_out: MetricDistribution,
    pub tokens_cached: MetricDistribution,
    pub cache_hit_rate_pct: f64,
    pub turns: MetricDistribution,
    pub duration_secs: MetricDistribution,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MatchedFindingSummary {
    pub index: usize,
    pub severity: String,
    pub problem: String,
    pub preexisting: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub stages: Vec<String>,
    pub prompts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct BenchmarkFindingRecord {
    pub index: usize,
    pub severity: String,
    pub problem: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    pub preexisting: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_attribution: Option<Value>,
    pub stages: Vec<String>,
    pub prompts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct BenchmarkDismissedRecord {
    pub index: usize,
    pub stage: String,
    pub description: String,
    pub reasoning: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct BenchmarkStageMetric {
    pub name: String,
    pub status: String,
    pub turns: u32,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub concerns_count: usize,
    pub dismissed_count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct BenchmarkEntryResult {
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default)]
    pub expected_behavior: BenchmarkExpectedBehavior,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    pub problem_description: String,
    pub found: bool,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<AttributionOutcome>,
    #[serde(default)]
    pub suppressed_with_no_findings: bool,
    /// Versioned (mbox-chain) entries use the strict summary semantics;
    /// classic commit entries keep the original metric definitions.
    #[serde(default)]
    pub versioned: bool,
    pub explanation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_finding: Option<MatchedFindingSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub miss_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismissed_by_stage: Option<String>,
    pub findings_count: usize,
    pub patch_findings_count: usize,
    pub preexisting_findings_count: usize,
    pub extra_findings_count: usize,
    pub concerns_count: usize,
    pub dismissed_concerns_count: usize,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub turns: u32,
    pub duration_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patchset_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_id: Option<i64>,
    pub planned_stages: Vec<String>,
    pub selected_guides: Vec<String>,
    pub stages: Vec<BenchmarkStageMetric>,
    pub findings: Vec<BenchmarkFindingRecord>,
    pub dismissed_concerns: Vec<BenchmarkDismissedRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BenchmarkRunReport {
    pub metadata: BenchmarkRunMetadata,
    pub summary: BenchmarkSummaryStats,
    pub results: Vec<BenchmarkEntryResult>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CommitReviewData {
    pub subject: Option<String>,
    pub patchset_id: Option<i64>,
    pub patch_id: Option<i64>,
    pub review_id: Option<i64>,
    pub review_status: Option<String>,
    pub summary_text: String,
    pub inline_review: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub turns: u32,
    pub duration_secs: u64,
    pub concerns_count: usize,
    pub dismissed_concerns_count: usize,
    pub planned_stages: Vec<String>,
    pub selected_guides: Vec<String>,
    pub stages: Vec<BenchmarkStageMetric>,
    pub findings: Vec<BenchmarkFindingRecord>,
    pub dismissed_concerns: Vec<BenchmarkDismissedRecord>,
}

impl CommitReviewData {
    pub fn patch_findings_count(&self) -> usize {
        self.findings.iter().filter(|f| !f.preexisting).count()
    }

    pub fn preexisting_findings_count(&self) -> usize {
        self.findings.iter().filter(|f| f.preexisting).count()
    }

    pub fn is_completed_review(&self) -> bool {
        self.review_id.is_some()
            && !matches!(
                self.review_status.as_deref(),
                None | Some("" | "Pending" | "In Review" | "Failed" | "Skipped" | "Cancelled")
            )
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub(crate) struct BenchmarkJudgeVerdict {
    pub status: String,
    #[serde(default)]
    pub matched_finding_index: Option<usize>,
    #[serde(default)]
    pub miss_reason: Option<String>,
    #[serde(default)]
    pub dismissed_by_stage: Option<String>,
    pub explanation: String,
}

/// Resolves the Sashiko repository's short commit SHA and dirty status.
///
/// Prefers the live git checkout when running inside the Sashiko repository so
/// local uncommitted edits are reflected via the returned dirty flag, falling
/// back to the compile-time `GIT_HASH` embedded by `build.rs`.
pub fn resolve_sashiko_git_sha(cwd: &Path) -> (String, bool) {
    let compile_sha = env!("GIT_HASH");
    let fallback_short = compile_sha
        .chars()
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();

    let Ok(out) = crate::git_cmd::in_dir(cwd)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
    else {
        return (fallback_short, false);
    };
    if !out.status.success() {
        return (fallback_short, false);
    }

    let head_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if head_sha.is_empty() || !head_sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return (fallback_short, false);
    }

    let dirty = crate::git_cmd::in_dir(cwd)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .map(|s| s.status.success() && !String::from_utf8_lossy(&s.stdout).trim().is_empty())
        .unwrap_or(false);

    (head_sha, dirty)
}

/// Sanitizes a benchmark suite stem for safe filesystem path construction.
pub fn sanitize_suite_name(file_path: &Path) -> String {
    let raw = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("benchmark");
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "benchmark".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Extracts unique commit SHAs in their first-seen order.
pub fn deduplicate_benchmark_commits(entries: &[BenchmarkEntry]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut unique = Vec::new();
    for entry in entries {
        let trimmed = entry.commit.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
            unique.push(trimmed.to_string());
        }
    }
    unique
}

/// Resolves or creates the per-run directory and preserved SQLite database path.
pub fn prepare_benchmark_run_paths(
    base_dir: &Path,
    benchmark_file: &Path,
    timestamp_tag: &str,
    sashiko_sha: &str,
    explicit_db: Option<&Path>,
    explicit_output_dir: Option<&Path>,
    analyze_only: bool,
) -> Result<BenchmarkRunPaths> {
    let suite = sanitize_suite_name(benchmark_file);
    let runs_root = match explicit_output_dir {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        Some(dir) => base_dir.join(dir),
        None => base_dir.join("benchmarks").join("runs"),
    };

    if analyze_only && explicit_db.is_none() {
        if let Some(existing) = find_latest_run_for_suite(&runs_root, &suite)? {
            return Ok(existing);
        }
        bail!(
            "No existing benchmark database found for suite '{}' in {}; \
             run without --analyze-only first or pass --db <path>",
            suite,
            runs_root.display()
        );
    }

    let run_id = format!("{timestamp_tag}_{sashiko_sha}_{suite}");
    let run_dir = runs_root.join(&run_id);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("Failed to create benchmark run directory {:?}", run_dir))?;

    let db_filename = format!("sashiko-{timestamp_tag}-{sashiko_sha}.db");
    let db_path = match explicit_db {
        Some(path) => path.to_path_buf(),
        None => run_dir.join(db_filename),
    };

    let worktree_dir = run_dir.join("worktrees");
    let results_json_path = run_dir.join("results.json");
    let summary_md_path = run_dir.join("summary.md");

    Ok(BenchmarkRunPaths {
        run_id,
        run_dir,
        db_path,
        worktree_dir,
        results_json_path,
        summary_md_path,
    })
}

fn find_latest_run_for_suite(runs_root: &Path, suite: &str) -> Result<Option<BenchmarkRunPaths>> {
    if !runs_root.exists() {
        return Ok(None);
    }
    let suffix = format!("_{suite}");
    let mut matching_dirs: Vec<PathBuf> = std::fs::read_dir(runs_root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(&suffix))
        })
        .collect();
    matching_dirs.sort();

    for dir in matching_dirs.into_iter().rev() {
        let mut db_files: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|ext| ext.to_str()) == Some("db")
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("sashiko-"))
            })
            .collect();
        db_files.sort();
        if let Some(db_path) = db_files.pop() {
            let run_id = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("existing")
                .to_string();
            return Ok(Some(BenchmarkRunPaths {
                run_id,
                worktree_dir: dir.join("worktrees"),
                results_json_path: dir.join("results.json"),
                summary_md_path: dir.join("summary.md"),
                run_dir: dir,
                db_path,
            }));
        }
    }
    Ok(None)
}

/// Executes a full self-contained benchmark run (or `--analyze-only` evaluation)
/// and returns the structured [`BenchmarkRunReport`].
pub async fn run_benchmark(
    options: BenchmarkOptions,
    mut settings: Settings,
) -> Result<BenchmarkRunReport> {
    let started_wall = Instant::now();
    let now_utc = chrono::Utc::now();
    let started_at = now_utc.to_rfc3339();
    let timestamp_tag = now_utc.format("%Y%m%d-%H%M%S").to_string();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (sashiko_git_sha, sashiko_git_dirty) = resolve_sashiko_git_sha(&cwd);

    let file = File::open(&options.file)
        .with_context(|| format!("Failed to open benchmark file {}", options.file.display()))?;
    let reader = BufReader::new(file);
    let entries: Vec<BenchmarkEntry> = serde_json::from_reader(reader)
        .with_context(|| format!("Failed to parse benchmark file {}", options.file.display()))?;
    if entries.is_empty() {
        bail!(
            "Benchmark file {} contains no entries",
            options.file.display()
        );
    }
    validate_benchmark_entries(&entries)
        .with_context(|| format!("Invalid benchmark file {}", options.file.display()))?;

    let unique_commits = deduplicate_benchmark_commits(&entries);
    let paths = prepare_benchmark_run_paths(
        &cwd,
        &options.file,
        &timestamp_tag,
        &sashiko_git_sha,
        options.db.as_deref(),
        options.output_dir.as_deref(),
        options.analyze_only,
    )?;

    let auto_preexisting = options
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().contains("preexisting"));
    let report_preexisting =
        options.report_preexisting || settings.linux_bug.enabled || auto_preexisting;

    configure_benchmark_settings(&mut settings, &options, &paths, report_preexisting)?;

    info!(
        "Benchmark run {} using preserved database {}",
        paths.run_id,
        paths.db_path.display()
    );
    let db = Arc::new(
        Database::new(&settings.database)
            .await
            .with_context(|| format!("Failed to open database at {}", paths.db_path.display()))?,
    );
    db.migrate().await?;
    db.ensure_project_stamp(options.project).await?;

    if options.project.uses_maintainers() && crate::maintainers::get_global_maintainers().is_none()
    {
        let repo_path = PathBuf::from(&settings.git.repository_path);
        if let Ok(idx) = crate::maintainers::MaintainersIndex::from_top_of_trunk(&repo_path) {
            crate::maintainers::init_global_maintainers(Arc::new(idx));
        }
    }

    if !options.analyze_only {
        let classic_commits = deduplicate_classic_benchmark_commits(&entries);
        if !classic_commits.is_empty() {
            ingest_benchmark_commits(&db, &settings, options.repo.as_deref(), &classic_commits)
                .await?;
        }
        if entries.iter().any(BenchmarkEntry::is_versioned) {
            ingest_versioned_benchmark_entries(&db, &options.file, &entries).await?;
        }
        run_benchmark_reviews(
            &db,
            &settings,
            &options,
            &paths,
            report_preexisting,
            &unique_commits,
        )
        .await?;
    }

    info!("Evaluating {} benchmark entries...", entries.len());
    let commit_map = load_all_commit_review_data(&db, &unique_commits).await?;
    let ai_provider = create_provider(&settings).context("Failed to create AI provider")?;
    let ai_provider: Arc<dyn AiProvider> =
        Arc::new(crate::ai::backoff_provider::BackoffProvider::new(
            ai_provider,
            Arc::new(crate::ai::quota::QuotaManager::with_limits(
                settings.ai.max_rps,
                settings.ai.max_tpm,
            )),
            None,
        ));
    let results = evaluate_benchmark_entries(
        &entries,
        &commit_map,
        ai_provider,
        settings.review.concurrency,
    )
    .await;

    let finished_at = chrono::Utc::now().to_rfc3339();
    let wall_clock_secs = started_wall.elapsed().as_secs();

    let metadata = BenchmarkRunMetadata {
        run_id: paths.run_id.clone(),
        started_at,
        finished_at,
        wall_clock_secs,
        sashiko_version: env!("CARGO_PKG_VERSION").to_string(),
        sashiko_git_sha,
        sashiko_git_dirty,
        prompts_git_hash: crate::prompt_bundle::PROMPT_BUNDLE_REVISION.to_string(),
        benchmark_file: options.file.display().to_string(),
        total_entries: entries.len(),
        unique_commits: unique_commits.len(),
        project: options.project.as_str().to_string(),
        ai_provider: settings.ai.provider.clone(),
        model: settings.ai.model.clone(),
        effort: settings.effective_effort().map(|e| e.as_str().to_string()),
        concurrency: settings.review.concurrency,
        report_preexisting,
        analyze_only: options.analyze_only,
        database_path: paths.db_path.display().to_string(),
        results_json_path: paths.results_json_path.display().to_string(),
        summary_md_path: paths.summary_md_path.display().to_string(),
    };

    let summary = build_benchmark_summary(&entries, &results, &commit_map);
    let report = BenchmarkRunReport {
        metadata,
        summary,
        results,
    };

    write_benchmark_artifacts(&report, &paths)?;
    Ok(report)
}

fn configure_benchmark_settings(
    settings: &mut Settings,
    options: &BenchmarkOptions,
    paths: &BenchmarkRunPaths,
    report_preexisting: bool,
) -> Result<()> {
    settings.project.kind = Some(options.project);
    settings.database.url = paths.db_path.to_string_lossy().into_owned();
    settings.database.token = String::new();
    settings.review.worktree_dir = Some(paths.worktree_dir.to_string_lossy().into_owned());
    if let Some(concurrency) = options.concurrency {
        settings.review.concurrency = concurrency.max(1);
    }
    settings.linux_bug.enabled = report_preexisting;
    // Outbound notifications are never sent during a benchmark run.
    settings.smtp = None;
    settings.forge.enabled = false;

    if !options.analyze_only {
        if let Some(repo) = options.repo.as_deref()
            && Path::new(repo).exists()
        {
            settings.git.repository_path = repo.to_string();
        }
        if settings.git.repository_path.trim().is_empty() {
            bail!(
                "[git] repository_path must be set in Settings.toml or provided via --repo <path> \
                 to run benchmark reviews"
            );
        }
    }
    Ok(())
}

fn normalize_commit_or_msgid(raw: &str) -> (&str, String) {
    let trimmed = raw.trim().trim_matches(['<', '>']);
    (trimmed, format!("<{trimmed}>"))
}

fn deduplicate_classic_benchmark_commits(entries: &[BenchmarkEntry]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut unique = Vec::new();
    for entry in entries {
        if entry.is_versioned() {
            continue;
        }
        let trimmed = entry.commit.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
            unique.push(trimmed.to_string());
        }
    }
    unique
}

async fn commit_has_completed_review(db: &Database, commit: &str) -> Result<bool> {
    let (trimmed, bracketed) = normalize_commit_or_msgid(commit);
    let mut rows = db
        .conn
        .query(
            "SELECT 1
             FROM patches p
             JOIN patchsets ps ON ps.id = p.patchset_id
             JOIN reviews r ON r.patch_id = p.id
             WHERE p.message_id IN (?1, ?2)
               AND ps.status = 'Reviewed'
               AND r.status = 'Reviewed'
             LIMIT 1",
            libsql::params![trimmed.to_string(), bracketed],
        )
        .await?;
    if rows.next().await?.is_some() {
        return Ok(true);
    }
    if trimmed.len() < 40 && !trimmed.contains('@') {
        let root_msg_id = format!("{trimmed}@sashiko.local");
        let mut fallback_rows = db
            .conn
            .query(
                "SELECT 1
                 FROM patchsets ps
                 JOIN patches p ON p.patchset_id = ps.id
                 JOIN reviews r ON r.patch_id = p.id
                 WHERE ps.cover_letter_message_id = ?1
                   AND ps.status = 'Reviewed'
                   AND r.status = 'Reviewed'
                 LIMIT 1",
                libsql::params![root_msg_id],
            )
            .await?;
        if fallback_rows.next().await?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn ingest_benchmark_commits(
    db: &Database,
    settings: &Settings,
    repo_override: Option<&str>,
    unique_commits: &[String],
) -> Result<()> {
    info!(
        "--- Phase 1: Ingesting {} unique commits ---",
        unique_commits.len()
    );
    let mut to_fetch = Vec::new();
    for sha in unique_commits {
        if commit_has_completed_review(db, sha).await.unwrap_or(false) {
            info!(
                "Commit {} already has a completed review in benchmark DB, skipping",
                sha
            );
            continue;
        }
        to_fetch.push(sha.clone());
    }
    if to_fetch.is_empty() {
        return Ok(());
    }

    let repo_url = Some(repo_override.unwrap_or(DEFAULT_KERNEL_REPO_URL).to_string());
    let repo_path = PathBuf::from(&settings.git.repository_path);
    let (tx, mut rx) = mpsc::channel::<Event>(to_fetch.len().saturating_mul(2).max(32));
    let (mut fetch_agent, _fetch_req_tx) =
        FetchAgent::new(repo_path, tx, settings.forge.api_token.clone());

    fetch_agent
        .fetch_and_submit_batch(repo_url, to_fetch.clone())
        .await;
    drop(fetch_agent);

    let mut ingested = 0usize;
    let mut failures = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            Event::PatchSubmitted {
                article_id,
                message_id,
                subject,
                author,
                message,
                diff,
                base_commit,
                timestamp,
                index,
                total,
                ..
            } => {
                persist_benchmark_patch(
                    db,
                    &article_id,
                    &message_id,
                    &subject,
                    &author,
                    &message,
                    &diff,
                    base_commit.as_deref(),
                    timestamp,
                    index,
                    total,
                )
                .await?;
                ingested += 1;
            }
            Event::IngestionFailed {
                article_id, error, ..
            } => {
                error!("Failed to ingest {}: {}", article_id, error);
                failures.push((article_id, error));
            }
            _ => {}
        }
    }

    if !failures.is_empty() {
        bail!(
            "{} of {} benchmark commits failed to ingest. First failure: {} ({})",
            failures.len(),
            to_fetch.len(),
            failures[0].0,
            failures[0].1
        );
    }

    info!("Successfully ingested {} unique commits.", ingested);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn persist_benchmark_patch(
    db: &Database,
    article_id: &str,
    message_id: &str,
    subject: &str,
    author: &str,
    body: &str,
    diff: &str,
    base_commit: Option<&str>,
    timestamp: i64,
    index: u32,
    total: u32,
) -> Result<()> {
    let root_msg_id = format!("{article_id}@sashiko.local");
    let baseline_id = match base_commit.and_then(CommitId::parse) {
        Some(commit) => Some(
            db.create_baseline(None, None, Some(commit.as_str()))
                .await?,
        ),
        None => None,
    };

    let thread_id = db
        .ensure_thread_for_message(&root_msg_id, timestamp)
        .await?;
    let in_reply_to = if total == 1 {
        None
    } else {
        Some(root_msg_id.as_str())
    };
    db.create_message_with_references(
        message_id,
        thread_id,
        in_reply_to,
        author,
        subject,
        timestamp,
        body,
        "submitted",
        "",
        None,
        Some("git-fetch"),
        Some(&root_msg_id),
    )
    .await?;

    let sid = db.ensure_subsystem("from git", "git-import").await?;
    let maintainer_sections: Vec<AttributedSubsystem> = crate::maintainers::sections_for_diff(diff);

    let patchset_id = db
        .create_patchset(
            thread_id,
            Some(&root_msg_id),
            message_id,
            subject,
            author,
            timestamp,
            total,
            2,
            "submitted",
            "",
            None,
            index,
            baseline_id,
            true,
            None,
            None,
        )
        .await?
        .ok_or_else(|| anyhow!("Failed to create patchset for commit {}", message_id))?;

    let _ = db.add_subsystem_to_patchset(patchset_id, sid).await;
    let _ = db
        .add_patchset_maintainer_sections(patchset_id, &maintainer_sections)
        .await;

    let git_patch_id = crate::prerequisites::calculate_git_patch_id(diff)
        .await
        .ok()
        .flatten();
    let patch_id = db
        .create_patch_with_git_patch_id(
            patchset_id,
            message_id,
            index,
            diff,
            git_patch_id.as_deref(),
        )
        .await?;
    let _ = db.add_subsystem_to_patch(patch_id, sid).await;

    // Reset non-Reviewed prior attempts when re-running against an existing DB
    // so Reviewer picks up and re-reviews previously Failed/Cancelled commits.
    let mut rev_rows = db
        .conn
        .query(
            "SELECT 1 FROM reviews WHERE patch_id = ? AND status = 'Reviewed' LIMIT 1",
            libsql::params![patch_id],
        )
        .await?;
    let has_completed_review = rev_rows.next().await?.is_some();
    drop(rev_rows);

    db.execute(
        "DELETE FROM findings WHERE review_id IN (SELECT id FROM reviews WHERE patch_id = ? AND status != 'Reviewed')",
        libsql::params![patch_id],
    )
    .await?;
    db.execute(
        "DELETE FROM tool_usages WHERE review_id IN (SELECT id FROM reviews WHERE patch_id = ? AND status != 'Reviewed')",
        libsql::params![patch_id],
    )
    .await?;
    db.execute(
        "DELETE FROM reviews WHERE patch_id = ? AND status != 'Reviewed'",
        libsql::params![patch_id],
    )
    .await?;

    if has_completed_review {
        db.execute(
            "UPDATE patches SET status = 'Reviewed', apply_error = NULL WHERE id = ? AND COALESCE(status, '') != 'Reviewed'",
            libsql::params![patch_id],
        )
        .await?;
        db.execute(
            "UPDATE patchsets SET status = 'Reviewed', failed_reason = NULL WHERE id = ? AND COALESCE(status, '') != 'Reviewed'",
            libsql::params![patchset_id],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE patches SET status = 'Pending', apply_error = NULL WHERE id = ?",
            libsql::params![patch_id],
        )
        .await?;
        db.execute(
            "UPDATE patchsets SET status = 'Pending', failed_reason = NULL WHERE id = ?",
            libsql::params![patchset_id],
        )
        .await?;
    }
    Ok(())
}

struct AbortTaskOnDrop(tokio::task::AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn load_benchmark_skipped_patch_ids(db: &Database) -> Result<std::collections::HashSet<i64>> {
    let mut rows = db
        .conn
        .query(
            "SELECT p.id
             FROM patches p
             JOIN patchsets ps ON ps.id = p.patchset_id
             WHERE p.status = 'Skipped' AND ps.status = 'Pending'
             LIMIT 10000",
            (),
        )
        .await?;
    let mut ids = std::collections::HashSet::new();
    while let Some(row) = rows.next().await? {
        ids.insert(row.get::<i64>(0)?);
    }
    Ok(ids)
}

async fn run_benchmark_reviews(
    db: &Arc<Database>,
    settings: &Settings,
    options: &BenchmarkOptions,
    paths: &BenchmarkRunPaths,
    report_preexisting: bool,
    unique_commits: &[String],
) -> Result<()> {
    info!(
        "--- Phase 2: Reviewing {} unique commits (concurrency: {}) ---",
        unique_commits.len(),
        settings.review.concurrency
    );
    let skip_patch_ids = load_benchmark_skipped_patch_ids(db).await?;
    let worker_overrides = WorkerSubprocessOverrides {
        prompts: Some(options.prompts.clone()),
        settings_path: options.settings_path.clone(),
        report_preexisting,
        skip_patch_ids,
    };
    let reviewer = Arc::new(
        Reviewer::new(db.clone(), settings.clone())
            .await
            .with_worker_overrides(worker_overrides),
    );
    let reviewer_task = {
        let reviewer = reviewer.clone();
        tokio::spawn(async move {
            reviewer.start().await;
        })
    };
    let _abort_guard = AbortTaskOnDrop(reviewer_task.abort_handle());

    let mut last_logged = (usize::MAX, usize::MAX);
    loop {
        let (completed, pending) = count_commit_review_progress(db, unique_commits).await?;
        if completed >= unique_commits.len() {
            info!("All {} unique commits have been reviewed.", completed);
            break;
        }
        if (completed, pending) != last_logged {
            info!(
                "Review progress: {}/{} completed ({} in flight / pending)",
                completed,
                unique_commits.len(),
                pending
            );
            last_logged = (completed, pending);
        }
        sleep(Duration::from_secs(2)).await;
    }

    reviewer_task.abort();
    let _ = reviewer_task.await;

    if paths.worktree_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&paths.worktree_dir)
    {
        warn!(
            "Failed to remove temporary benchmark worktree dir {:?}: {}",
            paths.worktree_dir, e
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct MboxArchiveRecord {
    #[serde(alias = "path")]
    key: String,
    #[serde(alias = "content")]
    mbox: String,
}

fn resolve_benchmark_relative_path(suite_dir: &Path, rel: &str) -> Result<PathBuf> {
    let trimmed = rel.trim();
    if trimmed.is_empty() {
        bail!("Empty relative fixture path in benchmark entry");
    }
    let rel_path = Path::new(trimmed);
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "Fixture path '{}' must be a safe relative path under {}",
                    trimmed,
                    suite_dir.display()
                );
            }
        }
    }
    Ok(suite_dir.join(rel_path))
}

pub fn load_mbox_archive(archive_path: &Path) -> Result<HashMap<String, String>> {
    let file = File::open(archive_path)
        .with_context(|| format!("Failed to open mbox archive {}", archive_path.display()))?;
    let is_gzip = archive_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gz"));

    let mut raw_text = String::new();
    if is_gzip {
        let mut bounded = GzDecoder::new(file).take(MAX_MBOX_ARCHIVE_DECOMPRESSED_BYTES + 1);
        bounded.read_to_string(&mut raw_text).with_context(|| {
            format!(
                "Failed to decompress gzip mbox archive {}",
                archive_path.display()
            )
        })?;
    } else {
        let mut bounded = file.take(MAX_MBOX_ARCHIVE_DECOMPRESSED_BYTES + 1);
        bounded
            .read_to_string(&mut raw_text)
            .with_context(|| format!("Failed to read mbox archive {}", archive_path.display()))?;
    }
    if raw_text.len() as u64 > MAX_MBOX_ARCHIVE_DECOMPRESSED_BYTES {
        bail!(
            "Mbox archive {} exceeds {} byte safety cap",
            archive_path.display(),
            MAX_MBOX_ARCHIVE_DECOMPRESSED_BYTES
        );
    }

    let mut entries = HashMap::new();
    for (idx, line_res) in BufReader::new(raw_text.as_bytes()).lines().enumerate() {
        let line = line_res?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: MboxArchiveRecord = serde_json::from_str(trimmed).with_context(|| {
            format!(
                "Invalid JSONL record on line {} of {}",
                idx + 1,
                archive_path.display()
            )
        })?;
        let norm_key = record.key.trim().trim_start_matches("./").to_string();
        entries.insert(norm_key, record.mbox);
    }
    Ok(entries)
}

fn read_benchmark_mbox_source(
    suite_dir: &Path,
    archive_cache: &mut HashMap<PathBuf, Arc<HashMap<String, String>>>,
    mbox_archive: Option<&str>,
    mbox_key: &str,
) -> Result<String> {
    if let Some(arch_rel) = mbox_archive.map(str::trim).filter(|s| !s.is_empty()) {
        let arch_path = resolve_benchmark_relative_path(suite_dir, arch_rel)?;
        let archive = if let Some(cached) = archive_cache.get(&arch_path) {
            cached.clone()
        } else {
            let loaded = Arc::new(load_mbox_archive(&arch_path)?);
            archive_cache.insert(arch_path.clone(), loaded.clone());
            loaded
        };
        let norm_key = mbox_key.trim().trim_start_matches("./");
        return archive.get(norm_key).cloned().ok_or_else(|| {
            anyhow!(
                "Mbox key '{}' not found in archive {}",
                norm_key,
                arch_path.display()
            )
        });
    }

    let mbox_file = resolve_benchmark_relative_path(suite_dir, mbox_key)?;
    std::fs::read_to_string(&mbox_file)
        .with_context(|| format!("Failed to read benchmark mbox {}", mbox_file.display()))
}

pub async fn ingest_versioned_benchmark_entries(
    db: &Database,
    benchmark_file: &Path,
    entries: &[BenchmarkEntry],
) -> Result<()> {
    let suite_dir = benchmark_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut target_msg_ids = BTreeSet::new();
    for entry in entries {
        if entry.is_versioned() {
            let (trimmed, _) = normalize_commit_or_msgid(&entry.commit);
            if !trimmed.is_empty() {
                target_msg_ids.insert(trimmed.to_string());
            }
        }
    }

    let mut archive_cache: HashMap<PathBuf, Arc<HashMap<String, String>>> = HashMap::new();
    let mut seen_targets = BTreeSet::new();
    let mut ingested_count = 0usize;

    for entry in entries {
        if !entry.is_versioned() {
            continue;
        }
        let (target_key, _) = normalize_commit_or_msgid(&entry.commit);
        if target_key.is_empty() || !seen_targets.insert(target_key.to_string()) {
            continue;
        }
        if commit_has_completed_review(db, &entry.commit)
            .await
            .unwrap_or(false)
        {
            info!(
                "Versioned benchmark target {} already has a completed review, skipping ingestion",
                entry.commit
            );
            continue;
        }

        ingest_single_versioned_entry(db, suite_dir, &mut archive_cache, entry, &target_msg_ids)
            .await
            .with_context(|| {
                format!(
                    "Failed to ingest versioned benchmark entry '{}' ({})",
                    entry.case_id.as_deref().unwrap_or("unnamed"),
                    entry.commit
                )
            })?;
        ingested_count += 1;
    }

    if ingested_count > 0 {
        info!(
            "Successfully ingested {} versioned benchmark patchset chains.",
            ingested_count
        );
    }
    Ok(())
}

async fn ingest_single_versioned_entry(
    db: &Database,
    suite_dir: &Path,
    archive_cache: &mut HashMap<PathBuf, Arc<HashMap<String, String>>>,
    entry: &BenchmarkEntry,
    target_msg_ids: &BTreeSet<String>,
) -> Result<()> {
    let mut sorted_ancestors = entry.ancestor_mboxes.clone();
    sorted_ancestors.sort_by_key(|a| a.version);

    for ancestor in &sorted_ancestors {
        let raw_mbox = read_benchmark_mbox_source(
            suite_dir,
            archive_cache,
            entry.mbox_archive.as_deref(),
            &ancestor.mbox_path,
        )?;
        let ps_ids = ingest_raw_mbox_messages(
            db,
            &raw_mbox,
            ancestor.baseline_commit.as_deref(),
            "lore-benchmark",
        )
        .await
        .with_context(|| {
            format!(
                "Failed to ingest ancestor v{} ({})",
                ancestor.version, ancestor.mbox_path
            )
        })?;

        for ps_id in ps_ids {
            finalize_ancestor_patchset_state(db, ps_id, ancestor).await?;
        }
    }

    let target_mbox_key = entry
        .target_mbox
        .as_deref()
        .ok_or_else(|| anyhow!("Versioned benchmark entry missing target_mbox"))?;
    let target_raw = read_benchmark_mbox_source(
        suite_dir,
        archive_cache,
        entry.mbox_archive.as_deref(),
        target_mbox_key,
    )?;
    let target_ps_ids = ingest_raw_mbox_messages(
        db,
        &target_raw,
        entry.baseline_commit.as_deref(),
        "lore-benchmark",
    )
    .await
    .with_context(|| format!("Failed to ingest target_mbox '{}'", target_mbox_key))?;

    for ps_id in &target_ps_ids {
        db.reconcile_version_lineage(*ps_id).await?;
    }

    let Some((_target_patch_id, resolved_ps_id, _, _, _)) =
        lookup_benchmark_patch_row(db, &entry.commit).await?
    else {
        bail!(
            "Target patch message_id '{}' was not created from target_mbox '{}'",
            entry.commit,
            target_mbox_key
        );
    };

    prepare_target_patchset_review_state(db, resolved_ps_id, target_msg_ids).await?;
    Ok(())
}

async fn ingest_raw_mbox_messages(
    db: &Database,
    raw_mbox: &str,
    baseline_commit: Option<&str>,
    group: &str,
) -> Result<Vec<i64>> {
    let raw_messages = crate::ingestor::split_mbox(raw_mbox.as_bytes());
    if raw_messages.is_empty() {
        bail!("Mbox fixture contains zero messages");
    }

    let baseline_id = match baseline_commit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw_sha) => {
            let parsed = CommitId::parse(raw_sha).ok_or_else(|| {
                anyhow!("Invalid baseline_commit '{}' (expected hex SHA)", raw_sha)
            })?;
            Some(
                db.create_baseline(None, None, Some(parsed.as_str()))
                    .await?,
            )
        }
        None => None,
    };

    let mut parsed_items = Vec::with_capacity(raw_messages.len());
    for msg_raw in raw_messages {
        let parsed = tokio::task::spawn_blocking(move || crate::patch::parse_email(&msg_raw))
            .await
            .context("Mbox parser task panicked")??;
        parsed_items.push(parsed);
    }

    // Order cover letters first, then numbered patches in index order, then
    // discussion replies, preserving relative arrival order within each bucket.
    parsed_items.sort_by_key(|(meta, _)| {
        if meta.is_patch_or_cover && meta.index == 0 {
            (0u8, 0u32)
        } else if meta.is_patch_or_cover {
            (1u8, meta.index)
        } else {
            (2u8, 0u32)
        }
    });

    let sid = db.ensure_subsystem("lore-benchmark", group).await?;
    let mut created_patchset_ids = BTreeSet::new();

    for (mut metadata, patch_opt) in parsed_items {
        let thread_id = resolve_mbox_message_thread_id(db, &metadata).await?;
        let refs_hdr = if metadata.references.is_empty() {
            None
        } else {
            let cleaned: Vec<String> = metadata
                .references
                .iter()
                .map(|r| r.trim_matches(['<', '>']).to_string())
                .collect();
            Some(cleaned.join(" "))
        };

        db.create_message_with_references(
            &metadata.message_id,
            thread_id,
            metadata.in_reply_to.as_deref(),
            &metadata.author,
            &metadata.subject,
            metadata.date,
            &metadata.body,
            &metadata.to,
            &metadata.cc,
            None,
            Some(group),
            refs_hdr.as_deref(),
        )
        .await?;

        let git_patch_id = match patch_opt.as_ref() {
            Some(p) => crate::prerequisites::calculate_git_patch_id(&p.diff)
                .await
                .ok()
                .flatten(),
            None => None,
        };

        if !metadata.is_patch_or_cover
            && let Some(ref patch) = patch_opt
            && metadata.index == 1
            && metadata.total == 1
            && crate::patch::is_promotable_reply_patch_subject(&metadata.subject)
            && let Some(next_version) = db
                .find_in_thread_followup_version(
                    thread_id,
                    &metadata.message_id,
                    &metadata.author,
                    &metadata.subject,
                    git_patch_id.as_deref(),
                    &patch.diff,
                )
                .await?
        {
            metadata.is_patch_or_cover = true;
            metadata.version = Some(next_version);
            metadata.subject =
                crate::patch::promote_reply_patch_subject(&metadata.subject, next_version);
        }

        if !metadata.is_patch_or_cover {
            continue;
        }

        let cover_letter_id = if metadata.index == 0 || metadata.total == 1 {
            Some(metadata.message_id.clone())
        } else {
            let parent_is_cover = match metadata.in_reply_to.as_deref() {
                Some(parent) => db
                    .message_is_cover_letter_for(
                        parent,
                        &metadata.author,
                        metadata.total,
                        metadata.version,
                    )
                    .await
                    .unwrap_or(false),
                None => false,
            };
            if parent_is_cover {
                metadata.in_reply_to.clone()
            } else {
                Some(metadata.message_id.clone())
            }
        };

        let maintainer_sections: Vec<AttributedSubsystem> = patch_opt
            .as_ref()
            .map(|p| crate::maintainers::sections_for_diff(&p.diff))
            .unwrap_or_default();

        if let Some(patchset_id) = db
            .create_patchset(
                thread_id,
                cover_letter_id.as_deref(),
                &metadata.message_id,
                &metadata.subject,
                &metadata.author,
                metadata.date,
                metadata.total,
                2,
                &metadata.to,
                &metadata.cc,
                metadata.version,
                metadata.index,
                baseline_id,
                false,
                None,
                None,
            )
            .await?
        {
            created_patchset_ids.insert(patchset_id);
            if let Some(cid) = crate::patch::parse_b4_change_id(&metadata.body) {
                let _ = db.set_patchset_change_id_if_unset(patchset_id, &cid).await;
            }
            let _ = db.add_subsystem_to_patchset(patchset_id, sid).await;
            let _ = db
                .add_patchset_maintainer_sections(patchset_id, &maintainer_sections)
                .await;

            if let Some(patch) = patch_opt {
                let patch_id = db
                    .create_patch_with_git_patch_id(
                        patchset_id,
                        &patch.message_id,
                        patch.part_index,
                        &patch.diff,
                        git_patch_id.as_deref(),
                    )
                    .await?;
                let _ = db.add_subsystem_to_patch(patch_id, sid).await;
            }
        }
    }

    Ok(created_patchset_ids.into_iter().collect())
}

async fn resolve_mbox_message_thread_id(
    db: &Database,
    metadata: &crate::patch::PatchsetMetadata,
) -> Result<i64> {
    if let Some(ref reply_to) = metadata.in_reply_to {
        if let Some(tid) = db.get_thread_id_for_message(reply_to).await? {
            return Ok(tid);
        }
        for r in metadata.references.iter().rev() {
            let cleaned = r.trim_matches(['<', '>']);
            if !cleaned.is_empty()
                && let Some(tid) = db.get_thread_id_for_message(cleaned).await?
            {
                return Ok(tid);
            }
        }
        return db.ensure_thread_for_message(reply_to, metadata.date).await;
    }
    db.ensure_thread_for_message(&metadata.message_id, metadata.date)
        .await
}

async fn finalize_ancestor_patchset_state(
    db: &Database,
    patchset_id: i64,
    ancestor: &BenchmarkAncestorMbox,
) -> Result<()> {
    db.reconcile_version_lineage(patchset_id).await?;

    match ancestor.patchset_status {
        BenchmarkAncestorStatus::Reviewed => {
            db.execute(
                "UPDATE patchsets SET status = 'Reviewed', failed_reason = NULL WHERE id = ?",
                libsql::params![patchset_id],
            )
            .await?;
            db.execute(
                "UPDATE patches SET status = 'Reviewed', apply_error = NULL WHERE patchset_id = ?",
                libsql::params![patchset_id],
            )
            .await?;

            let mut rows = db
                .conn
                .query(
                    "SELECT id, COALESCE(part_index, 1), message_id
                     FROM patches
                     WHERE patchset_id = ?
                     ORDER BY part_index ASC, id ASC
                     LIMIT 256",
                    libsql::params![patchset_id],
                )
                .await?;
            let mut patches = Vec::new();
            while let Some(row) = rows.next().await? {
                patches.push((
                    row.get::<i64>(0)?,
                    row.get::<i64>(1)? as u32,
                    row.get::<String>(2)?,
                ));
            }
            drop(rows);

            for (patch_id, part_index, patch_msg_id) in patches {
                let mut existing_rev = db
                    .conn
                    .query(
                        "SELECT id, provider FROM reviews WHERE patch_id = ? AND status = 'Reviewed' LIMIT 1",
                        libsql::params![patch_id],
                    )
                    .await?;
                let existing_rev_info = existing_rev
                    .next()
                    .await?
                    .map(|rrow| {
                        Ok::<_, libsql::Error>((
                            rrow.get::<i64>(0)?,
                            rrow.get::<String>(1).unwrap_or_default(),
                        ))
                    })
                    .transpose()?;
                drop(existing_rev);

                let (norm_patch_msg, _) = normalize_commit_or_msgid(&patch_msg_id);
                let matching_seeded: Vec<&BenchmarkSeededFinding> = ancestor
                    .prior_findings
                    .iter()
                    .filter(|seeded| match seeded.patch_message_id.as_deref() {
                        Some(seed_mid) => {
                            let (norm_seed, _) = normalize_commit_or_msgid(seed_mid);
                            norm_seed == norm_patch_msg
                        }
                        None => seeded.part_index == part_index,
                    })
                    .collect();

                let review_id = if let Some((rev_id, provider)) = existing_rev_info {
                    if provider != "benchmark-seed" || matching_seeded.is_empty() {
                        continue;
                    }
                    db.execute(
                        "DELETE FROM findings WHERE review_id = ?",
                        libsql::params![rev_id],
                    )
                    .await?;
                    rev_id
                } else {
                    let rev_id = db
                        .create_review(
                            patchset_id,
                            Some(patch_id),
                            "benchmark-seed",
                            "seed",
                            None,
                            None,
                        )
                        .await?;
                    db.complete_review(
                        rev_id,
                        "Reviewed",
                        "Benchmark placeholder: no Sashiko review was run on this version.",
                        Some("Benchmark placeholder: no Sashiko review was run on this version."),
                        None,
                        None,
                        None,
                    )
                    .await?;
                    rev_id
                };

                for seeded in matching_seeded {
                    db.create_finding(Finding {
                        review_id,
                        severity: if seeded.severity == Severity::Unknown {
                            Severity::Medium
                        } else {
                            seeded.severity
                        },
                        severity_explanation: seeded.severity_explanation.clone(),
                        problem: seeded.problem.clone(),
                        preexisting: Some(seeded.preexisting),
                        locations: None,
                    })
                    .await?;
                }
            }
        }
        BenchmarkAncestorStatus::Cancelled => {
            db.execute(
                "UPDATE patchsets SET status = 'Cancelled' WHERE id = ?",
                libsql::params![patchset_id],
            )
            .await?;
            db.execute(
                "UPDATE patches SET status = 'Cancelled' WHERE patchset_id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        }
    }

    Ok(())
}

async fn prepare_target_patchset_review_state(
    db: &Database,
    patchset_id: i64,
    target_msg_ids: &BTreeSet<String>,
) -> Result<()> {
    let mut rows = db
        .conn
        .query(
            "SELECT id, message_id FROM patches WHERE patchset_id = ? ORDER BY part_index ASC LIMIT 256",
            libsql::params![patchset_id],
        )
        .await?;
    let mut patches = Vec::new();
    while let Some(row) = rows.next().await? {
        patches.push((row.get::<i64>(0)?, row.get::<String>(1)?));
    }
    drop(rows);

    let mut any_pending = false;
    for (patch_id, msg_id) in patches {
        let (norm_mid, _) = normalize_commit_or_msgid(&msg_id);
        if !target_msg_ids.contains(norm_mid) {
            db.execute(
                "UPDATE patches SET status = 'Skipped', apply_error = NULL WHERE id = ? AND COALESCE(status, '') != 'Reviewed'",
                libsql::params![patch_id],
            )
            .await?;
            continue;
        }

        let mut rev_rows = db
            .conn
            .query(
                "SELECT 1 FROM reviews WHERE patch_id = ? AND status = 'Reviewed' LIMIT 1",
                libsql::params![patch_id],
            )
            .await?;
        let has_completed_review = rev_rows.next().await?.is_some();
        drop(rev_rows);

        db.execute(
            "DELETE FROM findings WHERE review_id IN (SELECT id FROM reviews WHERE patch_id = ? AND status != 'Reviewed')",
            libsql::params![patch_id],
        )
        .await?;
        db.execute(
            "DELETE FROM tool_usages WHERE review_id IN (SELECT id FROM reviews WHERE patch_id = ? AND status != 'Reviewed')",
            libsql::params![patch_id],
        )
        .await?;
        db.execute(
            "DELETE FROM reviews WHERE patch_id = ? AND status != 'Reviewed'",
            libsql::params![patch_id],
        )
        .await?;

        if has_completed_review {
            db.execute(
                "UPDATE patches SET status = 'Reviewed', apply_error = NULL WHERE id = ? AND COALESCE(status, '') != 'Reviewed'",
                libsql::params![patch_id],
            )
            .await?;
        } else {
            any_pending = true;
            db.execute(
                "UPDATE patches SET status = 'Pending', apply_error = NULL WHERE id = ?",
                libsql::params![patch_id],
            )
            .await?;
        }
    }

    if any_pending {
        db.execute(
            "UPDATE patchsets SET status = 'Pending', failed_reason = NULL WHERE id = ?",
            libsql::params![patchset_id],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE patchsets SET status = 'Reviewed', failed_reason = NULL WHERE id = ? AND COALESCE(status, '') != 'Reviewed'",
            libsql::params![patchset_id],
        )
        .await?;
    }
    Ok(())
}

async fn lookup_benchmark_patch_row(
    db: &Database,
    commit: &str,
) -> Result<Option<(i64, i64, Option<String>, Option<String>, Option<String>)>> {
    let (trimmed, bracketed) = normalize_commit_or_msgid(commit);
    let mut rows = db
        .conn
        .query(
            "SELECT p.id, p.patchset_id, p.status, ps.status, m.subject
             FROM patches p
             JOIN patchsets ps ON ps.id = p.patchset_id
             LEFT JOIN messages m ON m.message_id = p.message_id
             WHERE p.message_id IN (?1, ?2)
             ORDER BY p.id DESC LIMIT 1",
            libsql::params![trimmed.to_string(), bracketed],
        )
        .await?;
    if let Some(row) = rows.next().await? {
        return Ok(Some((
            row.get(0).unwrap_or_default(),
            row.get(1).unwrap_or_default(),
            row.get(2).ok(),
            row.get(3).ok(),
            row.get(4).ok(),
        )));
    }
    drop(rows);

    if trimmed.len() < 40 && !trimmed.contains('@') {
        let root_msg_id = format!("{trimmed}@sashiko.local");
        let mut fallback_rows = db
            .conn
            .query(
                "SELECT p.id, p.patchset_id, p.status, ps.status, m.subject
                 FROM patchsets ps
                 JOIN patches p ON p.patchset_id = ps.id
                 LEFT JOIN messages m ON m.message_id = p.message_id
                 WHERE ps.cover_letter_message_id = ?1
                 ORDER BY p.id DESC LIMIT 1",
                libsql::params![root_msg_id],
            )
            .await?;
        if let Some(row) = fallback_rows.next().await? {
            return Ok(Some((
                row.get(0).unwrap_or_default(),
                row.get(1).unwrap_or_default(),
                row.get(2).ok(),
                row.get(3).ok(),
                row.get(4).ok(),
            )));
        }
    }

    Ok(None)
}

async fn count_commit_review_progress(
    db: &Database,
    unique_commits: &[String],
) -> Result<(usize, usize)> {
    let mut completed = 0usize;
    let mut pending = 0usize;

    for commit in unique_commits {
        let Some((patch_id, _patchset_id, patch_status_opt, patchset_status_opt, _subject)) =
            lookup_benchmark_patch_row(db, commit).await?
        else {
            pending += 1;
            continue;
        };
        let patch_status = patch_status_opt.unwrap_or_default();
        let patchset_status = patchset_status_opt.unwrap_or_default();

        // A patchset is only complete once Reviewer::review_patchset_task has
        // finished all retries, cleaned up its worktree, and updated
        // patchsets.status out of Pending / In Review / Fetching.
        let patchset_terminal = !matches!(
            patchset_status.as_str(),
            "" | "Pending" | "In Review" | "Fetching"
        );
        if !patchset_terminal {
            pending += 1;
            continue;
        }

        let mut rev_rows = db
            .conn
            .query(
                "SELECT status FROM reviews WHERE patch_id = ?
                 ORDER BY CASE WHEN status = 'Reviewed' THEN 0 ELSE 1 END ASC, id DESC LIMIT 1",
                libsql::params![patch_id],
            )
            .await?;

        if let Some(rev_row) = rev_rows.next().await? {
            let rev_status: String = rev_row.get(0).unwrap_or_default();
            if rev_status == "Pending" || rev_status == "In Review" {
                pending += 1;
            } else {
                completed += 1;
            }
        } else if matches!(
            patch_status.as_str(),
            "Skipped" | "Failed" | "Failed To Apply" | "Cancelled"
        ) || matches!(
            patchset_status.as_str(),
            "Skipped" | "Failed" | "Failed To Apply" | "Cancelled" | "Reviewed" | "Incomplete"
        ) {
            completed += 1;
        } else {
            pending += 1;
        }
    }

    Ok((completed, pending))
}

async fn load_all_commit_review_data(
    db: &Database,
    unique_commits: &[String],
) -> Result<HashMap<String, CommitReviewData>> {
    let mut map = HashMap::with_capacity(unique_commits.len());
    for commit in unique_commits {
        let data = load_commit_review_data(db, commit).await?;
        map.insert(commit.clone(), data);
    }
    Ok(map)
}

pub(crate) async fn load_commit_review_data(
    db: &Database,
    commit: &str,
) -> Result<CommitReviewData> {
    let mut data = CommitReviewData::default();

    let Some((patch_id, patchset_id, patch_status, patchset_status, subject)) =
        lookup_benchmark_patch_row(db, commit).await?
    else {
        return Ok(data);
    };

    data.patch_id = Some(patch_id);
    data.patchset_id = Some(patchset_id);
    data.subject = subject;
    data.review_status = match patch_status.as_deref() {
        Some(s) if !matches!(s, "" | "Pending" | "In Review") => patch_status,
        _ => patchset_status.or(patch_status),
    };

    let mut rev_rows = db
        .conn
        .query(
            "SELECT id, status, summary, result_description, interaction_id, created_at, inline_review
             FROM reviews
             WHERE patch_id = ?
             ORDER BY CASE WHEN status = 'Reviewed' THEN 0 ELSE 1 END ASC, id DESC LIMIT 1",
            libsql::params![patch_id],
        )
        .await?;

    let Some(rev_row) = rev_rows.next().await? else {
        return Ok(data);
    };

    let review_id: i64 = rev_row.get(0).unwrap_or_default();
    let review_status: String = rev_row.get(1).unwrap_or_default();
    let summary: Option<String> = rev_row.get(2).ok();
    let result_desc: Option<String> = rev_row.get(3).ok();
    let interaction_id: Option<String> = rev_row.get(4).ok();
    let review_created_at: Option<i64> = rev_row.get(5).ok();
    if let Ok(Some(inline_str)) = crate::compression::get_compressed_string_opt(&rev_row, 6) {
        data.inline_review = inline_str.trim().to_string();
    }

    data.review_id = Some(review_id);
    if !matches!(review_status.as_str(), "" | "Pending" | "In Review")
        || data.review_status.is_none()
    {
        data.review_status = Some(review_status);
    }
    data.summary_text = format!(
        "{}\n{}",
        summary.unwrap_or_default(),
        result_desc.unwrap_or_default()
    )
    .trim()
    .to_string();

    let mut parsed_output: Option<Value> = None;
    if let Some(iid) = interaction_id {
        let mut int_rows = db
            .conn
            .query(
                "SELECT tokens_in, tokens_out, tokens_cached, created_at, output_raw
                 FROM ai_interactions
                 WHERE id = ?",
                libsql::params![iid],
            )
            .await?;
        if let Some(int_row) = int_rows.next().await? {
            data.tokens_in = int_row.get::<i64>(0).unwrap_or(0).max(0) as u32;
            data.tokens_out = int_row.get::<i64>(1).unwrap_or(0).max(0) as u32;
            data.tokens_cached = int_row.get::<i64>(2).unwrap_or(0).max(0) as u32;
            let int_created_at = int_row.get::<i64>(3).unwrap_or(0);
            if let Some(start_time) = review_created_at
                && int_created_at >= start_time
            {
                data.duration_secs = (int_created_at - start_time) as u64;
            }
            if let Ok(Some(raw_str)) = crate::compression::get_compressed_string_opt(&int_row, 4)
                && let Ok(val) = serde_json::from_str::<Value>(&raw_str)
            {
                parsed_output = Some(val);
            }
        }
    }

    let mut tool_rows = db
        .conn
        .query(
            "SELECT COUNT(*) FROM tool_usages WHERE review_id = ?",
            libsql::params![review_id],
        )
        .await?;
    if let Some(tool_row) = tool_rows.next().await? {
        let tool_count = tool_row.get::<i64>(0).unwrap_or(0).max(0) as u32;
        data.turns = 1 + tool_count;
    } else {
        data.turns = 1;
    }

    if let Some(ref output) = parsed_output {
        populate_findings_and_stages_from_output(&mut data, output);
    }

    if data.findings.is_empty() {
        load_findings_table_fallback(db, review_id, &mut data.findings).await?;
    }

    // Always merge pre-existing bugs linked to this review from the bugs table,
    // even when patch findings were already loaded from output_raw or findings.
    load_bugs_for_review(db, review_id, &mut data.findings).await?;

    if let Some(ref output) = parsed_output {
        populate_dismissed_from_output(&mut data, output);
    }

    if data.turns <= 1 && !data.stages.is_empty() {
        let stage_turns: u32 = data.stages.iter().map(|s| s.turns).sum();
        if stage_turns > 0 {
            data.turns = stage_turns;
        }
    }

    Ok(data)
}

fn populate_findings_and_stages_from_output(data: &mut CommitReviewData, output: &Value) {
    if data.inline_review.is_empty()
        && let Some(inline_str) = output.get("inline_review").and_then(Value::as_str)
    {
        data.inline_review = inline_str.trim().to_string();
    }
    if let Some(c) = output.get("concerns_count").and_then(Value::as_u64) {
        data.concerns_count = c as usize;
    }
    if let Some(d) = output
        .get("dismissed_concerns_count")
        .and_then(Value::as_u64)
    {
        data.dismissed_concerns_count = d as usize;
    }

    if let Some(review_map) = output.get("review_map") {
        if let Some(guides) = review_map
            .get("prompts")
            .and_then(|p| p.get("pre_screen"))
            .and_then(Value::as_array)
        {
            data.selected_guides = guides
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }

        if let Some(stages) = review_map.get("stages").and_then(Value::as_array) {
            for s in stages {
                let name = s
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let skipped = s.get("skipped").and_then(Value::as_bool).unwrap_or(false);
                let turns = s.get("turns").and_then(Value::as_u64).unwrap_or(0) as u32;
                let tokens_in = s.get("tokens_in").and_then(Value::as_u64).unwrap_or(0) as u32;
                let tokens_out = s.get("tokens_out").and_then(Value::as_u64).unwrap_or(0) as u32;
                let tokens_cached =
                    s.get("tokens_cached").and_then(Value::as_u64).unwrap_or(0) as u32;
                let concerns_count =
                    s.get("concerns_count").and_then(Value::as_u64).unwrap_or(0) as usize;
                let dismissed_count = s
                    .get("dismissed_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;

                if !skipped
                    && !matches!(
                        name.as_str(),
                        "pre-screen"
                            | "planning"
                            | "deduplication"
                            | "verification"
                            | "report"
                            | "summary"
                    )
                    && !name.starts_with("verification-")
                    && !name.starts_with("post-verification")
                {
                    data.planned_stages.push(name.clone());
                }

                data.stages.push(BenchmarkStageMetric {
                    name,
                    status: if skipped {
                        "skipped".to_string()
                    } else {
                        "executed".to_string()
                    },
                    turns,
                    tokens_in,
                    tokens_out,
                    tokens_cached,
                    concerns_count,
                    dismissed_count,
                });
            }
        }
    }

    if let Some(findings_arr) = output.get("findings").and_then(Value::as_array) {
        for f in findings_arr {
            let problem = f
                .get("problem")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if problem.is_empty() {
                continue;
            }
            let severity = f
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("Low")
                .to_string();
            let explanation = f
                .get("severity_explanation")
                .or_else(|| f.get("reasoning"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let preexisting = f
                .get("preexisting")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let origin = f
                .get("origin")
                .or_else(|| f.get("post_verification_stage"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let prior_attribution = f.get("prior_attribution").filter(|v| !v.is_null()).cloned();
            let stages = extract_str_vec(f.get("stages"));
            let prompts = extract_str_vec(f.get("prompts"));

            let next_idx = data.findings.len() + 1;
            data.findings.push(BenchmarkFindingRecord {
                index: next_idx,
                severity,
                problem,
                explanation,
                preexisting,
                origin,
                prior_attribution,
                stages,
                prompts,
            });
        }
    }
}

fn populate_dismissed_from_output(data: &mut CommitReviewData, output: &Value) {
    if let Some(dismissed_arr) = output.get("dismissed_concerns").and_then(Value::as_array) {
        for d in dismissed_arr {
            push_dismissed_item(&mut data.dismissed_concerns, d, None, false);
        }
    }

    if let Some(concerns_arr) = output.get("concerns").and_then(Value::as_array) {
        let finding_problems: BTreeSet<String> = data
            .findings
            .iter()
            .map(|f| f.problem.trim().to_ascii_lowercase())
            .collect();
        for c in concerns_arr {
            let desc = c
                .get("description")
                .or_else(|| c.get("problem"))
                .or_else(|| c.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !desc.is_empty() && finding_problems.contains(&desc) {
                continue;
            }
            push_dismissed_item(
                &mut data.dismissed_concerns,
                c,
                Some("preexisting-filtered"),
                true,
            );
        }
    }

    if data.dismissed_concerns_count == 0 {
        data.dismissed_concerns_count = data.dismissed_concerns.len();
    }
}

fn push_dismissed_item(
    out: &mut Vec<BenchmarkDismissedRecord>,
    item: &Value,
    default_stage: Option<&str>,
    force_default_stage: bool,
) {
    let description = item
        .get("description")
        .or_else(|| item.get("problem"))
        .or_else(|| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let reasoning = item
        .get("reasoning")
        .or_else(|| item.get("dismissal_reasoning"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if description.is_empty() && reasoning.is_empty() {
        return;
    }

    let stage = if force_default_stage {
        default_stage.unwrap_or("verification").to_string()
    } else {
        item.get("post_verification_stage")
            .or_else(|| item.get("origin"))
            .or_else(|| item.get("stage"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| extract_str_vec(item.get("stages")).into_iter().next())
            .or_else(|| default_stage.map(str::to_string))
            .unwrap_or_else(|| "verification".to_string())
    };

    let next_index = out.len() + 1;
    out.push(BenchmarkDismissedRecord {
        index: next_index,
        stage,
        description,
        reasoning,
    });
}

fn extract_str_vec(val: Option<&Value>) -> Vec<String> {
    val.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn load_findings_table_fallback(
    db: &Database,
    review_id: i64,
    findings: &mut Vec<BenchmarkFindingRecord>,
) -> Result<()> {
    let mut rows = db
        .conn
        .query(
            "SELECT problem, severity, severity_explanation, preexisting
             FROM findings
             WHERE review_id = ?
             ORDER BY id ASC",
            libsql::params![review_id],
        )
        .await?;

    let mut seen_problems: BTreeSet<String> = findings
        .iter()
        .map(|f| f.problem.trim().to_ascii_lowercase())
        .collect();
    while let Some(row) = rows.next().await? {
        let problem: String = row.get(0).unwrap_or_default();
        if problem.trim().is_empty() || !seen_problems.insert(problem.trim().to_ascii_lowercase()) {
            continue;
        }
        let sev_int: i32 = row.get(1).unwrap_or(1);
        let severity = match sev_int {
            4 => Severity::Critical,
            3 => Severity::High,
            2 => Severity::Medium,
            _ => Severity::Low,
        }
        .as_str()
        .to_string();
        let explanation: Option<String> = row.get(2).ok();
        let preexisting = row.get::<Option<bool>>(3).ok().flatten().unwrap_or(false);
        let next_idx = findings.len() + 1;
        findings.push(BenchmarkFindingRecord {
            index: next_idx,
            severity,
            problem,
            explanation,
            preexisting,
            origin: None,
            prior_attribution: None,
            stages: Vec::new(),
            prompts: Vec::new(),
        });
    }

    Ok(())
}

async fn load_bugs_for_review(
    db: &Database,
    review_id: i64,
    findings: &mut Vec<BenchmarkFindingRecord>,
) -> Result<()> {
    let mut seen_problems: BTreeSet<String> = findings
        .iter()
        .map(|f| f.problem.trim().to_ascii_lowercase())
        .collect();

    if let Ok(bugs) = db.list_bugs_for_review(review_id).await {
        for (bug, _) in bugs {
            let problem = bug.problem().trim().to_string();
            if problem.is_empty() || !seen_problems.insert(problem.to_ascii_lowercase()) {
                continue;
            }
            let next_idx = findings.len() + 1;
            findings.push(BenchmarkFindingRecord {
                index: next_idx,
                severity: bug.severity().as_str().to_string(),
                problem,
                explanation: bug.severity_explanation(),
                preexisting: true,
                origin: Some("bug_repository".to_string()),
                prior_attribution: None,
                stages: Vec::new(),
                prompts: Vec::new(),
            });
        }
    }

    Ok(())
}

async fn evaluate_benchmark_entries(
    entries: &[BenchmarkEntry],
    commit_map: &HashMap<String, CommitReviewData>,
    ai_provider: Arc<dyn AiProvider>,
    concurrency: usize,
) -> Vec<BenchmarkEntryResult> {
    let total_entries = entries.len();
    let processed_count = Arc::new(AtomicUsize::new(0));

    futures_util::stream::iter(entries.iter().cloned().enumerate())
        .map(|(idx, entry)| {
            let client = ai_provider.clone();
            let commit_data = commit_map.get(entry.commit.trim()).cloned();
            let processed_count = processed_count.clone();
            async move {
                let res = evaluate_single_entry(client, entry, commit_data).await;
                let current = processed_count.fetch_add(1, Ordering::Relaxed) + 1;
                if current.is_multiple_of(5) || current == total_entries {
                    info!("Evaluation progress: {}/{}", current, total_entries);
                }
                (idx, res)
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .fold(vec![None; total_entries], |mut acc, (idx, item)| {
            acc[idx] = Some(item);
            acc
        })
        .into_iter()
        .flatten()
        .collect()
}

pub(crate) async fn evaluate_single_entry(
    client: Arc<dyn AiProvider>,
    entry: BenchmarkEntry,
    commit_data: Option<CommitReviewData>,
) -> BenchmarkEntryResult {
    let Some(problem_description) = entry
        .problem_description
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return empty_entry_result(
            &entry,
            "",
            "SKIPPED",
            "No problem description provided",
            None,
        );
    };

    let Some(data) = commit_data else {
        return empty_entry_result(
            &entry,
            &problem_description,
            "NOT_FOUND_IN_DB",
            "Patch not found in database.",
            Some("review_failed"),
        );
    };

    if data.patch_id.is_none() {
        return empty_entry_result(
            &entry,
            &problem_description,
            "NOT_FOUND_IN_DB",
            "Patch not found in database.",
            Some("review_failed"),
        );
    }

    if !data.is_completed_review() {
        return build_entry_result_from_data(
            &entry,
            &problem_description,
            &data,
            "NOT_REVIEWED".to_string(),
            None,
            format!(
                "Patch has no completed review (status: {}).",
                data.review_status.as_deref().unwrap_or("None")
            ),
            None,
            Some("review_failed".to_string()),
            None,
        );
    }

    if entry.expected_behavior.is_suppression() && data.findings.is_empty() {
        let mut result = build_entry_result_from_data(
            &entry,
            &problem_description,
            &data,
            "SUPPRESSED".to_string(),
            None,
            "No findings reported; false-positive claim remained suppressed.".to_string(),
            None,
            None,
            None,
        );
        result.suppressed_with_no_findings = true;
        return result;
    }

    let prompt = build_judge_prompt_for_entry(&entry, &problem_description, &data);
    let mut session = BenchmarkEvalSession::new(prompt);
    let runner = SessionRunner::new(client.as_ref());

    match runner.run(&mut session).await {
        Ok(run_res) => {
            let verdict = run_res.output;
            let judge_matched =
                verdict.status == "DETECTED" || verdict.status == "PARTIALLY_DETECTED";
            let matched_record = if judge_matched {
                verdict
                    .matched_finding_index
                    .and_then(|idx| data.findings.iter().find(|f| f.index == idx))
                    .or_else(|| {
                        if verdict.matched_finding_index.is_none() && data.findings.len() == 1 {
                            data.findings.first()
                        } else {
                            None
                        }
                    })
            } else {
                None
            };
            let matched_finding = matched_record.map(|f| MatchedFindingSummary {
                index: f.index,
                severity: f.severity.clone(),
                problem: f.problem.clone(),
                preexisting: f.preexisting,
                origin: f.origin.clone(),
                stages: f.stages.clone(),
                prompts: f.prompts.clone(),
            });

            if entry.expected_behavior.is_suppression() {
                let (status, explanation) = if judge_matched {
                    ("FALSE_POSITIVE".to_string(), verdict.explanation)
                } else {
                    (
                        "SUPPRESSED".to_string(),
                        format!(
                            "False-positive claim remained suppressed ({})",
                            verdict.explanation
                        ),
                    )
                };
                return build_entry_result_from_data(
                    &entry,
                    &problem_description,
                    &data,
                    status,
                    None,
                    explanation,
                    matched_finding,
                    None,
                    None,
                );
            }

            let attribution = score_attribution(&entry, matched_record);

            let (miss_reason, dismissed_by_stage) = if verdict.status == "MISSED" {
                let reason = verdict
                    .miss_reason
                    .unwrap_or_else(|| "not_raised".to_string());
                (Some(reason), verdict.dismissed_by_stage)
            } else {
                (None, None)
            };

            build_entry_result_from_data(
                &entry,
                &problem_description,
                &data,
                verdict.status,
                attribution,
                verdict.explanation,
                matched_finding,
                miss_reason,
                dismissed_by_stage,
            )
        }
        Err(e) => build_entry_result_from_data(
            &entry,
            &problem_description,
            &data,
            "UNKNOWN".to_string(),
            None,
            format!("Evaluation failed: {e}"),
            None,
            None,
            None,
        ),
    }
}

fn empty_entry_result(
    entry: &BenchmarkEntry,
    problem_description: &str,
    status: &str,
    explanation: &str,
    miss_reason: Option<&str>,
) -> BenchmarkEntryResult {
    BenchmarkEntryResult {
        commit: entry.commit.clone(),
        case_id: entry.case_id.clone(),
        category: entry.category.clone(),
        expected_behavior: entry.expected_behavior,
        subject: None,
        fixed_by: entry.fixed_by.clone(),
        subsystem: entry.subsystem.clone(),
        problem_description: problem_description.to_string(),
        found: false,
        status: status.to_string(),
        attributed: entry
            .expected_behavior
            .expects_attribution()
            .then_some(false),
        attribution: entry
            .expected_behavior
            .expects_attribution()
            .then_some(AttributionOutcome::Missing),
        suppressed_with_no_findings: false,
        versioned: entry.is_versioned(),
        explanation: explanation.to_string(),
        matched_finding: None,
        miss_reason: miss_reason.map(str::to_string),
        dismissed_by_stage: None,
        findings_count: 0,
        patch_findings_count: 0,
        preexisting_findings_count: 0,
        extra_findings_count: 0,
        concerns_count: 0,
        dismissed_concerns_count: 0,
        tokens_in: 0,
        tokens_out: 0,
        tokens_cached: 0,
        turns: 0,
        duration_secs: 0,
        review_id: None,
        patchset_id: None,
        patch_id: None,
        planned_stages: Vec::new(),
        selected_guides: Vec::new(),
        stages: Vec::new(),
        findings: Vec::new(),
        dismissed_concerns: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_entry_result_from_data(
    entry: &BenchmarkEntry,
    problem_description: &str,
    data: &CommitReviewData,
    status: String,
    attribution: Option<AttributionOutcome>,
    explanation: String,
    matched_finding: Option<MatchedFindingSummary>,
    miss_reason: Option<String>,
    dismissed_by_stage: Option<String>,
) -> BenchmarkEntryResult {
    // Every path that produced no scored attribution (unreviewed, judge
    // failure, no match) leaves an expected attribution missing.
    let attribution = attribution.or_else(|| {
        entry
            .expected_behavior
            .expects_attribution()
            .then_some(AttributionOutcome::Missing)
    });
    let attributed = entry
        .expected_behavior
        .expects_attribution()
        .then(|| attribution == Some(AttributionOutcome::Correct));
    let found = matches!(
        status.as_str(),
        "DETECTED" | "PARTIALLY_DETECTED" | "SUPPRESSED"
    );
    let patch_findings_count = data.patch_findings_count();
    let preexisting_findings_count = data.preexisting_findings_count();
    let findings_count = data.findings.len();
    let matched_patch_finding = found && matched_finding.as_ref().is_some_and(|mf| !mf.preexisting);
    let extra_findings_count = if matched_patch_finding {
        patch_findings_count.saturating_sub(1)
    } else {
        patch_findings_count
    };

    BenchmarkEntryResult {
        commit: entry.commit.clone(),
        case_id: entry.case_id.clone(),
        category: entry.category.clone(),
        expected_behavior: entry.expected_behavior,
        subject: data.subject.clone(),
        fixed_by: entry.fixed_by.clone(),
        subsystem: entry.subsystem.clone(),
        problem_description: problem_description.to_string(),
        found,
        status,
        attributed,
        attribution,
        suppressed_with_no_findings: false,
        versioned: entry.is_versioned(),
        explanation,
        matched_finding,
        miss_reason,
        dismissed_by_stage,
        findings_count,
        patch_findings_count,
        preexisting_findings_count,
        extra_findings_count,
        concerns_count: data.concerns_count,
        dismissed_concerns_count: data.dismissed_concerns_count,
        tokens_in: data.tokens_in,
        tokens_out: data.tokens_out,
        tokens_cached: data.tokens_cached,
        turns: data.turns,
        duration_secs: data.duration_secs,
        review_id: data.review_id,
        patchset_id: data.patchset_id,
        patch_id: data.patch_id,
        planned_stages: data.planned_stages.clone(),
        selected_guides: data.selected_guides.clone(),
        stages: data.stages.clone(),
        findings: data.findings.clone(),
        dismissed_concerns: data.dismissed_concerns.clone(),
    }
}

pub(crate) fn build_judge_prompt_for_entry(
    entry: &BenchmarkEntry,
    problem_description: &str,
    data: &CommitReviewData,
) -> String {
    let mut findings_block = String::new();
    if data.findings.is_empty() {
        findings_block.push_str("(No findings reported)\n");
    } else {
        for f in &data.findings {
            let stages_label = if f.stages.is_empty() {
                "unknown".to_string()
            } else {
                f.stages.join(", ")
            };
            findings_block.push_str(&format!(
                "[F{}] [Severity: {}] [Preexisting: {}] [Stages: {}] {}\n",
                f.index, f.severity, f.preexisting, stages_label, f.problem
            ));
            if let Some(ref expl) = f.explanation {
                findings_block.push_str(&format!("     Explanation: {}\n", expl.trim()));
            }
        }
    }

    let mut inline_block = String::new();
    if !data.inline_review.is_empty() {
        let truncated: String = data
            .inline_review
            .chars()
            .take(MAX_INLINE_REVIEW_PROMPT_CHARS)
            .collect();
        inline_block.push_str("\nInline Review Report:\n");
        inline_block.push_str(&truncated);
        inline_block.push('\n');
    }

    let mut dismissed_block = String::new();
    if data.dismissed_concerns.is_empty() {
        dismissed_block.push_str("(No dismissed concerns recorded)\n");
    } else {
        for d in &data.dismissed_concerns {
            dismissed_block.push_str(&format!(
                "[D{}] [Stage: {}] {}\n     Dismissal Reasoning: {}\n",
                d.index, d.stage, d.description, d.reasoning
            ));
        }
    }

    // Attribution is scored deterministically from structured fields
    // (`score_attribution`); the judge only decides whether a finding matches.
    // For suppression entries the description is a refuted claim, so the judge
    // is told explicitly that matching it means a false positive.
    let (claim_heading, claim_preamble, task_target) = if entry.expected_behavior.is_suppression() {
        (
            "Refuted Claim (FALSE; must NOT be reported):",
            "The description below is a claim that an earlier reviewer made about a previous version of this patch and that has been refuted: it is NOT a real defect in the reviewed code. A correct review does not report it. Your task is to detect whether the tool nevertheless reported this false claim. Use `\"DETECTED\"` or `\"PARTIALLY_DETECTED\"` only when a finding asserts this same refuted claim as an active defect; findings about unrelated issues do not count.\n\n",
            "the Refuted Claim (asserting it as an active defect)",
        )
    } else {
        (
            "Ground Truth Defect:",
            "",
            "the Ground Truth Defect as an active defect",
        )
    };

    format!(
        "You are evaluating an automated Linux kernel code review tool against a target claim or ground-truth bug.\n\n\
        {claim_preamble}\
        {claim_heading}\n\
        {problem_description}\n\n\
        Reported Findings:\n\
        {findings_block}\n\
        Review Summary:\n\
        {}\n\
        {inline_block}\n\
        Dismissed / Filtered Candidate Concerns:\n\
        {dismissed_block}\n\
        Task:\n\
        1. Determine whether ANY of the Reported Findings (`[F1]`, `[F2]`, ...) or the Review Summary reports {task_target}:\n\
           - `\"DETECTED\"`: A reported finding accurately identifies the specific root cause and location/mechanism.\n\
           - `\"PARTIALLY_DETECTED\"`: A reported finding identifies the general defect or symptom in the right area with minor inaccuracy.\n\
           - `\"MISSED\"`: None of the reported findings report it as a defect.\n\
        2. When `status` is `\"DETECTED\"` or `\"PARTIALLY_DETECTED\"`, set `matched_finding_index` to the integer `1..=N` of the best matching `[F<index>]` finding (or `null` if matched only in the summary).\n\
        3. When `status` is `\"MISSED\"`, inspect the Dismissed / Filtered Candidate Concerns (`[D1]`, `[D2]`, ...) to diagnose why it was not reported:\n\
           - Set `miss_reason` to `\"wrongly_dismissed\"` if a dismissed concern in `[D1..]` raised the claim and was later dismissed, and set `dismissed_by_stage` to that `[D<index>]`'s `Stage`.\n\
           - Set `miss_reason` to `\"preexisting_filtered\"` if a concern with `Stage: preexisting-filtered` matched the claim.\n\
           - Set `miss_reason` to `\"not_raised\"` if no specialist stage or dismissed concern ever mentioned the claim.\n\
        Respond ONLY with a valid JSON object matching this schema:\n\
        {{\n\
          \"status\": \"DETECTED\" | \"PARTIALLY_DETECTED\" | \"MISSED\",\n\
          \"matched_finding_index\": <number or null>,\n\
          \"miss_reason\": \"not_raised\" | \"wrongly_dismissed\" | \"preexisting_filtered\" | null,\n\
          \"dismissed_by_stage\": <string or null>,\n\
          \"explanation\": \"<one-sentence explanation referencing [F#] or [D#] when applicable>\"\n\
        }}",
        if data.summary_text.is_empty() {
            "(empty)"
        } else {
            &data.summary_text
        }
    )
}

pub(crate) fn parse_judge_verdict(raw: &str) -> Result<BenchmarkJudgeVerdict, ValidationError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::FormatViolation(
            "Empty response from benchmark judge".to_string(),
        ));
    }

    let json_candidate = extract_json_object(trimmed).unwrap_or(trimmed);
    if let Ok(mut parsed) = serde_json::from_str::<BenchmarkJudgeVerdict>(json_candidate) {
        parsed.status = parsed.status.trim().to_ascii_uppercase();
        if !matches!(
            parsed.status.as_str(),
            "DETECTED" | "PARTIALLY_DETECTED" | "MISSED"
        ) {
            return Err(ValidationError::FormatViolation(format!(
                "Invalid status '{}' in judge JSON; expected DETECTED, PARTIALLY_DETECTED, or MISSED",
                parsed.status
            )));
        }
        return Ok(parsed);
    }

    // Fallback for plain-text responses `[DETECTED | PARTIALLY_DETECTED | MISSED]: explanation`
    let re_status = &*JUDGE_STATUS_RE;
    if let Some(cap) = re_status.captures(trimmed) {
        let status = cap[1].to_ascii_uppercase();
        let remaining = re_status.replace(trimmed, "").to_string();
        let explanation = remaining
            .trim()
            .trim_start_matches([':', '-', ' ', '\n', ']', '['])
            .to_string();
        let matched_finding_index = extract_finding_tag_index(&explanation);
        return Ok(BenchmarkJudgeVerdict {
            miss_reason: (status == "MISSED").then(|| "not_raised".to_string()),
            status,
            matched_finding_index,
            dismissed_by_stage: None,
            explanation,
        });
    }

    Err(ValidationError::FormatViolation(
        "Judge response was neither valid JSON nor contained DETECTED/PARTIALLY_DETECTED/MISSED"
            .to_string(),
    ))
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then_some(&text[start..=end])
}

fn extract_finding_tag_index(text: &str) -> Option<usize> {
    let caps = FINDING_TAG_RE.captures(text)?;
    caps.get(1)?.as_str().parse::<usize>().ok()
}

struct BenchmarkEvalSession {
    prompt: String,
}

impl BenchmarkEvalSession {
    fn new(prompt: String) -> Self {
        Self { prompt }
    }
}

#[async_trait]
impl LlmSession for BenchmarkEvalSession {
    type Output = BenchmarkJudgeVerdict;

    fn system_prompt(&self) -> String {
        String::new()
    }

    fn initial_user_prompt(&self) -> String {
        self.prompt.clone()
    }

    fn tools(&self) -> Option<Vec<crate::ai::AiTool>> {
        None
    }

    fn temperature(&self) -> Option<f32> {
        Some(0.0)
    }

    fn context_tag(&self) -> Option<String> {
        Some("benchmark-judge".to_string())
    }

    async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
        bail!("BenchmarkEvalSession does not use tools")
    }

    fn validate(
        &mut self,
        response: &crate::ai::AiResponse,
    ) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        parse_judge_verdict(text)
    }

    fn handle_provider_error(
        &mut self,
        error: &anyhow::Error,
        _attempt: usize,
        has_active_tools: bool,
    ) -> crate::ai::ErrorAction {
        if crate::ai::session::is_malformed_tool_call_error(&error.to_string()) {
            crate::ai::ErrorAction::RetryWithFeedback(
                crate::ai::session::malformed_tool_call_feedback(has_active_tools),
            )
        } else {
            crate::ai::ErrorAction::Fail
        }
    }
}

/// Computes `total`, `mean`, `p50`, `p90`, and `max` over a slice of `u64` values.
pub fn compute_distribution(values: &[u64]) -> MetricDistribution {
    if values.is_empty() {
        return MetricDistribution::default();
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let total: u64 = sorted.iter().sum();
    let mean = round_one_decimal(total as f64 / sorted.len() as f64);
    let p50 = percentile_nearest_rank(&sorted, 50);
    let p90 = percentile_nearest_rank(&sorted, 90);
    let max = *sorted.last().unwrap_or(&0);
    MetricDistribution {
        total,
        mean,
        p50,
        p90,
        max,
    }
}

fn percentile_nearest_rank(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * pct) / 100;
    sorted[idx]
}

fn round_one_decimal(val: f64) -> f64 {
    (val * 10.0).round() / 10.0
}

/// Statuses for entries that were never judged: the review did not complete
/// or the judge evaluation failed. They count as failures in every rate.
fn is_failure_status(status: &str) -> bool {
    matches!(status, "NOT_REVIEWED" | "NOT_FOUND_IN_DB" | "UNKNOWN")
}

/// Strict per-entry pass: suppression entries must stay suppressed; detection
/// entries need an exact `DETECTED` verdict (partial matches never pass) and
/// an attribution outcome matching the expectation: correct when prior credit
/// is expected, absent otherwise.
fn entry_passes_strictly(res: &BenchmarkEntryResult) -> bool {
    if res.expected_behavior.is_suppression() {
        return res.status == "SUPPRESSED";
    }
    if res.status != "DETECTED" {
        return false;
    }
    if res.expected_behavior.expects_attribution() {
        res.attribution == Some(AttributionOutcome::Correct)
    } else {
        matches!(res.attribution, None | Some(AttributionOutcome::Acceptable))
    }
}

/// Original per-entry pass used for classic (non-versioned) entries.
fn entry_passes_legacy(res: &BenchmarkEntryResult) -> bool {
    if res.expected_behavior.is_suppression() {
        res.status == "SUPPRESSED"
    } else if res.expected_behavior.expects_attribution() {
        res.found && res.attributed == Some(true)
    } else {
        res.found
    }
}

pub(crate) fn build_benchmark_summary(
    entries: &[BenchmarkEntry],
    results: &[BenchmarkEntryResult],
    commit_map: &HashMap<String, CommitReviewData>,
) -> BenchmarkSummaryStats {
    let total_entries = results.len();
    let unique_commits_list = deduplicate_benchmark_commits(entries);
    let unique_commits = unique_commits_list.len();

    let mut detected = 0usize;
    let mut partially_detected = 0usize;
    let mut missed = 0usize;
    let mut suppressed = 0usize;
    let mut false_positives = 0usize;
    let mut suppression_failed = 0usize;
    let mut suppressed_with_no_findings = 0usize;
    let mut attribution_expected_entries = 0usize;
    let mut attributed = 0usize;
    let mut attribution_wrong = 0usize;
    let mut attribution_missing = 0usize;
    let mut unexpected_attributions = 0usize;
    let mut attribution_acceptable = 0usize;
    let mut acceptable_on_expected = 0usize;
    let mut not_reviewed = 0usize;
    let mut evaluation_failed = 0usize;
    let mut detection_failed = 0usize;
    let mut skipped = 0usize;

    let mut missed_not_raised = 0usize;
    let mut missed_wrongly_dismissed = 0usize;
    let mut missed_preexisting_filtered = 0usize;
    let mut missed_review_failed = 0usize;

    let mut detections_by_stage: BTreeMap<String, usize> = BTreeMap::new();
    let mut false_dismissals_by_stage: BTreeMap<String, usize> = BTreeMap::new();
    let mut subsystem_buckets: BTreeMap<String, (usize, usize, usize, usize)> = BTreeMap::new();
    let mut category_buckets: BTreeMap<String, CategoryBenchmarkSummary> = BTreeMap::new();
    let mut matched_findings_by_commit: HashMap<String, BTreeSet<usize>> = HashMap::new();
    let mut matched_patch_findings_by_commit: HashMap<String, BTreeSet<usize>> = HashMap::new();

    for res in results {
        let subsys = res
            .subsystem
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unspecified")
            .to_string();

        // Versioned entries: every entry that was not skipped for lacking a
        // description is scored, and unreviewed entries and failed judge
        // evaluations count as failures. Classic entries keep the original
        // semantics, which only score judged outcomes.
        let strict = res.versioned;
        let is_scored = if strict {
            res.status != "SKIPPED"
        } else {
            matches!(
                res.status.as_str(),
                "DETECTED" | "PARTIALLY_DETECTED" | "MISSED" | "SUPPRESSED" | "FALSE_POSITIVE"
            )
        };
        let is_failure = strict && is_failure_status(&res.status);

        if is_scored {
            if res.expected_behavior.expects_attribution() {
                attribution_expected_entries += 1;
            }
            match (res.expected_behavior.expects_attribution(), res.attribution) {
                (true, Some(AttributionOutcome::Correct)) => attributed += 1,
                (true, Some(AttributionOutcome::Wrong)) => attribution_wrong += 1,
                (true, Some(AttributionOutcome::Acceptable)) => {
                    attribution_acceptable += 1;
                    acceptable_on_expected += 1;
                }
                (false, Some(AttributionOutcome::Acceptable)) => attribution_acceptable += 1,
                (true, _) => attribution_missing += 1,
                (false, Some(AttributionOutcome::Wrong)) => unexpected_attributions += 1,
                (false, _) => {}
            }
            if res.expected_behavior.is_suppression() {
                if is_failure {
                    suppression_failed += 1;
                }
                if res.suppressed_with_no_findings {
                    suppressed_with_no_findings += 1;
                }
            } else if is_failure {
                detection_failed += 1;
            }
        }

        if let Some(cat_name) = res
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && is_scored
        {
            let cat = category_buckets
                .entry(cat_name.to_string())
                .or_insert_with(|| CategoryBenchmarkSummary {
                    category: cat_name.to_string(),
                    total: 0,
                    passed: 0,
                    detected: 0,
                    partially_detected: 0,
                    missed: 0,
                    suppressed: 0,
                    false_positives: 0,
                    attribution_expected: 0,
                    attributed: 0,
                    attribution_wrong: 0,
                    attribution_missing: 0,
                    attribution_acceptable: 0,
                    failed: 0,
                    pass_rate_pct: 0.0,
                });
            cat.total += 1;
            if res.expected_behavior.expects_attribution() {
                cat.attribution_expected += 1;
            }
            match res.attribution {
                Some(AttributionOutcome::Correct) => cat.attributed += 1,
                Some(AttributionOutcome::Wrong) => cat.attribution_wrong += 1,
                Some(AttributionOutcome::Missing) => cat.attribution_missing += 1,
                Some(AttributionOutcome::Acceptable) => cat.attribution_acceptable += 1,
                None => {}
            }
            match res.status.as_str() {
                "DETECTED" => cat.detected += 1,
                "PARTIALLY_DETECTED" => cat.partially_detected += 1,
                "MISSED" => cat.missed += 1,
                "SUPPRESSED" => cat.suppressed += 1,
                "FALSE_POSITIVE" => cat.false_positives += 1,
                _ if is_failure => cat.failed += 1,
                _ => {}
            }
            let passed = if strict {
                entry_passes_strictly(res)
            } else {
                entry_passes_legacy(res)
            };
            if passed {
                cat.passed += 1;
            }
        }

        match res.status.as_str() {
            "DETECTED" => {
                let bucket = subsystem_buckets.entry(subsys).or_insert((0, 0, 0, 0));
                detected += 1;
                bucket.0 += 1;
                bucket.1 += 1;
            }
            "PARTIALLY_DETECTED" => {
                let bucket = subsystem_buckets.entry(subsys).or_insert((0, 0, 0, 0));
                partially_detected += 1;
                bucket.0 += 1;
                bucket.2 += 1;
            }
            "MISSED" => {
                let bucket = subsystem_buckets.entry(subsys).or_insert((0, 0, 0, 0));
                missed += 1;
                bucket.0 += 1;
                bucket.3 += 1;
                match res.miss_reason.as_deref() {
                    Some("wrongly_dismissed") => {
                        missed_wrongly_dismissed += 1;
                        let stage = res
                            .dismissed_by_stage
                            .as_deref()
                            .unwrap_or("verification")
                            .to_string();
                        *false_dismissals_by_stage.entry(stage).or_insert(0) += 1;
                    }
                    Some("preexisting_filtered") => missed_preexisting_filtered += 1,
                    Some("review_failed") => missed_review_failed += 1,
                    _ => missed_not_raised += 1,
                }
            }
            "SUPPRESSED" => {
                suppressed += 1;
            }
            "FALSE_POSITIVE" => {
                false_positives += 1;
            }
            "NOT_REVIEWED" | "NOT_FOUND_IN_DB" => {
                not_reviewed += 1;
                missed_review_failed += 1;
            }
            "UNKNOWN" => {
                evaluation_failed += 1;
            }
            "SKIPPED" => {
                skipped += 1;
            }
            _ => {}
        }

        if res.found
            && let Some(ref mf) = res.matched_finding
        {
            let commit_key = res.commit.trim().to_string();
            matched_findings_by_commit
                .entry(commit_key.clone())
                .or_default()
                .insert(mf.index);
            if !mf.preexisting {
                matched_patch_findings_by_commit
                    .entry(commit_key)
                    .or_default()
                    .insert(mf.index);
            }
            if mf.stages.is_empty() {
                if let Some(ref origin) = mf.origin {
                    *detections_by_stage.entry(origin.clone()).or_insert(0) += 1;
                } else {
                    *detections_by_stage
                        .entry("unspecified".to_string())
                        .or_insert(0) += 1;
                }
            } else {
                for stage in &mf.stages {
                    *detections_by_stage.entry(stage.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    let evaluated_entries = detected + partially_detected + missed + detection_failed;
    let exact_detection_rate_pct = if evaluated_entries > 0 {
        round_one_decimal((detected as f64 * 100.0) / evaluated_entries as f64)
    } else {
        0.0
    };
    let total_detection_rate_pct = if evaluated_entries > 0 {
        round_one_decimal(
            ((detected + partially_detected) as f64 * 100.0) / evaluated_entries as f64,
        )
    } else {
        0.0
    };
    let suppression_evaluated_entries = suppressed + false_positives + suppression_failed;
    let false_positive_suppression_rate_pct = if suppression_evaluated_entries > 0 {
        round_one_decimal((suppressed as f64 * 100.0) / suppression_evaluated_entries as f64)
    } else {
        0.0
    };
    let attribution_rate_denominator = attribution_expected_entries - acceptable_on_expected;
    let attribution_rate_pct = if attribution_rate_denominator > 0 {
        round_one_decimal((attributed as f64 * 100.0) / attribution_rate_denominator as f64)
    } else {
        0.0
    };
    let emitted_attributions = attributed + attribution_wrong + unexpected_attributions;
    let attribution_precision_pct = if emitted_attributions > 0 {
        round_one_decimal((attributed as f64 * 100.0) / emitted_attributions as f64)
    } else {
        0.0
    };

    // Deduplicated per-commit metrics
    let mut reviewed_commits = 0usize;
    let mut total_patch_findings = 0usize;
    let mut total_preexisting_findings = 0usize;
    let mut matched_findings = 0usize;
    let mut unmatched_extra_findings = 0usize;
    let mut clean_target_only_commits = 0usize;
    let mut findings_by_severity = SeverityCounts::default();
    let mut total_concerns_raised = 0usize;
    let mut total_concerns_dismissed = 0usize;

    let mut tokens_in_vec = Vec::new();
    let mut tokens_out_vec = Vec::new();
    let mut tokens_cached_vec = Vec::new();
    let mut turns_vec = Vec::new();
    let mut duration_vec = Vec::new();

    for sha in &unique_commits_list {
        let Some(data) = commit_map.get(sha) else {
            continue;
        };
        if !data.is_completed_review() {
            continue;
        }
        reviewed_commits += 1;
        let patch_f = data.patch_findings_count();
        let pre_f = data.preexisting_findings_count();
        total_patch_findings += patch_f;
        total_preexisting_findings += pre_f;
        total_concerns_raised += data.concerns_count;
        total_concerns_dismissed += data.dismissed_concerns_count;

        for f in &data.findings {
            match f.severity.to_ascii_lowercase().as_str() {
                "critical" => findings_by_severity.critical += 1,
                "high" => findings_by_severity.high += 1,
                "medium" => findings_by_severity.medium += 1,
                _ => findings_by_severity.low += 1,
            }
        }

        let matched_for_commit = matched_findings_by_commit
            .get(sha)
            .map(|s| s.len())
            .unwrap_or(0);
        let matched_patch_for_commit = matched_patch_findings_by_commit
            .get(sha)
            .map(|s| s.len())
            .unwrap_or(0);
        matched_findings += matched_for_commit;
        let extra = patch_f.saturating_sub(matched_patch_for_commit);
        unmatched_extra_findings += extra;
        if matched_for_commit > 0 && extra == 0 {
            clean_target_only_commits += 1;
        }

        tokens_in_vec.push(data.tokens_in as u64);
        tokens_out_vec.push(data.tokens_out as u64);
        tokens_cached_vec.push(data.tokens_cached as u64);
        turns_vec.push(data.turns as u64);
        duration_vec.push(data.duration_secs);
    }

    let avg_patch_findings_per_commit = if reviewed_commits > 0 {
        round_one_decimal(total_patch_findings as f64 / reviewed_commits as f64)
    } else {
        0.0
    };
    let concern_to_finding_conversion_pct = if total_concerns_raised > 0 {
        round_one_decimal((total_patch_findings as f64 * 100.0) / total_concerns_raised as f64)
    } else {
        0.0
    };

    let tokens_in = compute_distribution(&tokens_in_vec);
    let tokens_out = compute_distribution(&tokens_out_vec);
    let tokens_cached = compute_distribution(&tokens_cached_vec);
    let turns = compute_distribution(&turns_vec);
    let duration_secs = compute_distribution(&duration_vec);

    let cache_hit_rate_pct = if tokens_in.total > 0 {
        round_one_decimal((tokens_cached.total as f64 * 100.0) / tokens_in.total as f64)
    } else {
        0.0
    };

    let by_subsystem: Vec<SubsystemBenchmarkSummary> = subsystem_buckets
        .into_iter()
        .map(|(subsystem, (total, det, part, miss))| {
            let rate = if total > 0 {
                round_one_decimal(((det + part) as f64 * 100.0) / total as f64)
            } else {
                0.0
            };
            SubsystemBenchmarkSummary {
                subsystem,
                total,
                detected: det,
                partially_detected: part,
                missed: miss,
                detection_rate_pct: rate,
            }
        })
        .collect();

    let by_category: Vec<CategoryBenchmarkSummary> = category_buckets
        .into_values()
        .map(|mut cat| {
            cat.pass_rate_pct = if cat.total > 0 {
                round_one_decimal((cat.passed as f64 * 100.0) / cat.total as f64)
            } else {
                0.0
            };
            cat
        })
        .collect();

    BenchmarkSummaryStats {
        total_entries,
        evaluated_entries,
        unique_commits,
        reviewed_commits,
        detected,
        partially_detected,
        missed,
        not_reviewed,
        evaluation_failed,
        skipped,
        exact_detection_rate_pct,
        total_detection_rate_pct,
        suppression_evaluated_entries,
        suppressed,
        false_positives,
        false_positive_suppression_rate_pct,
        suppressed_with_no_findings,
        attribution_expected_entries,
        attributed,
        attribution_wrong,
        attribution_missing,
        attribution_acceptable,
        attribution_scored_entries: attribution_rate_denominator,
        unexpected_attributions,
        attribution_rate_pct,
        attribution_precision_pct,
        missed_not_raised,
        missed_wrongly_dismissed,
        missed_preexisting_filtered,
        missed_review_failed,
        total_patch_findings,
        total_preexisting_findings,
        matched_findings,
        unmatched_extra_findings,
        avg_patch_findings_per_commit,
        clean_target_only_commits,
        findings_by_severity,
        total_concerns_raised,
        total_concerns_dismissed,
        concern_to_finding_conversion_pct,
        detections_by_stage,
        false_dismissals_by_stage,
        by_subsystem,
        by_category,
        tokens_in,
        tokens_out,
        tokens_cached,
        cache_hit_rate_pct,
        turns,
        duration_secs,
    }
}

/// Formats a Markdown summary report for `summary.md`.
pub fn format_benchmark_summary_markdown(report: &BenchmarkRunReport) -> String {
    let m = &report.metadata;
    let s = &report.summary;
    let mut out = String::new();

    out.push_str(&format!("# Sashiko Benchmark Report: `{}`\n\n", m.run_id));
    out.push_str("## Run Metadata\n\n");
    out.push_str(&format!(
        "- **Sashiko Version**: `{}` (`{}{}`)\n",
        m.sashiko_version,
        m.sashiko_git_sha,
        if m.sashiko_git_dirty { "-dirty" } else { "" }
    ));
    out.push_str(&format!(
        "- **Prompts Revision**: `{}`\n",
        m.prompts_git_hash
    ));
    out.push_str(&format!("- **Benchmark Suite**: `{}`\n", m.benchmark_file));
    out.push_str(&format!(
        "- **Model / Provider**: `{}` (`{}`), effort: `{}`\n",
        m.model,
        m.ai_provider,
        m.effort.as_deref().unwrap_or("default")
    ));
    out.push_str(&format!(
        "- **Entries / Unique Commits**: {} entries across {} unique commits (reviewed: {})\n",
        s.total_entries, s.unique_commits, s.reviewed_commits
    ));
    out.push_str(&format!(
        "- **Wall-Clock Time**: {}s (`{}` -> `{}`)\n",
        m.wall_clock_secs, m.started_at, m.finished_at
    ));
    out.push_str(&format!(
        "- **Preserved Database**: `{}`\n\n",
        m.database_path
    ));

    out.push_str("## Detection Accuracy\n\n");
    out.push_str("| Metric | Count | Rate |\n|---|---:|---:|\n");
    out.push_str(&format!(
        "| **Detected (Exact)** | {} | {:.1}% |\n",
        s.detected, s.exact_detection_rate_pct
    ));
    out.push_str(&format!(
        "| **Partially Detected** | {} | {:.1}% |\n",
        s.partially_detected,
        (s.total_detection_rate_pct - s.exact_detection_rate_pct).max(0.0)
    ));
    out.push_str(&format!(
        "| **Total Detected** | **{} / {}** | **{:.1}%** |\n",
        s.detected + s.partially_detected,
        s.evaluated_entries,
        s.total_detection_rate_pct
    ));
    out.push_str(&format!(
        "| **Missed** | {} | {:.1}% |\n",
        s.missed,
        if s.evaluated_entries > 0 {
            (s.missed as f64 * 100.0) / s.evaluated_entries as f64
        } else {
            0.0
        }
    ));
    let any_versioned = report.results.iter().any(|r| r.versioned);
    if any_versioned {
        out.push_str(&format!(
            "| Not Reviewed / Failed (scored as failures) | {} | - |\n",
            s.not_reviewed
        ));
        out.push_str(&format!(
            "| Judge Evaluation Failed (scored as failures) | {} | - |\n",
            s.evaluation_failed
        ));
    } else {
        out.push_str(&format!(
            "| Not Reviewed / Failed | {} | - |\n",
            s.not_reviewed
        ));
    }
    out.push_str(&format!(
        "| Skipped (No Description) | {} | - |\n\n",
        s.skipped
    ));

    if s.suppression_evaluated_entries > 0 || s.attribution_expected_entries > 0 {
        out.push_str("## Versioned Feedback Metrics\n\n");
        out.push_str("| Metric | Count | Rate |\n|---|---:|---:|\n");
        if s.attribution_expected_entries > 0 {
            let pct = |n: usize| (n as f64 * 100.0) / s.attribution_expected_entries as f64;
            out.push_str(&format!(
                "| **Prior-Reviewer Attribution (correct)** | **{} / {}** | **{:.1}%** |\n",
                s.attributed, s.attribution_scored_entries, s.attribution_rate_pct
            ));
            out.push_str(&format!(
                "| Attribution Wrong | {} | {:.1}% |\n",
                s.attribution_wrong,
                pct(s.attribution_wrong)
            ));
            out.push_str(&format!(
                "| Attribution Missing | {} | {:.1}% |\n",
                s.attribution_missing,
                pct(s.attribution_missing)
            ));
        }
        out.push_str(&format!(
            "| Unexpected Attributions (no prior credit expected) | {} | - |\n",
            s.unexpected_attributions
        ));
        out.push_str(&format!(
            "| Acceptable Attributions (optional credit, not scored) | {} | - |\n",
            s.attribution_acceptable
        ));
        out.push_str(&format!(
            "| Attribution Precision (correct / emitted) | {} / {} | {:.1}% |\n",
            s.attributed,
            s.attributed + s.attribution_wrong + s.unexpected_attributions,
            s.attribution_precision_pct
        ));
        if s.suppression_evaluated_entries > 0 {
            out.push_str(&format!(
                "| **False-Positive Suppression** | **{} / {}** | **{:.1}%** |\n",
                s.suppressed,
                s.suppression_evaluated_entries,
                s.false_positive_suppression_rate_pct
            ));
            out.push_str(&format!(
                "| False Positives Reported | {} | {:.1}% |\n",
                s.false_positives,
                (s.false_positives as f64 * 100.0) / s.suppression_evaluated_entries as f64
            ));
            out.push_str(&format!(
                "| Suppressed With Zero Findings (vacuous pass) | {} | - |\n",
                s.suppressed_with_no_findings
            ));
        }
        out.push('\n');
    }

    if !s.by_category.is_empty() {
        out.push_str("## Category Breakdown\n\n");
        out.push_str("Passed is strict: exact detection only (partial matches never pass), correct attribution where prior credit is expected and none otherwise, or suppression. Failed entries (no completed review or judge failure) count in Total.\n\n");
        out.push_str("| Category | Passed | Detected | Partial | Attributed (ok/wrong/missing) | Suppressed | False Pos | Missed | Failed | Total | Pass Rate |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
        for cat in &s.by_category {
            let attr_cell = if cat.attribution_expected > 0 || cat.attribution_wrong > 0 {
                format!(
                    "{}/{}/{} of {}",
                    cat.attributed,
                    cat.attribution_wrong,
                    cat.attribution_missing,
                    cat.attribution_expected
                )
            } else {
                "-".to_string()
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.1}% |\n",
                cat.category,
                cat.passed,
                cat.detected,
                cat.partially_detected,
                attr_cell,
                cat.suppressed,
                cat.false_positives,
                cat.missed,
                cat.failed,
                cat.total,
                cat.pass_rate_pct
            ));
        }
        out.push('\n');
    }

    out.push_str("## Miss Diagnosis & Precision\n\n");
    out.push_str(&format!(
        "- **Miss Breakdown**: `not_raised`: {}, `wrongly_dismissed`: {}, `preexisting_filtered`: {}, `review_failed`: {}\n",
        s.missed_not_raised,
        s.missed_wrongly_dismissed,
        s.missed_preexisting_filtered,
        s.missed_review_failed
    ));
    out.push_str(&format!(
        "- **Patch Findings**: {} total ({:.1} avg/commit) — {} matched target bugs, {} extra/unmatched findings\n",
        s.total_patch_findings,
        s.avg_patch_findings_per_commit,
        s.matched_findings,
        s.unmatched_extra_findings
    ));
    out.push_str(&format!(
        "- **Clean Target-Only Commits**: {} / {}\n",
        s.clean_target_only_commits, s.reviewed_commits
    ));
    out.push_str(&format!(
        "- **Pre-existing Findings**: {}\n",
        s.total_preexisting_findings
    ));
    out.push_str(&format!(
        "- **Findings by Severity**: Critical: {}, High: {}, Medium: {}, Low: {}\n",
        s.findings_by_severity.critical,
        s.findings_by_severity.high,
        s.findings_by_severity.medium,
        s.findings_by_severity.low
    ));
    out.push_str(&format!(
        "- **Concerns Raised / Dismissed**: {} raised, {} dismissed ({:.1}% concern-to-finding conversion)\n\n",
        s.total_concerns_raised, s.total_concerns_dismissed, s.concern_to_finding_conversion_pct
    ));

    out.push_str("## Resource & Latency Distributions (Per Unique Reviewed Commit)\n\n");
    out.push_str("| Metric | Total | Mean | P50 | P90 | Max |\n|---|---:|---:|---:|---:|---:|\n");
    out.push_str(&format!(
        "| Tokens In | {} | {:.1} | {} | {} | {} |\n",
        s.tokens_in.total, s.tokens_in.mean, s.tokens_in.p50, s.tokens_in.p90, s.tokens_in.max
    ));
    out.push_str(&format!(
        "| Tokens Out | {} | {:.1} | {} | {} | {} |\n",
        s.tokens_out.total, s.tokens_out.mean, s.tokens_out.p50, s.tokens_out.p90, s.tokens_out.max
    ));
    out.push_str(&format!(
        "| Tokens Cached ({:.1}% hit) | {} | {:.1} | {} | {} | {} |\n",
        s.cache_hit_rate_pct,
        s.tokens_cached.total,
        s.tokens_cached.mean,
        s.tokens_cached.p50,
        s.tokens_cached.p90,
        s.tokens_cached.max
    ));
    out.push_str(&format!(
        "| Turns | {} | {:.1} | {} | {} | {} |\n",
        s.turns.total, s.turns.mean, s.turns.p50, s.turns.p90, s.turns.max
    ));
    out.push_str(&format!(
        "| Duration (s) | {} | {:.1} | {} | {} | {} |\n\n",
        s.duration_secs.total,
        s.duration_secs.mean,
        s.duration_secs.p50,
        s.duration_secs.p90,
        s.duration_secs.max
    ));

    if !s.by_subsystem.is_empty() {
        out.push_str("## Subsystem Breakdown\n\n");
        out.push_str("| Subsystem | Detected | Partial | Missed | Total | Rate |\n|---|---:|---:|---:|---:|---:|\n");
        for sub in &s.by_subsystem {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {:.1}% |\n",
                sub.subsystem,
                sub.detected,
                sub.partially_detected,
                sub.missed,
                sub.total,
                sub.detection_rate_pct
            ));
        }
        out.push('\n');
    }

    out.push_str("## Per-Entry Results\n\n");
    out.push_str("| Commit | Subsystem | Status | Matched / Miss Reason | Findings (Patch/Extra) | Turns | Time (s) | Explanation |\n|---|---|---|---|---:|---:|---:|---|\n");
    for r in &report.results {
        let short_commit: String = r.commit.chars().take(12).collect();
        let subsys = r.subsystem.as_deref().unwrap_or("-");
        let detail = if let Some(ref mf) = r.matched_finding {
            let stages = if mf.stages.is_empty() {
                "-".to_string()
            } else {
                mf.stages.join("+")
            };
            format!("F{} [{}] ({})", mf.index, mf.severity, stages)
        } else if let Some(ref mr) = r.miss_reason {
            if let Some(ref stage) = r.dismissed_by_stage {
                format!("{mr} ({stage})")
            } else {
                mr.clone()
            }
        } else {
            "-".to_string()
        };
        let clean_expl = r.explanation.replace(['\n', '|'], " ");
        let status = match r.attribution.filter(|_| r.versioned) {
            Some(AttributionOutcome::Correct) => format!("{} (attribution: correct)", r.status),
            Some(AttributionOutcome::Wrong) => format!("{} (attribution: wrong)", r.status),
            Some(AttributionOutcome::Missing) => format!("{} (attribution: missing)", r.status),
            Some(AttributionOutcome::Acceptable) => {
                format!("{} (attribution: acceptable)", r.status)
            }
            None if r.suppressed_with_no_findings => format!("{} (no findings)", r.status),
            None => r.status.clone(),
        };
        out.push_str(&format!(
            "| `{}` | `{}` | **{}** | `{}` | {}/{} | {} | {} | {} |\n",
            short_commit,
            subsys,
            status,
            detail,
            r.patch_findings_count,
            r.extra_findings_count,
            r.turns,
            r.duration_secs,
            clean_expl
        ));
    }

    out
}

fn write_benchmark_artifacts(report: &BenchmarkRunReport, paths: &BenchmarkRunPaths) -> Result<()> {
    let json_bytes = serde_json::to_vec_pretty(report)?;
    std::fs::write(&paths.results_json_path, &json_bytes).with_context(|| {
        format!(
            "Failed to write benchmark results JSON to {}",
            paths.results_json_path.display()
        )
    })?;

    // Backwards-compatible copy in the current working directory.
    let legacy_path = Path::new("benchmark_results.json");
    if paths.results_json_path != legacy_path {
        let _ = std::fs::write(legacy_path, &json_bytes);
    }

    let md = format_benchmark_summary_markdown(report);
    let mut md_file = File::create(&paths.summary_md_path).with_context(|| {
        format!(
            "Failed to write benchmark Markdown summary to {}",
            paths.summary_md_path.display()
        )
    })?;
    md_file.write_all(md.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_deduplicate_benchmark_commits_preserves_order() {
        let entries = vec![
            BenchmarkEntry {
                commit: "aaa111".to_string(),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: Some("bug 1".to_string()),
                ..Default::default()
            },
            BenchmarkEntry {
                commit: "bbb222".to_string(),
                fixed_by: None,
                subsystem: Some("mm".to_string()),
                problem_description: Some("bug 2".to_string()),
                ..Default::default()
            },
            BenchmarkEntry {
                commit: "aaa111".to_string(),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: Some("bug 3 on same commit".to_string()),
                ..Default::default()
            },
        ];

        let unique = deduplicate_benchmark_commits(&entries);
        assert_eq!(unique, vec!["aaa111".to_string(), "bbb222".to_string()]);
    }

    #[test]
    fn test_prepare_benchmark_run_paths_creates_run_dir_with_sha_and_date() -> Result<()> {
        let temp = tempdir()?;
        let bench_file = Path::new("projects/linux/benchmarks/benchmark_tiny.json");
        let paths = prepare_benchmark_run_paths(
            temp.path(),
            bench_file,
            "20261019-153000",
            "715294a276ef",
            None,
            None,
            false,
        )?;

        assert_eq!(paths.run_id, "20261019-153000_715294a276ef_benchmark_tiny");
        assert!(paths.run_dir.exists());
        assert_eq!(
            paths.db_path.file_name().and_then(|n| n.to_str()),
            Some("sashiko-20261019-153000-715294a276ef.db")
        );
        assert_eq!(paths.results_json_path, paths.run_dir.join("results.json"));
        assert_eq!(paths.summary_md_path, paths.run_dir.join("summary.md"));

        // Create the DB file and verify analyze_only auto-discovers it even when
        // explicit_output_dir is passed (as the CLI default does).
        std::fs::write(&paths.db_path, b"sqlite")?;
        let discovered = prepare_benchmark_run_paths(
            temp.path(),
            bench_file,
            "20261019-160000",
            "715294a276ef",
            None,
            Some(Path::new("benchmarks/runs")),
            true,
        )?;
        assert_eq!(discovered.db_path, paths.db_path);
        assert_eq!(discovered.run_dir, paths.run_dir);

        // Verify analyze_only fails cleanly when no prior run exists for a suite.
        let missing_err = prepare_benchmark_run_paths(
            temp.path(),
            Path::new("projects/linux/benchmarks/nonexistent_suite.json"),
            "20261019-160000",
            "715294a276ef",
            None,
            Some(Path::new("benchmarks/runs")),
            true,
        )
        .expect_err("Expected error when analyze_only finds no existing run");
        assert!(
            missing_err
                .to_string()
                .contains("No existing benchmark database found")
        );
        Ok(())
    }

    #[test]
    fn test_compute_distribution_stats() {
        let empty = compute_distribution(&[]);
        assert_eq!(empty, MetricDistribution::default());

        let dist = compute_distribution(&[100, 200, 300, 400, 1000]);
        assert_eq!(dist.total, 2000);
        assert_eq!(dist.mean, 400.0);
        assert_eq!(dist.p50, 300);
        assert_eq!(dist.p90, 400);
        assert_eq!(dist.max, 1000);
    }

    #[test]
    fn test_parse_judge_verdict_json_and_fallback() {
        let json_resp = r#"```json
        {
          "status": "DETECTED",
          "matched_finding_index": 2,
          "miss_reason": null,
          "dismissed_by_stage": null,
          "explanation": "Matches [F2] which reports the exact use-after-free."
        }
        ```"#;
        let parsed = parse_judge_verdict(json_resp).unwrap();
        assert_eq!(parsed.status, "DETECTED");
        assert_eq!(parsed.matched_finding_index, Some(2));
        assert!(parsed.explanation.contains("[F2]"));

        let missed_json = r#"{
          "status": "MISSED",
          "matched_finding_index": null,
          "miss_reason": "wrongly_dismissed",
          "dismissed_by_stage": "verification",
          "explanation": "[D1] raised the null deref in locking stage, but verification dismissed it."
        }"#;
        let parsed_miss = parse_judge_verdict(missed_json).unwrap();
        assert_eq!(parsed_miss.status, "MISSED");
        assert_eq!(
            parsed_miss.miss_reason.as_deref(),
            Some("wrongly_dismissed")
        );
        assert_eq!(
            parsed_miss.dismissed_by_stage.as_deref(),
            Some("verification")
        );

        let plain_resp = "PARTIALLY_DETECTED: Finding [F1] identifies the leak on the error path.";
        let parsed_plain = parse_judge_verdict(plain_resp).unwrap();
        assert_eq!(parsed_plain.status, "PARTIALLY_DETECTED");
        assert_eq!(parsed_plain.matched_finding_index, Some(1));
    }

    #[test]
    fn test_build_benchmark_summary_deduplicates_multi_entry_commit_metrics() {
        let entries = vec![
            BenchmarkEntry {
                commit: "b168ed458dde".to_string(),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: Some("First bug in commit".to_string()),
                ..Default::default()
            },
            BenchmarkEntry {
                commit: "b168ed458dde".to_string(),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: Some("Second bug in same commit".to_string()),
                ..Default::default()
            },
        ];

        let finding_1 = BenchmarkFindingRecord {
            index: 1,
            severity: "High".to_string(),
            problem: "First bug".to_string(),
            explanation: None,
            preexisting: false,
            origin: Some("verification".to_string()),
            prior_attribution: None,
            stages: vec!["resources".to_string()],
            prompts: vec![],
        };
        let finding_2 = BenchmarkFindingRecord {
            index: 2,
            severity: "Medium".to_string(),
            problem: "Unrelated noise finding".to_string(),
            explanation: None,
            preexisting: false,
            origin: Some("verification".to_string()),
            prior_attribution: None,
            stages: vec!["implementation".to_string()],
            prompts: vec![],
        };

        let mut commit_map = HashMap::new();
        commit_map.insert(
            "b168ed458dde".to_string(),
            CommitReviewData {
                subject: Some("net: sample commit".to_string()),
                patchset_id: Some(1),
                patch_id: Some(1),
                review_id: Some(1),
                review_status: Some("Reviewed".to_string()),
                summary_text: "Summary".to_string(),
                inline_review: String::new(),
                tokens_in: 50_000,
                tokens_out: 5_000,
                tokens_cached: 25_000,
                turns: 8,
                duration_secs: 40,
                concerns_count: 3,
                dismissed_concerns_count: 1,
                planned_stages: vec!["resources".to_string()],
                selected_guides: vec![],
                stages: vec![],
                findings: vec![finding_1.clone(), finding_2],
                dismissed_concerns: vec![BenchmarkDismissedRecord {
                    index: 1,
                    stage: "verification".to_string(),
                    description: "Second bug".to_string(),
                    reasoning: "Assumed caller held lock".to_string(),
                }],
            },
        );

        let results = vec![
            BenchmarkEntryResult {
                commit: "b168ed458dde".to_string(),
                subject: Some("net: sample commit".to_string()),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: "First bug in commit".to_string(),
                found: true,
                status: "DETECTED".to_string(),
                explanation: "Matches [F1]".to_string(),
                matched_finding: Some(MatchedFindingSummary {
                    index: 1,
                    severity: "High".to_string(),
                    problem: "First bug".to_string(),
                    preexisting: false,
                    origin: Some("verification".to_string()),
                    stages: vec!["resources".to_string()],
                    prompts: vec![],
                }),
                miss_reason: None,
                dismissed_by_stage: None,
                findings_count: 2,
                patch_findings_count: 2,
                preexisting_findings_count: 0,
                extra_findings_count: 1,
                concerns_count: 3,
                dismissed_concerns_count: 1,
                tokens_in: 50_000,
                tokens_out: 5_000,
                tokens_cached: 25_000,
                turns: 8,
                duration_secs: 40,
                review_id: Some(1),
                patchset_id: Some(1),
                patch_id: Some(1),
                planned_stages: vec!["resources".to_string()],
                selected_guides: vec![],
                stages: vec![],
                findings: vec![],
                dismissed_concerns: vec![],
                ..Default::default()
            },
            BenchmarkEntryResult {
                commit: "b168ed458dde".to_string(),
                subject: Some("net: sample commit".to_string()),
                fixed_by: None,
                subsystem: Some("net".to_string()),
                problem_description: "Second bug in same commit".to_string(),
                found: false,
                status: "MISSED".to_string(),
                explanation: "[D1] was dismissed in verification".to_string(),
                matched_finding: None,
                miss_reason: Some("wrongly_dismissed".to_string()),
                dismissed_by_stage: Some("verification".to_string()),
                findings_count: 2,
                patch_findings_count: 2,
                preexisting_findings_count: 0,
                extra_findings_count: 2,
                concerns_count: 3,
                dismissed_concerns_count: 1,
                tokens_in: 50_000,
                tokens_out: 5_000,
                tokens_cached: 25_000,
                turns: 8,
                duration_secs: 40,
                review_id: Some(1),
                patchset_id: Some(1),
                patch_id: Some(1),
                planned_stages: vec!["resources".to_string()],
                selected_guides: vec![],
                stages: vec![],
                findings: vec![],
                dismissed_concerns: vec![],
                ..Default::default()
            },
        ];

        let summary = build_benchmark_summary(&entries, &results, &commit_map);
        assert_eq!(summary.total_entries, 2);
        assert_eq!(summary.unique_commits, 1);
        assert_eq!(summary.reviewed_commits, 1);
        assert_eq!(summary.detected, 1);
        assert_eq!(summary.missed, 1);
        assert_eq!(summary.missed_wrongly_dismissed, 1);
        assert_eq!(
            summary.false_dismissals_by_stage.get("verification"),
            Some(&1)
        );
        assert_eq!(summary.detections_by_stage.get("resources"), Some(&1));
        // Tokens and findings are counted once for the unique commit, not doubled!
        assert_eq!(summary.tokens_in.total, 50_000);
        assert_eq!(summary.tokens_cached.total, 25_000);
        assert_eq!(summary.cache_hit_rate_pct, 50.0);
        assert_eq!(summary.total_patch_findings, 2);
        assert_eq!(summary.matched_findings, 1);
        assert_eq!(summary.unmatched_extra_findings, 1);
    }

    #[test]
    fn test_failed_reviews_excluded_from_reviewed_commits_and_sequential_finding_indices() {
        let mut data = CommitReviewData::default();
        let output = serde_json::json!({
            "findings": [
                { "problem": "   ", "severity": "Low" },
                { "problem": "Patch null deref", "severity": "High", "preexisting": false }
            ],
            "concerns": [
                { "description": "Patch null deref" },
                { "description": "Pre-existing race in foo" },
                { "description": "Filtered pre-existing concern" }
            ]
        });
        populate_findings_and_stages_from_output(&mut data, &output);
        assert_eq!(data.findings.len(), 1);
        assert_eq!(data.findings[0].index, 1);

        // Simulate merging a pre-existing bug from the bugs table before filtering concerns.
        data.findings.push(BenchmarkFindingRecord {
            index: data.findings.len() + 1,
            severity: "High".to_string(),
            problem: "Pre-existing race in foo".to_string(),
            explanation: None,
            preexisting: true,
            origin: Some("bug_repository".to_string()),
            prior_attribution: None,
            stages: vec![],
            prompts: vec![],
        });
        populate_dismissed_from_output(&mut data, &output);
        assert_eq!(data.dismissed_concerns.len(), 1);
        assert_eq!(
            data.dismissed_concerns[0].description,
            "Filtered pre-existing concern"
        );

        // A failed review with a non-null review_id must not count as a reviewed commit.
        let entries = vec![BenchmarkEntry {
            commit: "failed123".to_string(),
            fixed_by: None,
            subsystem: Some("mm".to_string()),
            problem_description: Some("bug".to_string()),
            ..Default::default()
        }];
        let mut commit_map = HashMap::new();
        commit_map.insert(
            "failed123".to_string(),
            CommitReviewData {
                review_id: Some(99),
                review_status: Some("Failed".to_string()),
                tokens_in: 1000,
                ..Default::default()
            },
        );
        let results = vec![empty_entry_result(
            &entries[0],
            "bug",
            "NOT_REVIEWED",
            "Failed",
            Some("review_failed"),
        )];
        let summary = build_benchmark_summary(&entries, &results, &commit_map);
        assert_eq!(summary.reviewed_commits, 0);
        assert_eq!(summary.tokens_in.total, 0);
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn test_persist_and_load_commit_review_data_resets_and_prefers_reviewed() {
        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let sha = "1111111111111111111111111111111111111111";
        persist_benchmark_patch(
            &db,
            sha,
            sha,
            "net: fix race",
            "Author <a@b.c>",
            "diff --git a/foo.c b/foo.c\n",
            "body",
            None,
            1_700_000_000,
            1,
            1,
        )
        .await
        .unwrap();

        let (patch_id, patchset_id, _, _, _) = lookup_benchmark_patch_row(&db, sha)
            .await
            .unwrap()
            .expect("patch row should exist");

        // Case 1: Patchset fails before any review row is created (e.g., Failed To Apply),
        // while patches.status is still Pending. load_commit_review_data should report
        // patchset-level status ("Failed To Apply").
        db.execute(
            "UPDATE patchsets SET status = 'Failed To Apply', failed_reason = 'conflict' WHERE id = ?",
            libsql::params![patchset_id],
        )
        .await
        .unwrap();
        let data_before_review = load_commit_review_data(&db, sha).await.unwrap();
        assert_eq!(
            data_before_review.review_status.as_deref(),
            Some("Failed To Apply")
        );

        // Case 2: Patchset is Cancelled with a Failed review row. Re-running
        // persist_benchmark_patch must delete the Failed review and reset both
        // patch and patchset out of Cancelled back to Pending.
        let failed_rev_id = db
            .create_review(patchset_id, Some(patch_id), "gemini", "pro", None, None)
            .await
            .unwrap();
        db.execute(
            "UPDATE reviews SET status = 'Failed' WHERE id = ?",
            libsql::params![failed_rev_id],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patchsets SET status = 'Cancelled' WHERE id = ?",
            libsql::params![patchset_id],
        )
        .await
        .unwrap();

        persist_benchmark_patch(
            &db,
            sha,
            sha,
            "net: fix race",
            "Author <a@b.c>",
            "diff --git a/foo.c b/foo.c\n",
            "body",
            None,
            1_700_000_000,
            1,
            1,
        )
        .await
        .unwrap();

        let (_, _, patch_status, patchset_status, _) =
            lookup_benchmark_patch_row(&db, sha).await.unwrap().unwrap();
        assert_eq!(patch_status.as_deref(), Some("Pending"));
        assert_eq!(patchset_status.as_deref(), Some("Pending"));

        // Case 3: When both an older Reviewed row and a newer Failed row exist,
        // load_commit_review_data prefers the Reviewed row, and persist_benchmark_patch
        // cleans up the newer non-Reviewed row while keeping the commit Reviewed.
        let reviewed_id = db
            .create_review(patchset_id, Some(patch_id), "gemini", "pro", None, None)
            .await
            .unwrap();
        db.execute(
            "UPDATE reviews SET status = 'Reviewed', summary = 'Found issue' WHERE id = ?",
            libsql::params![reviewed_id],
        )
        .await
        .unwrap();
        let newer_failed_id = db
            .create_review(patchset_id, Some(patch_id), "gemini", "pro", None, None)
            .await
            .unwrap();
        db.execute(
            "UPDATE reviews SET status = 'Failed', summary = 'Transient error' WHERE id = ?",
            libsql::params![newer_failed_id],
        )
        .await
        .unwrap();

        let loaded = load_commit_review_data(&db, sha).await.unwrap();
        assert_eq!(loaded.review_id, Some(reviewed_id));
        assert_eq!(loaded.review_status.as_deref(), Some("Reviewed"));

        persist_benchmark_patch(
            &db,
            sha,
            sha,
            "net: fix race",
            "Author <a@b.c>",
            "diff --git a/foo.c b/foo.c\n",
            "body",
            None,
            1_700_000_000,
            1,
            1,
        )
        .await
        .unwrap();

        let mut remaining = db
            .conn
            .query(
                "SELECT id, status FROM reviews WHERE patch_id = ?",
                libsql::params![patch_id],
            )
            .await
            .unwrap();
        let row = remaining.next().await.unwrap().expect("Reviewed row kept");
        assert_eq!(row.get::<i64>(0).unwrap(), reviewed_id);
        assert_eq!(row.get::<String>(1).unwrap(), "Reviewed");
        assert!(remaining.next().await.unwrap().is_none());
    }

    #[test]
    fn test_load_mbox_archive_gzip_and_path_traversal_guard() -> Result<()> {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let temp = tempdir()?;
        let suite_dir = temp.path();

        assert!(resolve_benchmark_relative_path(suite_dir, "").is_err());
        assert!(resolve_benchmark_relative_path(suite_dir, "../escape.mbox").is_err());
        assert!(resolve_benchmark_relative_path(suite_dir, "/etc/passwd").is_err());
        assert_eq!(
            resolve_benchmark_relative_path(suite_dir, "fixtures/v1.mbox")?,
            suite_dir.join("fixtures/v1.mbox")
        );

        let archive_path = suite_dir.join("versioned_mboxes.jsonl.gz");
        let file = File::create(&archive_path)?;
        let mut gz = GzEncoder::new(file, Compression::default());
        writeln!(
            gz,
            "{}",
            serde_json::json!({
                "key": "./case01/v1.mbox",
                "mbox": "From mboxrd@z Thu Jan  1 00:00:00 1970\nSubject: [PATCH] foo\n"
            })
        )?;
        writeln!(
            gz,
            "{}",
            serde_json::json!({
                "path": "case01/v2.mbox",
                "content": "From mboxrd@z Thu Jan  1 00:00:00 1970\nSubject: [PATCH v2] foo\n"
            })
        )?;
        gz.finish()?;

        let map = load_mbox_archive(&archive_path)?;
        assert_eq!(map.len(), 2);
        assert!(map.get("case01/v1.mbox").unwrap().contains("[PATCH] foo"));
        assert!(
            map.get("case01/v2.mbox")
                .unwrap()
                .contains("[PATCH v2] foo")
        );
        Ok(())
    }

    fn test_run_metadata(total_entries: usize) -> BenchmarkRunMetadata {
        BenchmarkRunMetadata {
            run_id: "r".to_string(),
            started_at: String::new(),
            finished_at: String::new(),
            wall_clock_secs: 0,
            sashiko_version: String::new(),
            sashiko_git_sha: String::new(),
            sashiko_git_dirty: false,
            prompts_git_hash: String::new(),
            benchmark_file: String::new(),
            total_entries,
            unique_commits: total_entries,
            project: "linux".to_string(),
            ai_provider: String::new(),
            model: String::new(),
            effort: None,
            concurrency: 1,
            report_preexisting: false,
            analyze_only: true,
            database_path: String::new(),
            results_json_path: String::new(),
            summary_md_path: String::new(),
        }
    }

    fn attribution_entry(reviewer: &str, version: u32, message_ids: &[&str]) -> BenchmarkEntry {
        BenchmarkEntry {
            commit: "v3-target@example.com".to_string(),
            case_id: Some("attr_case".to_string()),
            category: Some("ignored_prior_bug".to_string()),
            problem_description: Some("Leak on error path".to_string()),
            expected_behavior: BenchmarkExpectedBehavior::DetectWithAttribution,
            expected_reviewer: Some(reviewer.to_string()),
            expected_prior_version: Some(version),
            expected_prior_message_ids: message_ids.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn finding_with_attribution(attribution: Option<Value>) -> BenchmarkFindingRecord {
        BenchmarkFindingRecord {
            index: 1,
            severity: "High".to_string(),
            problem: "skb leak on foo_init() error".to_string(),
            prior_attribution: attribution,
            ..Default::default()
        }
    }

    #[test]
    fn test_judge_regexes_compile() {
        assert!(JUDGE_STATUS_RE.is_match("detected"));
        assert!(FINDING_TAG_RE.is_match("[F12]"));
        assert_eq!(extract_finding_tag_index("see [F3] and [F4]"), Some(3));
    }

    #[test]
    fn test_score_attribution_correct_requires_identity_version_and_message_id() {
        let entry = attribution_entry(
            "Eric Dumazet <edumazet@google.com>",
            1,
            &["20250101.abc@google.com", "20250102.def@google.com"],
        );
        let correct = serde_json::json!({
            "originally_raised_by": "Eric Dumazet",
            "originally_raised_in_version": 1,
            "origin_kind": "human",
            "raised_by_identities": ["EDumazet@Google.com"],
            "evidence": [
                { "source_ref": "HM1.2", "version": 1, "message_id": "<20250102.def@google.com>", "quote": "this leaks the skb when foo_init() fails" }
            ]
        });
        let f = finding_with_attribution(Some(correct.clone()));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Correct)
        );

        // Wrong version.
        let mut v2 = correct.clone();
        v2["originally_raised_in_version"] = serde_json::json!(2);
        let f = finding_with_attribution(Some(v2));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );

        // Wrong person (same domain, different mailbox).
        let mut other = correct.clone();
        other["raised_by_identities"] = serde_json::json!(["kuba@google.com"]);
        let f = finding_with_attribution(Some(other));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );

        // Evidence cites a message outside the accepted set.
        let mut wrong_msg = correct.clone();
        wrong_msg["evidence"][0]["message_id"] = serde_json::json!("20250103.xyz@google.com");
        let f = finding_with_attribution(Some(wrong_msg));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );

        // Evidence without any message id cannot be verified.
        let mut no_msg = correct;
        no_msg["evidence"][0]
            .as_object_mut()
            .map(|o| o.remove("message_id"));
        let f = finding_with_attribution(Some(no_msg));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );
    }

    #[test]
    fn test_score_attribution_ignores_names_and_free_text() {
        // "Eric" is a substring of "generic", and the old scorer accepted any
        // 4+ character name token anywhere in the review text.
        let entry = attribution_entry(
            "Eric Dumazet <edumazet@google.com>",
            1,
            &["20250101.abc@google.com"],
        );
        let mut f = finding_with_attribution(None);
        f.problem = "generic error path leaks the skb; Eric Dumazet noted this on v1".to_string();
        f.explanation = Some("edumazet@google.com raised this in version 1".to_string());
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Missing)
        );

        // Display name only, without structured identities, is not credit.
        let legacy = serde_json::json!({
            "originally_raised_by": "Eric Dumazet <edumazet@google.com>",
            "originally_raised_in_version": 1,
            "origin_kind": "human",
            "reference_url": "https://lore.kernel.org/all/20250101.abc@google.com/"
        });
        let f = finding_with_attribution(Some(legacy));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );

        // Unparseable attribution (version as a string) is wrong, not correct.
        let garbage = serde_json::json!({
            "originally_raised_in_version": "v1",
            "raised_by_identities": ["edumazet@google.com"],
            "evidence": [{ "message_id": "20250101.abc@google.com" }]
        });
        let f = finding_with_attribution(Some(garbage));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );
    }

    #[test]
    fn test_score_attribution_version_is_exact_not_substring() {
        // "v1" is a substring of "v10"; versions compare as integers.
        let entry = attribution_entry("Jan Kara <jack@suse.cz>", 1, &["m1@suse.cz"]);
        let v10 = serde_json::json!({
            "originally_raised_in_version": 10,
            "raised_by_identities": ["jack@suse.cz"],
            "evidence": [{ "source_ref": "HM1.1", "version": 10, "message_id": "m1@suse.cz", "quote": "q" }]
        });
        let f = finding_with_attribution(Some(v10));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );

        let entry10 = attribution_entry("Jan Kara <jack@suse.cz>", 10, &["m1@suse.cz"]);
        let v1 = serde_json::json!({
            "originally_raised_in_version": 1,
            "raised_by_identities": ["jack@suse.cz"],
            "evidence": [{ "source_ref": "HM1.1", "version": 1, "message_id": "m1@suse.cz", "quote": "q" }]
        });
        let f = finding_with_attribution(Some(v1));
        assert_eq!(
            score_attribution(&entry10, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );
    }

    #[test]
    fn test_score_attribution_lkp_and_sashiko_identities() {
        // The old scorer credited lkp whenever "kernel" or "test" appeared.
        let entry = attribution_entry(
            "kernel test robot <lkp@intel.com>",
            1,
            &["202501010101.AbCdEf-lkp@intel.com"],
        );
        let tokens_only = serde_json::json!({
            "originally_raised_by": "kernel test robot",
            "originally_raised_in_version": 1,
            "raised_by_identities": ["kernel test robot"],
            "evidence": [{ "message_id": "202501010101.AbCdEf-lkp@intel.com" }]
        });
        let f = finding_with_attribution(Some(tokens_only));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );

        let exact = serde_json::json!({
            "originally_raised_in_version": 1,
            "raised_by_identities": ["lkp@intel.com"],
            "evidence": [{ "source_ref": "HM1.3", "version": 1, "message_id": "202501010101.AbCdEf-lkp@intel.com", "quote": "q" }]
        });
        let f = finding_with_attribution(Some(exact));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Correct)
        );

        // Message-IDs compare case-sensitively in their local part.
        let lowered = serde_json::json!({
            "originally_raised_in_version": 1,
            "raised_by_identities": ["lkp@intel.com"],
            "evidence": [{ "message_id": "202501010101.abcdef-lkp@intel.com" }]
        });
        let f = finding_with_attribution(Some(lowered));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );

        let sashiko = attribution_entry("Sashiko", 1, &["v1-p1@example.com"]);
        let pf = serde_json::json!({
            "originally_raised_by": "Sashiko",
            "originally_raised_in_version": 1,
            "origin_kind": "sashiko_review",
            "raised_by_identities": ["sashiko"],
            "evidence": [{ "source_ref": "PF1", "version": 1, "message_id": "v1-p1@example.com", "quote": "q" }]
        });
        let f = finding_with_attribution(Some(pf));
        assert_eq!(
            score_attribution(&sashiko, Some(&f)),
            Some(AttributionOutcome::Correct)
        );
    }

    #[test]
    fn test_score_attribution_accepts_co_reviewers_only_when_listed() {
        let mut entry = attribution_entry(
            "Gary Guo <gary@garyguo.net>",
            1,
            &["g1@garyguo.net", "b1@tardis.local"],
        );
        let boqun = serde_json::json!({
            "originally_raised_in_version": 1,
            "raised_by_identities": ["boqun@kernel.org"],
            "evidence": [{ "source_ref": "HM1.4", "version": 1, "message_id": "b1@tardis.local", "quote": "q" }]
        });
        let f = finding_with_attribution(Some(boqun));
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );
        entry.expected_co_reviewers = vec!["Boqun Feng <boqun@kernel.org>".to_string()];
        assert!(entry.validate_attribution_labels().is_ok());
        assert_eq!(
            score_attribution(&entry, Some(&f)),
            Some(AttributionOutcome::Correct)
        );

        let bad = BenchmarkEntry {
            expected_co_reviewers: vec!["Boqun".to_string()],
            ..entry.clone()
        };
        assert!(bad.validate_attribution_labels().is_err());
        let not_attr = BenchmarkEntry {
            expected_behavior: BenchmarkExpectedBehavior::DetectRegression,
            expected_prior_message_ids: vec![],
            ..entry
        };
        assert!(not_attr.validate_attribution_labels().is_err());
    }

    fn acceptable(reviewer: &str, version: u32, ids: &[&str]) -> AcceptableAttribution {
        AcceptableAttribution {
            reviewer: reviewer.to_string(),
            version,
            message_ids: ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn versioned_detect_bug(acceptable: Vec<AcceptableAttribution>) -> BenchmarkEntry {
        BenchmarkEntry {
            commit: "v3-target@example.com".to_string(),
            case_id: Some("acc_case".to_string()),
            expected_behavior: BenchmarkExpectedBehavior::DetectBug,
            ancestor_mboxes: [1, 2]
                .into_iter()
                .map(|v| BenchmarkAncestorMbox {
                    version: v,
                    mbox_path: format!("acc_case/v{v}.mbox"),
                    baseline_commit: None,
                    patchset_status: BenchmarkAncestorStatus::Reviewed,
                    prior_findings: vec![],
                })
                .collect(),
            target_mbox: Some("acc_case/target.mbox".to_string()),
            acceptable_attributions: acceptable,
            ..Default::default()
        }
    }

    #[test]
    fn test_score_attribution_acceptable_paths() {
        let entry = versioned_detect_bug(vec![acceptable(
            "Jan Kara <jack@suse.cz>",
            1,
            &["jan-v1@suse.cz"],
        )]);
        assert!(entry.validate_attribution_labels().is_ok());
        let score = |v: Value| score_attribution(&entry, Some(&finding_with_attribution(Some(v))));

        // No attribution on a detect_bug match is fine.
        assert_eq!(
            score_attribution(&entry, Some(&finding_with_attribution(None))),
            None
        );

        // Identity, version and evidence all match the acceptable item.
        let ok = serde_json::json!({
            "originally_raised_in_version": 1,
            "raised_by_identities": ["Jack@SUSE.cz"],
            "evidence": [{ "source_ref": "HM1.6", "version": 1, "message_id": "<jan-v1@suse.cz>", "quote": "q" }]
        });
        assert_eq!(score(ok.clone()), Some(AttributionOutcome::Acceptable));

        // Without evidence, identity and version suffice.
        let mut no_ev = ok.clone();
        no_ev["evidence"] = serde_json::json!([]);
        assert_eq!(score(no_ev), Some(AttributionOutcome::Acceptable));

        // Wrong version.
        let mut v2 = ok.clone();
        v2["originally_raised_in_version"] = serde_json::json!(2);
        assert_eq!(score(v2), Some(AttributionOutcome::Wrong));

        // Evidence present but outside the acceptable message ids.
        let mut other_msg = ok.clone();
        other_msg["evidence"][0]["message_id"] = serde_json::json!("other@suse.cz");
        assert_eq!(score(other_msg), Some(AttributionOutcome::Wrong));

        // An extra identity that no acceptable item covers.
        let mut extra = ok.clone();
        extra["raised_by_identities"] = serde_json::json!(["jack@suse.cz", "tytso@mit.edu"]);
        assert_eq!(score(extra), Some(AttributionOutcome::Wrong));

        // No identities at all is never acceptable.
        let mut none = ok;
        none["raised_by_identities"] = serde_json::json!([]);
        assert_eq!(score(none), Some(AttributionOutcome::Wrong));

        // On a detect_with_attribution entry, the required credit stays
        // correct, an acceptable alternative is acceptable, anything else wrong.
        let mut required = attribution_entry("Gary Guo <gary@garyguo.net>", 1, &["g1@garyguo.net"]);
        required.acceptable_attributions = vec![acceptable(
            "Boqun Feng <boqun@kernel.org>",
            2,
            &["b2@tardis.local"],
        )];
        let gary = serde_json::json!({
            "originally_raised_in_version": 1,
            "raised_by_identities": ["gary@garyguo.net"],
            "evidence": [{ "message_id": "g1@garyguo.net" }]
        });
        let boqun = serde_json::json!({
            "originally_raised_in_version": 2,
            "raised_by_identities": ["boqun@kernel.org"],
            "evidence": [{ "message_id": "b2@tardis.local" }]
        });
        let f = |v: Value| finding_with_attribution(Some(v));
        assert_eq!(
            score_attribution(&required, Some(&f(gary))),
            Some(AttributionOutcome::Correct)
        );
        assert_eq!(
            score_attribution(&required, Some(&f(boqun))),
            Some(AttributionOutcome::Acceptable)
        );
    }

    #[test]
    fn test_validate_acceptable_attributions() {
        let ok = versioned_detect_bug(vec![acceptable(
            "Jan Kara <jack@suse.cz>",
            1,
            &["m@suse.cz"],
        )]);
        assert!(ok.validate_attribution_labels().is_ok());

        let bad_version = versioned_detect_bug(vec![acceptable(
            "Jan Kara <jack@suse.cz>",
            3,
            &["m@suse.cz"],
        )]);
        assert!(bad_version.validate_attribution_labels().is_err());

        let no_ids = versioned_detect_bug(vec![acceptable("Jan Kara <jack@suse.cz>", 1, &[])]);
        assert!(no_ids.validate_attribution_labels().is_err());

        let brackets = versioned_detect_bug(vec![acceptable(
            "Jan Kara <jack@suse.cz>",
            1,
            &["<m@suse.cz>"],
        )]);
        assert!(brackets.validate_attribution_labels().is_err());

        let name_only = versioned_detect_bug(vec![acceptable("Jan Kara", 1, &["m@suse.cz"])]);
        assert!(name_only.validate_attribution_labels().is_err());

        for behavior in [
            BenchmarkExpectedBehavior::DetectRegression,
            BenchmarkExpectedBehavior::SuppressFalsePositive,
        ] {
            let e = BenchmarkEntry {
                expected_behavior: behavior,
                ..ok.clone()
            };
            assert!(e.validate_attribution_labels().is_err());
        }
    }

    #[test]
    fn test_acceptable_attribution_summary_is_neutral() {
        let mk = |commit: &str, behavior, attribution| {
            let entry = BenchmarkEntry {
                commit: commit.to_string(),
                category: Some("ignored_prior_bug".to_string()),
                problem_description: Some("bug".to_string()),
                expected_behavior: behavior,
                expected_reviewer: (behavior == BenchmarkExpectedBehavior::DetectWithAttribution)
                    .then(|| "Jan Kara <jack@suse.cz>".to_string()),
                expected_prior_version: (behavior
                    == BenchmarkExpectedBehavior::DetectWithAttribution)
                    .then_some(1),
                target_mbox: Some("c/target.mbox".to_string()),
                ..Default::default()
            };
            let data = CommitReviewData {
                review_id: Some(1),
                review_status: Some("Reviewed".to_string()),
                findings: vec![finding_with_attribution(None)],
                ..Default::default()
            };
            let res = build_entry_result_from_data(
                &entry,
                "bug",
                &data,
                "DETECTED".to_string(),
                attribution,
                String::new(),
                None,
                None,
                None,
            );
            (entry, data, res)
        };
        let cases = [
            mk(
                "a",
                BenchmarkExpectedBehavior::DetectBug,
                Some(AttributionOutcome::Acceptable),
            ),
            mk("b", BenchmarkExpectedBehavior::DetectBug, None),
            mk(
                "c",
                BenchmarkExpectedBehavior::DetectWithAttribution,
                Some(AttributionOutcome::Correct),
            ),
            mk(
                "d",
                BenchmarkExpectedBehavior::DetectWithAttribution,
                Some(AttributionOutcome::Acceptable),
            ),
        ];
        let entries: Vec<BenchmarkEntry> = cases.iter().map(|c| c.0.clone()).collect();
        let results: Vec<BenchmarkEntryResult> = cases.iter().map(|c| c.2.clone()).collect();
        let commit_map: HashMap<String, CommitReviewData> = cases
            .iter()
            .map(|c| (c.0.commit.clone(), c.1.clone()))
            .collect();
        let s = build_benchmark_summary(&entries, &results, &commit_map);
        assert_eq!(s.attribution_acceptable, 2);
        assert_eq!(s.attribution_expected_entries, 2);
        assert_eq!(s.attribution_scored_entries, 1);
        assert_eq!(s.attributed, 1);
        assert_eq!(s.attribution_rate_pct, 100.0);
        assert_eq!(s.attribution_wrong, 0);
        assert_eq!(s.attribution_missing, 0);
        assert_eq!(s.unexpected_attributions, 0);
        assert_eq!(s.attribution_precision_pct, 100.0);
        let cat = &s.by_category[0];
        assert_eq!(cat.attribution_acceptable, 2);
        // Both detect_bug entries pass; the required-credit entry with only
        // acceptable credit does not.
        assert_eq!(cat.passed, 3);
    }

    #[test]
    fn test_score_attribution_on_entries_without_expected_credit() {
        let detect_bug = BenchmarkEntry {
            commit: "c@example.com".to_string(),
            expected_behavior: BenchmarkExpectedBehavior::DetectBug,
            ..Default::default()
        };
        let any = serde_json::json!({
            "originally_raised_in_version": 1,
            "raised_by_identities": ["someone@example.com"],
            "evidence": [{ "message_id": "x@example.com" }]
        });
        let f = finding_with_attribution(Some(any.clone()));
        assert_eq!(
            score_attribution(&detect_bug, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );
        let plain = finding_with_attribution(None);
        assert_eq!(score_attribution(&detect_bug, Some(&plain)), None);
        assert_eq!(score_attribution(&detect_bug, None), None);

        let regression = BenchmarkEntry {
            expected_behavior: BenchmarkExpectedBehavior::DetectRegression,
            ..detect_bug.clone()
        };
        assert_eq!(
            score_attribution(&regression, Some(&f)),
            Some(AttributionOutcome::Wrong)
        );

        let suppression = BenchmarkEntry {
            expected_behavior: BenchmarkExpectedBehavior::SuppressFalsePositive,
            ..detect_bug
        };
        assert_eq!(score_attribution(&suppression, Some(&f)), None);

        // No matched finding on an attribution entry means missing credit.
        let entry = attribution_entry("Sashiko", 1, &["v1-p1@example.com"]);
        assert_eq!(
            score_attribution(&entry, None),
            Some(AttributionOutcome::Missing)
        );
    }

    #[test]
    fn test_validate_attribution_labels() {
        let ok = attribution_entry("Jan Kara <jack@suse.cz>", 1, &["m1@suse.cz"]);
        assert!(ok.validate_attribution_labels().is_ok());

        let no_ids = attribution_entry("Jan Kara <jack@suse.cz>", 1, &[]);
        assert!(no_ids.validate_attribution_labels().is_err());

        let brackets = attribution_entry("Jan Kara <jack@suse.cz>", 1, &["<m1@suse.cz>"]);
        assert!(brackets.validate_attribution_labels().is_err());

        let dupes = attribution_entry("Jan Kara <jack@suse.cz>", 1, &["m1@suse.cz", "m1@suse.cz"]);
        assert!(dupes.validate_attribution_labels().is_err());

        let name_only = attribution_entry("Jan Kara", 1, &["m1@suse.cz"]);
        assert!(name_only.validate_attribution_labels().is_err());

        let mut no_version = ok.clone();
        no_version.expected_prior_version = None;
        assert!(no_version.validate_attribution_labels().is_err());

        let mut not_an_ancestor = ok.clone();
        not_an_ancestor.ancestor_mboxes = vec![BenchmarkAncestorMbox {
            version: 2,
            mbox_path: "c/v2.mbox".to_string(),
            baseline_commit: None,
            patchset_status: BenchmarkAncestorStatus::Reviewed,
            prior_findings: vec![],
        }];
        assert!(not_an_ancestor.validate_attribution_labels().is_err());

        let detect_bug_with_reviewer = BenchmarkEntry {
            expected_behavior: BenchmarkExpectedBehavior::DetectBug,
            expected_prior_message_ids: vec![],
            ..ok.clone()
        };
        assert!(
            detect_bug_with_reviewer
                .validate_attribution_labels()
                .is_err()
        );

        let suppression_with_ids = BenchmarkEntry {
            expected_behavior: BenchmarkExpectedBehavior::SuppressFalsePositive,
            ..ok
        };
        assert!(suppression_with_ids.validate_attribution_labels().is_err());

        let classic = BenchmarkEntry {
            commit: "abc".to_string(),
            ..Default::default()
        };
        assert!(validate_benchmark_entries(&[classic]).is_ok());
    }

    #[test]
    fn test_attribution_and_suppression_summary_metrics() {
        let entry_attr_ok = BenchmarkEntry {
            commit: "v2-msg-1@example.com".to_string(),
            case_id: Some("cat1_01".to_string()),
            category: Some("ignored_prior_bug".to_string()),
            subsystem: Some("net".to_string()),
            problem_description: Some("Leak on error path".to_string()),
            expected_behavior: BenchmarkExpectedBehavior::DetectWithAttribution,
            expected_reviewer: Some("Jakub Kicinski <kuba@kernel.org>".to_string()),
            expected_prior_version: Some(1),
            expected_prior_message_ids: vec!["v1-reply@kernel.org".to_string()],
            target_mbox: Some("case/target.mbox".to_string()),
            ..Default::default()
        };
        let entry_attr_missing = BenchmarkEntry {
            commit: "v2-msg-2@example.com".to_string(),
            case_id: Some("cat1_02".to_string()),
            category: Some("ignored_prior_bug".to_string()),
            subsystem: Some("net".to_string()),
            problem_description: Some("Missing unlock".to_string()),
            expected_behavior: BenchmarkExpectedBehavior::DetectWithAttribution,
            expected_reviewer: Some("Eric Dumazet <edumazet@google.com>".to_string()),
            expected_prior_version: Some(2),
            expected_prior_message_ids: vec!["v2-reply@google.com".to_string()],
            target_mbox: Some("case/target.mbox".to_string()),
            ..Default::default()
        };
        let entry_attr_unreviewed = BenchmarkEntry {
            commit: "v2-msg-5@example.com".to_string(),
            case_id: Some("cat1_03".to_string()),
            category: Some("ignored_prior_bug".to_string()),
            problem_description: Some("Double free".to_string()),
            ..entry_attr_missing.clone()
        };
        let entry_bug_partial = BenchmarkEntry {
            commit: "v2-msg-6@example.com".to_string(),
            case_id: Some("cat1_04".to_string()),
            category: Some("ignored_prior_bug".to_string()),
            problem_description: Some("Refcount leak".to_string()),
            expected_behavior: BenchmarkExpectedBehavior::DetectBug,
            target_mbox: Some("case/target.mbox".to_string()),
            ..Default::default()
        };
        let entry_suppressed = BenchmarkEntry {
            commit: "v2-msg-3@example.com".to_string(),
            case_id: Some("cat3_01".to_string()),
            category: Some("wrong_claim_rejected".to_string()),
            subsystem: Some("mm".to_string()),
            problem_description: Some("False claim about folio refcount".to_string()),
            expected_behavior: BenchmarkExpectedBehavior::SuppressFalsePositive,
            target_mbox: Some("case/target.mbox".to_string()),
            ..Default::default()
        };
        let entry_fp = BenchmarkEntry {
            commit: "v2-msg-4@example.com".to_string(),
            case_id: Some("cat3_02".to_string()),
            category: Some("wrong_claim_rejected".to_string()),
            subsystem: Some("mm".to_string()),
            problem_description: Some("False claim about page lock".to_string()),
            expected_behavior: BenchmarkExpectedBehavior::SuppressFalsePositive,
            target_mbox: Some("case/target.mbox".to_string()),
            ..Default::default()
        };
        let entry_supp_unknown = BenchmarkEntry {
            commit: "v2-msg-7@example.com".to_string(),
            case_id: Some("cat3_03".to_string()),
            ..entry_fp.clone()
        };

        let matched = |problem: &str| MatchedFindingSummary {
            index: 1,
            severity: "High".to_string(),
            problem: problem.to_string(),
            preexisting: false,
            origin: None,
            stages: vec!["prior-feedback".to_string()],
            prompts: vec![],
        };
        let reviewed = |id: i64, findings: Vec<BenchmarkFindingRecord>| CommitReviewData {
            review_id: Some(id),
            review_status: Some("Reviewed".to_string()),
            findings,
            ..Default::default()
        };

        let data_attr_ok = reviewed(1, vec![finding_with_attribution(None)]);
        let data_attr_missing = reviewed(2, vec![finding_with_attribution(None)]);
        let data_suppressed = reviewed(3, vec![]);
        let data_fp = reviewed(4, vec![finding_with_attribution(None)]);
        let data_unreviewed = CommitReviewData {
            review_id: Some(5),
            review_status: Some("Failed".to_string()),
            ..Default::default()
        };
        let data_partial = reviewed(6, vec![finding_with_attribution(None)]);
        let data_supp_unknown = reviewed(7, vec![finding_with_attribution(None)]);

        let entries = vec![
            entry_attr_ok.clone(),
            entry_attr_missing.clone(),
            entry_attr_unreviewed.clone(),
            entry_bug_partial.clone(),
            entry_suppressed.clone(),
            entry_fp.clone(),
            entry_supp_unknown.clone(),
        ];
        let mut commit_map = HashMap::new();
        for (e, d) in [
            (&entry_attr_ok, &data_attr_ok),
            (&entry_attr_missing, &data_attr_missing),
            (&entry_attr_unreviewed, &data_unreviewed),
            (&entry_bug_partial, &data_partial),
            (&entry_suppressed, &data_suppressed),
            (&entry_fp, &data_fp),
            (&entry_supp_unknown, &data_supp_unknown),
        ] {
            commit_map.insert(e.commit.clone(), d.clone());
        }

        let mut suppressed_result = build_entry_result_from_data(
            &entry_suppressed,
            "False claim about folio refcount",
            &data_suppressed,
            "SUPPRESSED".to_string(),
            None,
            "Zero findings reported".to_string(),
            None,
            None,
            None,
        );
        suppressed_result.suppressed_with_no_findings = true;

        let results = vec![
            build_entry_result_from_data(
                &entry_attr_ok,
                "Leak on error path",
                &data_attr_ok,
                "DETECTED".to_string(),
                Some(AttributionOutcome::Correct),
                "Detected with v1 attribution".to_string(),
                Some(matched("skb leak on foo_init() error")),
                None,
                None,
            ),
            build_entry_result_from_data(
                &entry_attr_missing,
                "Missing unlock",
                &data_attr_missing,
                "DETECTED".to_string(),
                Some(AttributionOutcome::Wrong),
                "Detected but credited the wrong reviewer".to_string(),
                Some(matched("Missing spin_unlock()")),
                None,
                None,
            ),
            build_entry_result_from_data(
                &entry_attr_unreviewed,
                "Double free",
                &data_unreviewed,
                "NOT_REVIEWED".to_string(),
                None,
                "Failed".to_string(),
                None,
                Some("review_failed".to_string()),
                None,
            ),
            build_entry_result_from_data(
                &entry_bug_partial,
                "Refcount leak",
                &data_partial,
                "PARTIALLY_DETECTED".to_string(),
                Some(AttributionOutcome::Wrong),
                "Partial match with spurious credit".to_string(),
                Some(matched("refcount issue")),
                None,
                None,
            ),
            suppressed_result,
            build_entry_result_from_data(
                &entry_fp,
                "False claim about page lock",
                &data_fp,
                "FALSE_POSITIVE".to_string(),
                None,
                "False positive [F1] matched safe claim".to_string(),
                Some(matched("Bogus page lock warning")),
                None,
                None,
            ),
            build_entry_result_from_data(
                &entry_supp_unknown,
                "False claim about page lock",
                &data_supp_unknown,
                "UNKNOWN".to_string(),
                None,
                "Evaluation failed".to_string(),
                None,
                None,
                None,
            ),
        ];
        assert_eq!(results[2].attribution, Some(AttributionOutcome::Missing));
        assert_eq!(results[2].attributed, Some(false));

        let summary = build_benchmark_summary(&entries, &results, &commit_map);
        // Unreviewed detection entries stay in the denominator as failures.
        assert_eq!(summary.evaluated_entries, 4);
        assert_eq!(summary.detected, 2);
        assert_eq!(summary.partially_detected, 1);
        assert_eq!(summary.exact_detection_rate_pct, 50.0);
        assert_eq!(summary.total_detection_rate_pct, 75.0);
        assert_eq!(summary.not_reviewed, 1);
        assert_eq!(summary.evaluation_failed, 1);
        assert_eq!(summary.attribution_expected_entries, 3);
        assert_eq!(summary.attributed, 1);
        assert_eq!(summary.attribution_wrong, 1);
        assert_eq!(summary.attribution_missing, 1);
        assert_eq!(summary.unexpected_attributions, 1);
        assert_eq!(summary.attribution_rate_pct, 33.3);
        assert_eq!(summary.attribution_precision_pct, 33.3);
        // The judge failure on a suppression entry is a failure, not a pass.
        assert_eq!(summary.suppression_evaluated_entries, 3);
        assert_eq!(summary.suppressed, 1);
        assert_eq!(summary.suppressed_with_no_findings, 1);
        assert_eq!(summary.false_positives, 1);
        assert_eq!(summary.false_positive_suppression_rate_pct, 33.3);

        assert_eq!(summary.by_category.len(), 2);
        let cat1 = &summary.by_category[0];
        assert_eq!(cat1.category, "ignored_prior_bug");
        assert_eq!(cat1.total, 4);
        // Only the exact detection with correct credit passes; the partial
        // match and the wrongly credited detection do not.
        assert_eq!(cat1.passed, 1);
        assert_eq!(cat1.failed, 1);
        assert_eq!(cat1.attribution_wrong, 2);
        assert_eq!(cat1.attribution_missing, 1);
        let cat3 = &summary.by_category[1];
        assert_eq!(cat3.total, 3);
        assert_eq!(cat3.passed, 1);
        assert_eq!(cat3.failed, 1);

        let report = BenchmarkRunReport {
            metadata: test_run_metadata(entries.len()),
            summary,
            results,
        };
        let md = format_benchmark_summary_markdown(&report);
        assert!(md.contains("| Attribution Wrong | 1 |"));
        assert!(md.contains("| Attribution Missing | 1 |"));
        assert!(md.contains("| Suppressed With Zero Findings (vacuous pass) | 1 |"));
        assert!(md.contains("DETECTED (attribution: wrong)"));
    }

    /// Classic (non-versioned) suites keep the original metric semantics:
    /// unreviewed and unevaluated entries stay out of the detection
    /// denominator, and partial matches count toward the total rate.
    #[test]
    fn test_classic_summary_semantics_unchanged() {
        let mk = |commit: &str| BenchmarkEntry {
            commit: commit.to_string(),
            subsystem: Some("mm".to_string()),
            problem_description: Some("bug".to_string()),
            ..Default::default()
        };
        let entries: Vec<BenchmarkEntry> = ["c1", "c2", "c3", "c4", "c5", "c6", "c7"]
            .iter()
            .map(|c| mk(c))
            .collect();
        let reviewed = |id: i64| CommitReviewData {
            review_id: Some(id),
            review_status: Some("Reviewed".to_string()),
            findings: vec![finding_with_attribution(None)],
            ..Default::default()
        };
        let mut commit_map = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            commit_map.insert(e.commit.clone(), reviewed(i as i64 + 1));
        }
        let matched = MatchedFindingSummary {
            index: 1,
            severity: "High".to_string(),
            problem: "p".to_string(),
            preexisting: false,
            origin: None,
            stages: vec!["s".to_string()],
            prompts: vec![],
        };
        let statuses = [
            ("DETECTED", true),
            ("DETECTED", true),
            ("PARTIALLY_DETECTED", true),
            ("MISSED", false),
            ("UNKNOWN", false),
            ("NOT_REVIEWED", false),
            ("SKIPPED", false),
        ];
        let results: Vec<BenchmarkEntryResult> = entries
            .iter()
            .zip(statuses)
            .map(|(e, (status, matched_f))| {
                let data = commit_map.get(&e.commit).cloned().unwrap_or_default();
                build_entry_result_from_data(
                    e,
                    "bug",
                    &data,
                    status.to_string(),
                    None,
                    String::new(),
                    matched_f.then(|| matched.clone()),
                    (status == "NOT_REVIEWED").then(|| "review_failed".to_string()),
                    None,
                )
            })
            .collect();
        assert!(results.iter().all(|r| !r.versioned));

        let s = build_benchmark_summary(&entries, &results, &commit_map);
        assert_eq!(s.evaluated_entries, 4);
        assert_eq!(s.detected, 2);
        assert_eq!(s.partially_detected, 1);
        assert_eq!(s.missed, 1);
        assert_eq!(s.not_reviewed, 1);
        assert_eq!(s.skipped, 1);
        assert_eq!(s.exact_detection_rate_pct, 50.0);
        assert_eq!(s.total_detection_rate_pct, 75.0);
        assert_eq!(s.missed_review_failed, 1);
        assert_eq!(s.missed_not_raised, 1);
        assert_eq!(s.suppression_evaluated_entries, 0);
        assert_eq!(s.attribution_expected_entries, 0);
        assert!(s.by_category.is_empty());

        let report = BenchmarkRunReport {
            metadata: test_run_metadata(entries.len()),
            summary: s,
            results,
        };
        let md = format_benchmark_summary_markdown(&report);
        assert!(md.contains("| **Total Detected** | **3 / 4** | **75.0%** |"));
        assert!(md.contains("| Not Reviewed / Failed | 1 | - |"));
        assert!(!md.contains("scored as failures"));
        assert!(!md.contains("Versioned Feedback Metrics"));
    }

    #[test]
    fn test_judge_prompt_marks_suppression_claim_as_refuted() {
        let entry = BenchmarkEntry {
            commit: "c@example.com".to_string(),
            expected_behavior: BenchmarkExpectedBehavior::SuppressFalsePositive,
            ..Default::default()
        };
        let data = CommitReviewData::default();
        let prompt = build_judge_prompt_for_entry(&entry, "Bogus lock claim", &data);
        assert!(prompt.contains("Refuted Claim (FALSE; must NOT be reported):"));
        assert!(prompt.contains("NOT a real defect"));
        assert!(!prompt.contains("attributed"));

        let detect = BenchmarkEntry {
            expected_behavior: BenchmarkExpectedBehavior::DetectWithAttribution,
            expected_reviewer: Some("Jan Kara <jack@suse.cz>".to_string()),
            ..entry
        };
        let prompt = build_judge_prompt_for_entry(&detect, "Real bug", &data);
        assert!(prompt.contains("Ground Truth Defect:"));
        assert!(!prompt.contains("Refuted Claim"));
        assert!(!prompt.contains("jack@suse.cz"));
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn test_ingest_versioned_benchmark_entries_links_multihop_chain_and_skips_nontarget_parts()
    -> Result<()> {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await?;
        db.migrate().await?;

        let temp = tempdir()?;
        let suite_dir = temp.path();
        let bench_file = suite_dir.join("benchmark_versioned.json");
        let archive_path = suite_dir.join("versioned_mboxes.jsonl.gz");

        let v1_mbox = concat!(
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Alice Author <alice@example.com>\n",
            "To: netdev@vger.kernel.org\n",
            "Subject: [PATCH 0/2] net: add foo driver\n",
            "Date: Mon, 1 Jan 2024 10:00:00 +0000\n",
            "Message-ID: <v1-cover@example.com>\n\n",
            "Cover letter for v1.\n\n",
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Alice Author <alice@example.com>\n",
            "To: netdev@vger.kernel.org\n",
            "Subject: [PATCH 1/2] net: foo: add header\n",
            "Date: Mon, 1 Jan 2024 10:00:01 +0000\n",
            "Message-ID: <v1-p1@example.com>\n",
            "In-Reply-To: <v1-cover@example.com>\n",
            "References: <v1-cover@example.com>\n\n",
            "Add foo header.\n\n",
            "---\n",
            "diff --git a/drivers/net/foo.h b/drivers/net/foo.h\n",
            "new file mode 100644\n",
            "index 0000000..1111111\n",
            "--- /dev/null\n",
            "+++ b/drivers/net/foo.h\n",
            "@@ -0,0 +1,2 @@\n",
            "+int foo_init(void);\n",
            "+void foo_exit(void);\n",
            "-- \n2.45.0\n\n",
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Alice Author <alice@example.com>\n",
            "To: netdev@vger.kernel.org\n",
            "Subject: [PATCH 2/2] net: foo: add core probe\n",
            "Date: Mon, 1 Jan 2024 10:00:02 +0000\n",
            "Message-ID: <v1-p2@example.com>\n",
            "In-Reply-To: <v1-cover@example.com>\n",
            "References: <v1-cover@example.com>\n\n",
            "Add core probe.\n\n",
            "---\n",
            "diff --git a/drivers/net/foo.c b/drivers/net/foo.c\n",
            "new file mode 100644\n",
            "index 0000000..2222222\n",
            "--- /dev/null\n",
            "+++ b/drivers/net/foo.c\n",
            "@@ -0,0 +1,5 @@\n",
            "+int foo_init(void)\n",
            "+{\n",
            "+\treturn 0;\n",
            "+}\n",
            "+void foo_exit(void) {}\n",
            "-- \n2.45.0\n\n",
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Bob Maintainer <bob@kernel.org>\n",
            "To: Alice Author <alice@example.com>\n",
            "Cc: netdev@vger.kernel.org\n",
            "Subject: Re: [PATCH 2/2] net: foo: add core probe\n",
            "Date: Mon, 1 Jan 2024 12:00:00 +0000\n",
            "Message-ID: <v1-reply-bob@kernel.org>\n",
            "In-Reply-To: <v1-p2@example.com>\n",
            "References: <v1-cover@example.com> <v1-p2@example.com>\n\n",
            "foo_init() leaks clk_prepare_enable() when register_netdev() fails.\n\n",
        );

        let v2_mbox = concat!(
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Alice Author <alice@example.com>\n",
            "To: netdev@vger.kernel.org\n",
            "Subject: [PATCH v2 0/2] net: add foo driver\n",
            "Date: Tue, 2 Jan 2024 10:00:00 +0000\n",
            "Message-ID: <v2-cover@example.com>\n\n",
            "v2: fix build warning.\n\n",
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Alice Author <alice@example.com>\n",
            "To: netdev@vger.kernel.org\n",
            "Subject: [PATCH v2 1/2] net: foo: add header\n",
            "Date: Tue, 2 Jan 2024 10:00:01 +0000\n",
            "Message-ID: <v2-p1@example.com>\n",
            "In-Reply-To: <v2-cover@example.com>\n",
            "References: <v2-cover@example.com>\n\n",
            "Add foo header.\n\n",
            "---\n",
            "diff --git a/drivers/net/foo.h b/drivers/net/foo.h\n",
            "new file mode 100644\n",
            "index 0000000..1111111\n",
            "--- /dev/null\n",
            "+++ b/drivers/net/foo.h\n",
            "@@ -0,0 +1,2 @@\n",
            "+int foo_init(void);\n",
            "+void foo_exit(void);\n",
            "-- \n2.45.0\n\n",
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Alice Author <alice@example.com>\n",
            "To: netdev@vger.kernel.org\n",
            "Subject: [PATCH v2 2/2] net: foo: add core probe\n",
            "Date: Tue, 2 Jan 2024 10:00:02 +0000\n",
            "Message-ID: <v2-p2@example.com>\n",
            "In-Reply-To: <v2-cover@example.com>\n",
            "References: <v2-cover@example.com>\n\n",
            "Add core probe.\n\n",
            "---\n",
            "diff --git a/drivers/net/foo.c b/drivers/net/foo.c\n",
            "new file mode 100644\n",
            "index 0000000..2222223\n",
            "--- /dev/null\n",
            "+++ b/drivers/net/foo.c\n",
            "@@ -0,0 +1,6 @@\n",
            "+int foo_init(void)\n",
            "+{\n",
            "+\t/* build fix */\n",
            "+\treturn 0;\n",
            "+}\n",
            "+void foo_exit(void) {}\n",
            "-- \n2.45.0\n\n",
        );

        let v3_mbox = concat!(
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Alice Author <alice@example.com>\n",
            "To: netdev@vger.kernel.org\n",
            "Subject: [PATCH v3 0/2] net: add foo driver\n",
            "Date: Wed, 3 Jan 2024 10:00:00 +0000\n",
            "Message-ID: <v3-cover@example.com>\n\n",
            "v3: addressed review comments.\n\n",
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Alice Author <alice@example.com>\n",
            "To: netdev@vger.kernel.org\n",
            "Subject: [PATCH v3 1/2] net: foo: add header\n",
            "Date: Wed, 3 Jan 2024 10:00:01 +0000\n",
            "Message-ID: <v3-p1@example.com>\n",
            "In-Reply-To: <v3-cover@example.com>\n",
            "References: <v3-cover@example.com>\n\n",
            "Add foo header.\n\n",
            "---\n",
            "diff --git a/drivers/net/foo.h b/drivers/net/foo.h\n",
            "new file mode 100644\n",
            "index 0000000..1111111\n",
            "--- /dev/null\n",
            "+++ b/drivers/net/foo.h\n",
            "@@ -0,0 +1,2 @@\n",
            "+int foo_init(void);\n",
            "+void foo_exit(void);\n",
            "-- \n2.45.0\n\n",
            "From mboxrd@z Thu Jan  1 00:00:00 1970\n",
            "From: Alice Author <alice@example.com>\n",
            "To: netdev@vger.kernel.org\n",
            "Subject: [PATCH v3 2/2] net: foo: add core probe\n",
            "Date: Wed, 3 Jan 2024 10:00:02 +0000\n",
            "Message-ID: <v3-p2@example.com>\n",
            "In-Reply-To: <v3-cover@example.com>\n",
            "References: <v3-cover@example.com>\n\n",
            "Add core probe.\n\n",
            "---\n",
            "diff --git a/drivers/net/foo.c b/drivers/net/foo.c\n",
            "new file mode 100644\n",
            "index 0000000..2222224\n",
            "--- /dev/null\n",
            "+++ b/drivers/net/foo.c\n",
            "@@ -0,0 +1,6 @@\n",
            "+int foo_init(void)\n",
            "+{\n",
            "+\t/* still leaks clk on register_netdev error */\n",
            "+\treturn 0;\n",
            "+}\n",
            "+void foo_exit(void) {}\n",
            "-- \n2.45.0\n\n",
        );

        let archive_file = File::create(&archive_path)?;
        let mut gz = GzEncoder::new(archive_file, Compression::default());
        for (key, mbox) in [
            ("foo/v1.mbox", v1_mbox),
            ("foo/v2.mbox", v2_mbox),
            ("foo/v3.mbox", v3_mbox),
        ] {
            writeln!(gz, "{}", serde_json::json!({ "key": key, "mbox": mbox }))?;
        }
        gz.finish()?;

        let baseline_sha = "1111111111111111111111111111111111111111";
        let entry = BenchmarkEntry {
            commit: "v3-p2@example.com".to_string(),
            case_id: Some("multihop_01".to_string()),
            category: Some("multihop_or_routing".to_string()),
            subsystem: Some("net".to_string()),
            problem_description: Some("clk leak on register_netdev failure".to_string()),
            mbox_archive: Some("versioned_mboxes.jsonl.gz".to_string()),
            ancestor_mboxes: vec![
                BenchmarkAncestorMbox {
                    version: 1,
                    mbox_path: "foo/v1.mbox".to_string(),
                    baseline_commit: Some(baseline_sha.to_string()),
                    patchset_status: BenchmarkAncestorStatus::Reviewed,
                    prior_findings: vec![BenchmarkSeededFinding {
                        patch_message_id: Some("v1-p2@example.com".to_string()),
                        part_index: 2,
                        severity: Severity::High,
                        problem: "Seeded v1 Sashiko finding: clk leak".to_string(),
                        severity_explanation: Some("Resource leak on probe failure".to_string()),
                        preexisting: false,
                    }],
                },
                BenchmarkAncestorMbox {
                    version: 2,
                    mbox_path: "foo/v2.mbox".to_string(),
                    baseline_commit: Some(baseline_sha.to_string()),
                    patchset_status: BenchmarkAncestorStatus::Cancelled,
                    prior_findings: vec![],
                },
            ],
            target_mbox: Some("foo/v3.mbox".to_string()),
            baseline_commit: Some(baseline_sha.to_string()),
            expected_behavior: BenchmarkExpectedBehavior::DetectWithAttribution,
            expected_reviewer: Some("Bob Maintainer <bob@kernel.org>".to_string()),
            expected_prior_version: Some(1),
            ..Default::default()
        };

        ingest_versioned_benchmark_entries(&db, &bench_file, std::slice::from_ref(&entry)).await?;

        let (v1_p2_id, v1_ps_id, v1_p2_status, v1_ps_status, _) =
            lookup_benchmark_patch_row(&db, "v1-p2@example.com")
                .await?
                .expect("v1 patch 2 should exist");
        let (v2_p2_id, v2_ps_id, v2_p2_status, v2_ps_status, _) =
            lookup_benchmark_patch_row(&db, "v2-p2@example.com")
                .await?
                .expect("v2 patch 2 should exist");
        let (v3_p1_id, v3_ps_id, v3_p1_status, v3_ps_status, _) =
            lookup_benchmark_patch_row(&db, "v3-p1@example.com")
                .await?
                .expect("v3 patch 1 should exist");
        let (v3_p2_id, v3_ps_id_2, v3_p2_status, _, _) =
            lookup_benchmark_patch_row(&db, "v3-p2@example.com")
                .await?
                .expect("v3 patch 2 should exist");

        assert_eq!(v3_ps_id, v3_ps_id_2);
        assert_eq!(v1_ps_status.as_deref(), Some("Reviewed"));
        assert_eq!(v1_p2_status.as_deref(), Some("Reviewed"));
        assert_eq!(v2_ps_status.as_deref(), Some("Cancelled"));
        assert_eq!(v2_p2_status.as_deref(), Some("Cancelled"));
        assert_eq!(v3_ps_status.as_deref(), Some("Pending"));
        // Non-target patch 1/2 in v3 is marked Skipped so only target patch 2/2 runs LLM stages.
        assert_eq!(v3_p1_status.as_deref(), Some("Skipped"));
        assert_eq!(v3_p2_status.as_deref(), Some("Pending"));
        assert_ne!(v3_p1_id, v3_p2_id);

        // Verify v1 <- v2 <- v3 patchset lineage and patch lineage links.
        let mut ps_lineage = db
            .conn
            .query(
                "SELECT id, previous_patchset_id FROM patchsets WHERE id IN (?, ?, ?) ORDER BY id ASC",
                libsql::params![v1_ps_id, v2_ps_id, v3_ps_id],
            )
            .await?;
        let r1 = ps_lineage.next().await?.unwrap();
        assert_eq!(r1.get::<Option<i64>>(1)?, None);
        let r2 = ps_lineage.next().await?.unwrap();
        assert_eq!(r2.get::<Option<i64>>(1)?, Some(v1_ps_id));
        let r3 = ps_lineage.next().await?.unwrap();
        assert_eq!(r3.get::<Option<i64>>(1)?, Some(v2_ps_id));
        drop(ps_lineage);

        let mut p_lineage = db
            .conn
            .query(
                "SELECT previous_patch_id FROM patches WHERE id = ?",
                libsql::params![v3_p2_id],
            )
            .await?;
        let p_row = p_lineage.next().await?.unwrap();
        assert_eq!(p_row.get::<Option<i64>>(0)?, Some(v2_p2_id));
        drop(p_lineage);

        // Verify seeded Sashiko finding on v1 patch 2.
        let mut seeded_rows = db
            .conn
            .query(
                "SELECT f.problem FROM findings f JOIN reviews r ON f.review_id = r.id WHERE r.patch_id = ?",
                libsql::params![v1_p2_id],
            )
            .await?;
        let s_row = seeded_rows.next().await?.expect("seeded finding row");
        assert!(
            s_row
                .get::<String>(0)?
                .contains("Seeded v1 Sashiko finding")
        );
        assert!(seeded_rows.next().await?.is_none());
        drop(seeded_rows);

        let skipped_ids = load_benchmark_skipped_patch_ids(&db).await?;
        assert!(skipped_ids.contains(&v3_p1_id));
        assert!(!skipped_ids.contains(&v3_p2_id));

        // Re-ingesting against an existing DB (or shared ancestor) must not duplicate seeded findings.
        ingest_versioned_benchmark_entries(&db, &bench_file, std::slice::from_ref(&entry)).await?;
        let mut count_rows = db
            .conn
            .query(
                "SELECT COUNT(*) FROM findings f JOIN reviews r ON f.review_id = r.id WHERE r.patch_id = ?",
                libsql::params![v1_p2_id],
            )
            .await?;
        let count_row = count_rows.next().await?.expect("count row");
        assert_eq!(count_row.get::<i64>(0)?, 1);
        Ok(())
    }

    /// Checks that every Message-ID in `ids` names a message in the bundled
    /// mbox of `version` whose From address is one of `reviewers`. For
    /// Sashiko, the ID must be the patch a seeded prior finding is attached to.
    fn resolve_credit_ids(
        entry: &BenchmarkEntry,
        archive: &HashMap<String, String>,
        version: u32,
        reviewers: &[&str],
        ids: &[String],
        failures: &mut Vec<String>,
    ) -> Result<()> {
        let cid = entry.label();
        let Some(ancestor) = entry.ancestor_mboxes.iter().find(|a| a.version == version) else {
            failures.push(format!("case {cid}: no v{version} ancestor"));
            return Ok(());
        };
        let Some(mbox) = archive.get(&ancestor.mbox_path) else {
            failures.push(format!("case {cid}: {} missing", ancestor.mbox_path));
            return Ok(());
        };
        let mut authors: HashMap<String, String> = HashMap::new();
        for raw_msg in crate::ingestor::split_mbox(mbox.as_bytes()) {
            let (meta, _) = crate::patch::parse_email(&raw_msg)?;
            authors.insert(
                normalize_message_id(&meta.message_id).to_string(),
                meta.author,
            );
        }
        let accepted: BTreeSet<String> = reviewers
            .iter()
            .filter_map(|r| canonical_reviewer_identity(r))
            .collect();
        let seeded: BTreeSet<&str> = ancestor
            .prior_findings
            .iter()
            .filter_map(|pf| pf.patch_message_id.as_deref())
            .collect();
        for mid in ids {
            let Some(author) = authors.get(mid.as_str()) else {
                failures.push(format!("case {cid}: {mid} not in v{version} mbox"));
                continue;
            };
            if accepted.contains("sashiko") && seeded.contains(mid.as_str()) {
                continue;
            }
            let from = canonical_identity(author).unwrap_or_default();
            if !accepted.contains(&from) {
                failures.push(format!(
                    "case {cid}: {mid} is from {author:?}, not an expected reviewer"
                ));
            }
        }
        Ok(())
    }

    /// Every required (`expected_prior_message_ids`) and optional
    /// (`acceptable_attributions`) credit must resolve to real messages by
    /// the credited reviewer in the bundled mboxes.
    #[test]
    fn test_benchmark_versioned_attribution_labels_resolve_to_mbox_messages() -> Result<()> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let suite_dir = manifest_dir.join("projects/linux/benchmarks");
        let raw = std::fs::read_to_string(suite_dir.join("benchmark_versioned.json"))?;
        let entries: Vec<BenchmarkEntry> = serde_json::from_str(&raw)?;
        let archive = load_mbox_archive(&suite_dir.join("versioned_mboxes.jsonl.gz"))?;

        let mut failures = Vec::new();
        for entry in &entries {
            if entry.expected_behavior.expects_attribution() {
                let reviewers: Vec<&str> = entry
                    .expected_reviewer
                    .iter()
                    .chain(entry.expected_co_reviewers.iter())
                    .map(String::as_str)
                    .collect();
                resolve_credit_ids(
                    entry,
                    &archive,
                    entry.expected_prior_version.unwrap_or_default(),
                    &reviewers,
                    &entry.expected_prior_message_ids,
                    &mut failures,
                )?;
            }
            for acc in &entry.acceptable_attributions {
                resolve_credit_ids(
                    entry,
                    &archive,
                    acc.version,
                    &[acc.reviewer.as_str()],
                    &acc.message_ids,
                    &mut failures,
                )?;
            }
        }
        assert!(
            failures.is_empty(),
            "Attribution ground truth does not resolve:\n{}",
            failures.join("\n")
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_benchmark_versioned_suite_ingests_and_links_ancestors() -> Result<()> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let bench_file = manifest_dir.join("projects/linux/benchmarks/benchmark_versioned.json");
        let raw = std::fs::read_to_string(&bench_file)?;
        let entries: Vec<BenchmarkEntry> = serde_json::from_str(&raw)?;
        assert_eq!(entries.len(), 53, "Expected 53 multi-version LKML entries");
        validate_benchmark_entries(&entries)?;

        let mut cat_counts: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &entries {
            let cat = entry
                .category
                .as_deref()
                .expect("every entry must declare category");
            *cat_counts.entry(cat.to_string()).or_default() += 1;
            assert!(
                !entry.ancestor_mboxes.is_empty(),
                "case {:?} must have ancestor_mboxes",
                entry.case_id
            );
            assert!(
                entry.target_mbox.is_some(),
                "case {:?} must have target_mbox",
                entry.case_id
            );
            assert!(
                entry
                    .baseline_commit
                    .as_deref()
                    .is_some_and(|s| s.len() == 40),
                "case {:?} must have 40-char baseline_commit",
                entry.case_id
            );
        }
        assert_eq!(cat_counts.get("ignored_prior_bug").copied(), Some(15));
        assert_eq!(cat_counts.get("bad_advice_regression").copied(), Some(8));
        assert_eq!(cat_counts.get("wrong_claim_rejected").copied(), Some(11));
        assert_eq!(cat_counts.get("authority_unverifiable").copied(), Some(10));
        assert_eq!(cat_counts.get("multihop_or_routing").copied(), Some(9));

        let linux_repo = manifest_dir.join("third_party/linux");
        if linux_repo.join(".git").exists() || linux_repo.join("HEAD").exists() {
            for entry in &entries {
                let shas = std::iter::once(entry.baseline_commit.as_deref()).chain(
                    entry
                        .ancestor_mboxes
                        .iter()
                        .map(|a| a.baseline_commit.as_deref()),
                );
                for sha in shas.flatten() {
                    let rev = format!("{sha}^{{commit}}");
                    let out = crate::git_cmd::in_dir(&linux_repo)
                        .args(["cat-file", "-t", &rev])
                        .output()?;
                    assert!(
                        out.status.success(),
                        "case {:?} baseline_commit {sha} missing in third_party/linux",
                        entry.case_id
                    );
                }
            }
        }

        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await?;
        db.migrate().await?;
        ingest_versioned_benchmark_entries(&db, &bench_file, &entries).await?;

        let mut failures = Vec::new();
        for entry in &entries {
            let cid = entry.case_id.as_deref().unwrap_or(&entry.commit);
            let Some((patch_id, ps_id, patch_status, ps_status, _)) =
                lookup_benchmark_patch_row(&db, &entry.commit).await?
            else {
                failures.push(format!(
                    "case {cid}: target Commit {} not found",
                    entry.commit
                ));
                continue;
            };
            if patch_status.as_deref() != Some("Pending") {
                failures.push(format!("case {cid}: patch_status={patch_status:?}"));
            }
            if ps_status.as_deref() != Some("Pending") {
                failures.push(format!("case {cid}: ps_status={ps_status:?}"));
            }

            let prev_ps = db.get_previous_patchset_id(ps_id).await?;
            let Some(prev_ps_id) = prev_ps else {
                failures.push(format!(
                    "case {cid}: target patchset {ps_id} failed to link previous_patchset_id"
                ));
                continue;
            };

            let contexts = db.build_prior_version_contexts(ps_id, None, None).await?;
            let Some(ctx) = contexts.get(&patch_id) else {
                failures.push(format!(
                    "case {cid}: target patch {patch_id} (ps {ps_id}, prev_ps {prev_ps_id}) missing from contexts {:?}",
                    contexts.keys().collect::<Vec<_>>()
                ));
                continue;
            };
            if ctx.is_empty_for_audit() {
                let mut mcount_rows = db
                    .conn
                    .query(
                        "SELECT COUNT(*), (SELECT subject FROM patchsets WHERE id = ?) FROM messages WHERE thread_id IN (SELECT thread_id FROM patchsets WHERE id = ?)",
                        libsql::params![prev_ps_id, prev_ps_id],
                    )
                    .await?;
                let mrow = mcount_rows.next().await?.unwrap();
                let msg_cnt: i64 = mrow.get(0)?;
                let prev_subj: Option<String> = mrow.get(1)?;
                failures.push(format!(
                    "case {cid}: patch {patch_id} (ps {ps_id} -> prev_ps {prev_ps_id} {:?}, prev_thread_msgs={msg_cnt}, anc_vers={:?}) has zero auditable items",
                    prev_subj, ctx.ancestor_versions
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "Benchmark suite validation failed ({} cases):\n{}",
            failures.len(),
            failures.join("\n")
        );
        Ok(())
    }
}
