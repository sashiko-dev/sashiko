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

use crate::ReviewStatus;
use crate::settings::DatabaseSettings;
use anyhow::{Result, bail};
use libsql::Builder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

pub struct Database {
    pub conn: libsql::Connection,
    bug_actor: String,
    bug_tool: String,
    bug_model: Option<String>,
    bug_claim: Option<BugAnalysisClaim>,
}

/// Ownership of one analysis attempt, separate from its audit attribution.
#[derive(Clone)]
struct BugAnalysisClaim {
    bug_id: i64,
    owner: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Subsystem {
    pub id: i64,
    pub name: String,
    pub mailing_list_address: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PatchsetRow {
    pub id: i64,
    pub subject: Option<String>,
    pub status: Option<String>,
    pub thread_id: Option<i64>,
    pub author: Option<String>,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub total_parts: Option<u32>,
    pub received_parts: Option<u32>,
    pub mailing_lists: Vec<String>,
    pub subsystems: Vec<String>,
    pub findings_low: Option<i64>,
    pub findings_medium: Option<i64>,
    pub findings_high: Option<i64>,
    pub findings_critical: Option<i64>,
    pub baseline_id: Option<i64>,
    pub slug: Option<String>,
    pub failed_reason: Option<String>,
    pub skip_filters: Option<String>,
    pub only_filters: Option<String>,
    pub target_review_count: Option<u32>,
    pub model_name: Option<String>,
    pub prompts_git_hash: Option<String>,
    pub baseline_logs: Option<String>,
    pub provider: Option<String>,
    #[serde(skip)]
    pub embargo_until: Option<i64>,
    pub mr_url: Option<String>,
    pub mr_title: Option<String>,
    pub mr_number: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ReleaseReview {
    pub id: i64,
    pub patch_id: i64,
    pub patch_message_id: String,
    pub index: i64,
    pub inline_review: String,
    pub summary: String,
    pub findings: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchsetReviewOutcome {
    Clean,
    HasFindings,
    Incomplete,
}

const CLEAN_PATCHSET_PREDICATE: &str = "
    EXISTS (
        SELECT 1 FROM reviews r
        WHERE r.patchset_id = p.id AND r.status = 'Reviewed'
    )
    AND NOT EXISTS (
        SELECT 1 FROM reviews r
        WHERE r.patchset_id = p.id AND r.status = 'Skipped'
          AND r.result_description = 'Skipped AI review via --no-ai'
    )
    AND NOT EXISTS (
        SELECT 1 FROM patches pa
        WHERE pa.patchset_id = p.id
          AND COALESCE(pa.status, '') != 'Skipped'
          AND NOT EXISTS (
              SELECT 1 FROM reviews skipped
              WHERE skipped.patch_id = pa.id
                AND skipped.status = 'Skipped'
                AND skipped.result_description = 'Skipped: touches only ignored files'
          )
          AND (
              SELECT COUNT(*) FROM reviews completed
              WHERE completed.patch_id = pa.id
                AND completed.status = 'Reviewed'
          ) < COALESCE(p.target_review_count, 1)
    )
    AND NOT EXISTS (
        SELECT 1 FROM reviews r
        JOIN findings f ON f.review_id = r.id
        WHERE r.patchset_id = p.id AND r.status = 'Reviewed'
    )";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub message_id: String,
    pub thread_id: Option<i64>,
    pub in_reply_to: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub date: Option<i64>,
    pub body: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub thread: Option<Vec<serde_json::Value>>,
    pub git_blob_hash: Option<String>,
    pub mailing_list: Option<String>,
    pub diff: Option<String>,
    pub references_hdr: Option<String>,
}

pub struct AiInteractionParams<'a> {
    pub id: &'a str,
    pub parent_id: Option<&'a str>,
    pub workflow_id: Option<&'a str>,
    pub provider: &'a str,
    pub model: &'a str,
    pub input: &'a str,
    pub output: &'a str,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolUsage {
    pub review_id: i64,
    pub provider: String,
    pub model: String,
    pub tool_name: String,
    pub arguments: Option<String>,
    pub output_length: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum Severity {
    #[default]
    Unknown = 0,
    Low = 1,
    Medium = 2,
    High = 3,
    Critical = 4,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Unknown => "Unknown",
            Severity::Low => "Low",
            Severity::Medium => "Medium",
            Severity::High => "High",
            Severity::Critical => "Critical",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Severity {
    pub fn from_i32(val: i32) -> Self {
        match val {
            4 => Severity::Critical,
            3 => Severity::High,
            2 => Severity::Medium,
            1 => Severity::Low,
            _ => Severity::Unknown,
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        let s = s.trim();
        if s.eq_ignore_ascii_case("critical") {
            Severity::Critical
        } else if s.to_lowercase().starts_with("high") {
            Severity::High
        } else if s.to_lowercase().starts_with("medium") {
            Severity::Medium
        } else if s.to_lowercase().starts_with("low") {
            Severity::Low
        } else {
            Severity::Unknown
        }
    }
}

/// Triage lifecycle of a Linux kernel bug.
///
/// Owned by humans, the API, and the deduplication stage. Deliberately separate
/// from [`BugPipelineState`]: re-running analysis must never be able to discard
/// a triage decision that a person made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BugLifecycleStatus {
    /// Recorded but not yet triaged.
    #[default]
    New,
    /// Confirmed as a real, actionable defect.
    Open,
    /// Resolved by a fix that has landed.
    Fixed,
    /// Determined not to be a real defect.
    Dismissed,
    /// Folded into a canonical bug; implies duplicate_of_id is set.
    Duplicate,
    /// Closed without a fix, for example obsolete or will not fix.
    Closed,
}

impl BugLifecycleStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BugLifecycleStatus::New => "new",
            BugLifecycleStatus::Open => "open",
            BugLifecycleStatus::Fixed => "fixed",
            BugLifecycleStatus::Dismissed => "dismissed",
            BugLifecycleStatus::Duplicate => "duplicate",
            BugLifecycleStatus::Closed => "closed",
        }
    }

    /// Reports whether the bug has reached a state that needs no further triage.
    pub fn is_resolved(&self) -> bool {
        matches!(
            self,
            BugLifecycleStatus::Fixed
                | BugLifecycleStatus::Dismissed
                | BugLifecycleStatus::Duplicate
                | BugLifecycleStatus::Closed
        )
    }
}

impl std::fmt::Display for BugLifecycleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for BugLifecycleStatus {
    type Err = anyhow::Error;

    /// Parses strictly. An unrecognised value means the row disagrees with the
    /// CHECK constraint on the column, which is a corrupt database rather than
    /// something to paper over with a default.
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "new" => Ok(BugLifecycleStatus::New),
            "open" => Ok(BugLifecycleStatus::Open),
            "fixed" => Ok(BugLifecycleStatus::Fixed),
            "dismissed" => Ok(BugLifecycleStatus::Dismissed),
            "duplicate" => Ok(BugLifecycleStatus::Duplicate),
            "closed" => Ok(BugLifecycleStatus::Closed),
            other => bail!("unknown bug lifecycle status: {other:?}"),
        }
    }
}

/// Execution state of the analysis pipeline for a Linux kernel bug.
///
/// Written exclusively by the bug worker. Crash recovery only ever touches this
/// field, which is what keeps triage state safe across restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BugPipelineState {
    /// Waiting to be claimed by a worker.
    #[default]
    Pending,
    /// Claimed by a worker holding an unexpired lease.
    Running,
    /// Analysis completed.
    Succeeded,
    /// Analysis errored and remains eligible for retry.
    Failed,
    /// Analysis errored too many times; never claimed again without operator action.
    Abandoned,
}

impl BugPipelineState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BugPipelineState::Pending => "pending",
            BugPipelineState::Running => "running",
            BugPipelineState::Succeeded => "succeeded",
            BugPipelineState::Failed => "failed",
            BugPipelineState::Abandoned => "abandoned",
        }
    }

    /// Reports whether analysis is queued or in flight, and therefore whether
    /// the user should be told that results are still on their way.
    pub fn is_in_progress(&self) -> bool {
        matches!(self, BugPipelineState::Pending | BugPipelineState::Running)
    }
}

impl std::fmt::Display for BugPipelineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for BugPipelineState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "pending" => Ok(BugPipelineState::Pending),
            "running" => Ok(BugPipelineState::Running),
            "succeeded" => Ok(BugPipelineState::Succeeded),
            "failed" => Ok(BugPipelineState::Failed),
            "abandoned" => Ok(BugPipelineState::Abandoned),
            other => bail!("unknown bug pipeline state: {other:?}"),
        }
    }
}

pub struct Finding {
    pub review_id: i64,
    pub severity: Severity,
    pub severity_explanation: Option<String>,
    pub problem: String,
    pub preexisting: Option<bool>,
    pub locations: Option<serde_json::Value>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Bug {
    #[serde(rename = "internal_id")]
    pub id: i64,
    #[serde(alias = "slug")]
    pub bugid: String,
    pub title: String,
    /// Triage state. See [`BugLifecycleStatus`].
    #[serde(default)]
    pub lifecycle_status: BugLifecycleStatus,
    /// Analysis execution state. See [`BugPipelineState`].
    #[serde(default)]
    pub pipeline_state: BugPipelineState,
    pub reporter: String,
    pub reported_at: i64,
    /// Email address of whoever is working on this bug, if anyone.
    pub assignee: Option<String>,
    pub assigned_at: Option<i64>,
    pub discovered_in_patchset_id: Option<i64>,
    pub discovered_in_patch_id: Option<i64>,
    pub discovered_in_commit: Option<String>,
    pub source_ref: Option<String>,
    /// Deduplication embedding, joined in from bug_vectors rather than
    /// stored on the core row. Only populated by queries that need it.
    pub vector_json: Option<String>,
    pub duplicate_of_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,

    #[serde(default)]
    pub subsystems: Vec<String>,
    #[serde(default)]
    pub enrichments: Vec<BugEnrichment>,
}

impl std::fmt::Debug for Bug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bug")
            .field("id", &self.id)
            .field("bugid", &self.bugid)
            .field("title", &self.title)
            .field("lifecycle_status", &self.lifecycle_status)
            .field("pipeline_state", &self.pipeline_state)
            .field("reporter", &self.reporter)
            .field("reported_at", &self.reported_at)
            .field("assignee", &self.assignee)
            .field("subsystems", &self.subsystems)
            .field("enrichments", &self.enrichments.len())
            .field("discovered_in_patchset_id", &self.discovered_in_patchset_id)
            .field("discovered_in_patch_id", &self.discovered_in_patch_id)
            .field("discovered_in_commit", &self.discovered_in_commit)
            .field("source_ref", &self.source_ref)
            .field("duplicate_of_id", &self.duplicate_of_id)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl Bug {
    #[inline]
    pub fn slug(&self) -> &str {
        &self.bugid
    }

    #[inline]
    pub fn problem(&self) -> &str {
        &self.title
    }

    pub fn severity(&self) -> Severity {
        for e in self.enrichments.iter().rev() {
            if e.kind == "severity_calibration"
                && let Some(ref data) = e.data_json
            {
                if let Some(sev_str) = data.get("severity").and_then(|v| v.as_str()) {
                    return Severity::from_str(sev_str);
                }
                if let Some(sev_int) = data.get("severity_int").and_then(|v| v.as_i64()) {
                    return Severity::from_i32(sev_int as i32);
                }
            }
        }
        Severity::Unknown
    }

    pub fn severity_explanation(&self) -> Option<String> {
        for e in self.enrichments.iter().rev() {
            if e.kind == "severity_calibration" {
                if let Some(ref content) = e.content
                    && !content.is_empty()
                {
                    return Some(content.clone());
                }
                if let Some(ref data) = e.data_json
                    && let Some(exp) = data.get("explanation").and_then(|v| v.as_str())
                {
                    return Some(exp.to_string());
                }
            } else if e.kind == "verification"
                && let Some(ref data) = e.data_json
                && let Some(refutation) = data.get("refutation_evidence").and_then(|v| v.as_str())
            {
                return Some(refutation.to_string());
            }
        }
        None
    }

    pub fn description(&self) -> Option<String> {
        for e in self.enrichments.iter().rev() {
            if e.kind == "report"
                && let Some(ref content) = e.content
            {
                return Some(content.clone());
            }
        }
        None
    }

    pub fn inline_review(&self) -> String {
        self.description().unwrap_or_default()
    }

    pub fn verified_on_sha(&self) -> Option<String> {
        for e in self.enrichments.iter().rev() {
            if e.kind == "verification"
                && let Some(ref data) = e.data_json
                && let Some(sha) = data.get("verified_on_sha").and_then(|v| v.as_str())
            {
                return Some(sha.to_string());
            }
        }
        None
    }

    pub fn locations(&self) -> Option<serde_json::Value> {
        for e in self.enrichments.iter().rev() {
            if e.kind == "verification"
                && let Some(ref data) = e.data_json
                && let Some(locs) = data.get("locations")
                && !locs.is_null()
            {
                return Some(locs.clone());
            }
        }
        for e in &self.enrichments {
            if (e.kind == "candidate" || e.kind == "raw_candidate")
                && let Some(ref data) = e.data_json
                && let Some(locs) = data.get("locations")
                && !locs.is_null()
            {
                return Some(locs.clone());
            }
        }
        None
    }

    pub fn source_files(&self) -> Option<Vec<String>> {
        for e in self.enrichments.iter().rev() {
            if e.kind == "verification"
                && let Some(ref data) = e.data_json
                && let Some(files) = data
                    .get("source_files")
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            {
                return Some(files);
            }
        }
        None
    }

    pub fn introduced_in_commit(&self) -> Option<String> {
        for e in self.enrichments.iter().rev() {
            if e.kind == "origin_discovery" {
                if let Some(ref data) = e.data_json
                    && let Some(sha) = data.get("introducing_commit_sha").and_then(|v| v.as_str())
                {
                    if let Some(title) = data
                        .get("introducing_commit_title")
                        .and_then(|v| v.as_str())
                    {
                        return Some(format!("{} ({})", &sha[..12.min(sha.len())], title));
                    }
                    return Some(sha.to_string());
                }
                if let Some(ref content) = e.content {
                    return Some(content.clone());
                }
            }
        }
        None
    }

    pub fn is_fixed(&self) -> bool {
        if self.lifecycle_status == BugLifecycleStatus::Fixed {
            return true;
        }
        for e in &self.enrichments {
            if e.kind == "fix_candidate"
                && let Some(ref data) = e.data_json
                && data.get("status").and_then(|v| v.as_str()) == Some("merged")
            {
                return true;
            }
        }
        false
    }

    pub fn fixed_in_commit(&self) -> Option<String> {
        for e in self.enrichments.iter().rev() {
            if e.kind == "fix_candidate"
                && let Some(ref data) = e.data_json
                && let Some(sha) = data.get("commit_sha").and_then(|v| v.as_str())
            {
                return Some(sha.to_string());
            }
        }
        None
    }

    pub fn raw_input(&self) -> Option<String> {
        for e in &self.enrichments {
            if e.kind == "candidate" || e.kind == "raw_candidate" {
                if let Some(ref data) = e.data_json {
                    return serde_json::to_string(data).ok();
                }
                if let Some(ref content) = e.content {
                    return Some(content.clone());
                }
            }
        }
        None
    }

    pub fn tokens_in(&self) -> usize {
        self.enrichments.iter().filter_map(|e| e.tokens_in).sum()
    }

    pub fn tokens_out(&self) -> usize {
        self.enrichments.iter().filter_map(|e| e.tokens_out).sum()
    }

    pub fn tokens_cached(&self) -> usize {
        self.enrichments
            .iter()
            .filter_map(|e| e.tokens_cached)
            .sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BugEnrichment {
    pub id: i64,
    pub bug_id: i64,
    pub kind: String,
    pub tool: String,
    pub model: Option<String>,
    pub author: Option<String>,
    pub created_at: i64,
    pub content: Option<String>,
    pub data_json: Option<serde_json::Value>,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    pub tokens_cached: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NewBugEnrichment {
    pub kind: String,
    pub tool: String,
    pub model: Option<String>,
    pub author: Option<String>,
    pub created_at: i64,
    pub content: Option<String>,
    pub data_json: Option<serde_json::Value>,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    pub tokens_cached: Option<usize>,
    pub logs: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewBug {
    #[serde(alias = "slug")]
    pub bugid: String,
    #[serde(default = "default_bug_title", alias = "problem")]
    pub title: String,
    /// Defaults to New: a freshly reported bug has not been triaged yet.
    #[serde(default)]
    pub lifecycle_status: BugLifecycleStatus,
    /// Defaults to Pending: a freshly reported bug is awaiting analysis.
    #[serde(default)]
    pub pipeline_state: BugPipelineState,
    #[serde(default = "default_bug_reporter")]
    pub reporter: String,
    #[serde(default = "default_now")]
    pub reported_at: i64,
    #[serde(default)]
    pub assignee: Option<String>,
    pub discovered_in_patchset_id: Option<i64>,
    pub discovered_in_patch_id: Option<i64>,
    pub discovered_in_commit: Option<String>,
    pub source_ref: Option<String>,
    pub vector_json: Option<String>,
    pub duplicate_of_id: Option<i64>,
    #[serde(default)]
    pub subsystems: Vec<AttributedSubsystem>,
}

/// Where a subsystem name attached to a bug came from.
///
/// Only [`SubsystemSource::MaintainersSection`] identifies a real kernel
/// maintainer, so only that variant can confer access to a bug. The other two
/// are useful for display and filtering and confer nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubsystemSource {
    /// A section title matched out of the kernel MAINTAINERS file.
    MaintainersSection,
    /// A directory prefix derived from the touched paths, or the `kernel`
    /// sentinel used when nothing more specific could be determined.
    PathPrefix,
    /// Supplied verbatim by whoever filed the bug. The default, because a name
    /// of unknown origin must not be mistaken for a maintainer's jurisdiction.
    #[default]
    CallerSupplied,
}

impl SubsystemSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MaintainersSection => "maintainers_section",
            Self::PathPrefix => "path_prefix",
            Self::CallerSupplied => "caller_supplied",
        }
    }

    /// Parses a stored value, treating anything unrecognised as caller
    /// supplied so that an unexpected string cannot widen access.
    pub fn from_stored(value: &str) -> Self {
        match value {
            "maintainers_section" => Self::MaintainersSection,
            "path_prefix" => Self::PathPrefix,
            _ => Self::CallerSupplied,
        }
    }

    /// Whether a row with this provenance can grant a maintainer access to the
    /// bug it is attached to.
    pub fn confers_authority(&self) -> bool {
        matches!(self, Self::MaintainersSection)
    }
}

/// Attaches a subsystem to a bug, refreshing the provenance when the pair is
/// already present. Rewriting the provenance matters: a name that used to be
/// caller supplied and is later matched out of MAINTAINERS has to start
/// conferring authority, and a name that stops matching has to stop.
const UPSERT_BUG_SUBSYSTEM_SQL: &str = "INSERT INTO bug_subsystems (bug_id, subsystem, source) \
     VALUES (?, ?, ?) \
     ON CONFLICT(bug_id, subsystem) DO UPDATE SET source = excluded.source";

/// A subsystem name together with the provenance of that name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttributedSubsystem {
    pub name: String,
    pub source: SubsystemSource,
}

impl AttributedSubsystem {
    pub fn new(name: impl Into<String>, source: SubsystemSource) -> Self {
        Self {
            name: name.into(),
            source,
        }
    }

    /// A name matched out of MAINTAINERS, which is the only kind that grants
    /// a maintainer authority over the bug.
    pub fn from_maintainers(name: impl Into<String>) -> Self {
        Self::new(name, SubsystemSource::MaintainersSection)
    }

    /// A directory prefix or sentinel derived from the touched paths.
    pub fn from_path_prefix(name: impl Into<String>) -> Self {
        Self::new(name, SubsystemSource::PathPrefix)
    }
}

/// Accepts either a bare string or an object. A bare string is recorded as
/// caller supplied, which is the fail-closed reading of a name whose origin
/// was never stated.
impl<'de> Deserialize<'de> for AttributedSubsystem {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Bare(String),
            Attributed {
                name: String,
                #[serde(default)]
                source: SubsystemSource,
            },
        }

        Ok(match Repr::deserialize(deserializer)? {
            Repr::Bare(name) => Self::new(name, SubsystemSource::CallerSupplied),
            Repr::Attributed { name, source } => Self::new(name, source),
        })
    }
}

fn default_bug_title() -> String {
    String::new()
}

fn default_bug_reporter() -> String {
    "sashiko".to_string()
}

fn default_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Canonical projection for reads of a core bug row.
///
/// Every query whose rows are handed to `Database::parse_bug_row_core` must
/// select exactly these columns, in this order. Keeping the list in one place
/// is what stops a new query from silently shifting the column indices.
///
/// The deduplication embedding is intentionally absent: it lives in
/// bug_vectors and is only fetched by the dedup path, so ordinary reads
/// never carry the blob.
/// Renders the summary of a bug as it appears embedded in a patchset or review
/// payload.
///
/// Shared by both callers so that the two views cannot drift apart. Both
/// previously omitted the bug's state entirely, which left the badge on the
/// patchset detail card permanently reading Open regardless of the bug's
/// actual triage or analysis state.
fn bug_reference_json(bug: &Bug, is_newly_discovered: bool) -> serde_json::Value {
    serde_json::json!({
        "id": bug.id,
        "bugid": bug.bugid,
        "slug": bug.bugid,
        "problem": bug.problem(),
        "severity": bug.severity().as_str(),
        "subsystems": bug.subsystems,
        "subsystem": bug.subsystems.first().cloned(),
        "inline_review": bug.inline_review(),
        "is_newly_discovered": is_newly_discovered,
        "created_at": bug.created_at,
        "lifecycle_status": bug.lifecycle_status.as_str(),
        "pipeline_state": bug.pipeline_state.as_str(),
        "is_fixed": bug.is_fixed(),
        "assignee": bug.assignee,
    })
}

const BUG_ROW_COLUMNS: &str = "id, bugid, title, lifecycle_status, pipeline_state,
     reporter, reported_at, assignee, assigned_at,
     discovered_in_patchset_id, discovered_in_patch_id, discovered_in_commit,
     source_ref, duplicate_of_id, created_at, updated_at";

#[derive(Default, Debug, Clone)]
pub struct UpdateBugOutcomeParams<'a> {
    /// Triage verdict reached by the analysis. Execution failures are reported
    /// through `Database::fail_bug_analysis` instead, so that a crashed run can
    /// never be mistaken for a triage decision.
    pub lifecycle_status: BugLifecycleStatus,
    pub problem: Option<&'a str>,
    pub subsystems: Option<&'a [AttributedSubsystem]>,
    pub source_files: Option<&'a [String]>,
    pub locations: Option<&'a serde_json::Value>,
    pub severity: Severity,
    pub severity_explanation: Option<&'a str>,
    pub inline_review: &'a str,
    pub logs: Option<&'a str>,
    pub vector_json: Option<&'a str>,
    pub introduced_in_commit: Option<&'a str>,
    pub verified_on_sha: Option<&'a str>,
    pub is_fixed: bool,
    pub fixed_in_commit: Option<&'a str>,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    pub tokens_cached: Option<usize>,
}

#[derive(Default, Debug, Clone)]
pub struct MarkDuplicateBugParams<'a> {
    /// Automatic analysis may only fold a bug that has not been triaged.
    pub preserve_triage: bool,
    pub ephemeral_id: i64,
    pub canonical_id: i64,
    pub reasoning: &'a str,
    pub logs: Option<&'a str>,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    pub tokens_cached: Option<usize>,
}

/// Which bugs a listing may return.
///
/// The default is the empty scope rather than everything: a caller that forgets
/// to say what the principal may see gets nothing back, so widening access has
/// to be written down deliberately.
#[derive(Debug, Clone, Copy)]
pub enum BugVisibility<'a> {
    /// Every bug. For Sashiko operators, the kernel security list, and
    /// maintainers of a section that claims the whole tree.
    Unrestricted,
    /// Only bugs that the MAINTAINERS file attributes to one of these section
    /// titles. Matching is ASCII case-insensitive, and rows attributed by a
    /// directory prefix or by the caller are never matched.
    Sections(&'a [String]),
}

impl Default for BugVisibility<'_> {
    fn default() -> Self {
        BugVisibility::Sections(&[])
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListBugsParams<'a> {
    pub page: Option<u32>,
    pub limit: Option<u32>,
    pub min_severity: Option<Severity>,
    pub subsystem: Option<&'a str>,
    pub subsystems: Option<&'a [String]>,
    pub lifecycle_status: Option<BugLifecycleStatus>,
    pub pipeline_state: Option<BugPipelineState>,
    pub assignee: Option<AssigneeFilter<'a>>,
    pub search: Option<&'a str>,
    pub sort_by: Option<&'a str>,
    pub sort_order: Option<&'a str>,
    /// What the calling principal is allowed to see. Applied on top of every
    /// other filter, so a subsystem filter can only ever narrow the result.
    pub visibility: BugVisibility<'a>,
}

/// Selects bugs by who they are assigned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssigneeFilter<'a> {
    /// Only bugs nobody has picked up.
    Unassigned,
    /// Only bugs assigned to this exact address.
    Is(&'a str),
}

/// What an outbox row is for.
///
/// Review notifications and transactional mail share a transport but differ in
/// how they are deduplicated, rate limited and observed, so the purpose is
/// carried explicitly rather than inferred from whether a patch is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmailKind {
    /// A review result or a patchwork notification, tied to a patch.
    #[default]
    ReviewNotification,
    /// A sign-in link, tied to a person.
    SignInLink,
}

impl EmailKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EmailKind::ReviewNotification => "review_notification",
            EmailKind::SignInLink => "sign_in_link",
        }
    }

    /// Reads a stored value.
    ///
    /// A value written by a newer binary is reported as a review notification,
    /// which is the treatment that adds no headers and grants no exemption, so
    /// an unrecognized row is delivered plainly rather than dropped.
    pub fn from_stored(value: &str) -> Self {
        match value {
            "sign_in_link" => EmailKind::SignInLink,
            "review_notification" => EmailKind::ReviewNotification,
            other => {
                tracing::warn!("Unrecognized email kind {:?}, treating as review", other);
                EmailKind::ReviewNotification
            }
        }
    }
}

pub struct EmailOutboxRow {
    pub id: i64,
    pub patch_id: Option<i64>,
    pub kind: EmailKind,
    pub status: String,
    pub to_addresses: String,
    pub cc_addresses: String,
    pub subject: String,
    pub in_reply_to: String,
    pub references_hdr: String,
    pub body: String,
    pub locked_at: Option<i64>,
    pub error_log: Option<String>,
    pub created_at: i64,
}

pub struct PatchworkOutboxRow {
    pub id: i64,
    pub patch_msg_id: String,
    pub api_url: String,
    pub check_state: String,
    pub description: String,
    pub target_url: String,
    pub context: String,
    pub status: String,
    pub retry_count: i64,
    pub next_retry_at: Option<i64>,
    pub locked_at: Option<i64>,
    pub error_log: Option<String>,
    pub created_at: i64,
}

impl Database {
    pub fn has_bug_actor(&self) -> bool {
        self.bug_actor != "system" || self.bug_tool != "sashiko"
    }

    pub fn bug_actor(&self) -> &str {
        &self.bug_actor
    }

    pub fn bug_tool(&self) -> &str {
        &self.bug_tool
    }

    pub fn bug_model(&self) -> Option<&str> {
        self.bug_model.as_deref()
    }

    /// Attribution is scoped to this handle, never shared mutable connection state.
    pub fn with_bug_actor(&self, author: &str, tool: &str, model: Option<String>) -> Self {
        Self {
            conn: self.conn.clone(),
            bug_actor: author.into(),
            bug_tool: tool.into(),
            bug_model: model,
            bug_claim: self.bug_claim.clone(),
        }
    }

    /// Keeps attribution attached to writes performed inside a transaction.
    fn with_connection(&self, conn: libsql::Connection) -> Self {
        Self {
            conn,
            bug_actor: self.bug_actor.clone(),
            bug_tool: self.bug_tool.clone(),
            bug_model: self.bug_model.clone(),
            bug_claim: self.bug_claim.clone(),
        }
    }

    /// Binds analysis writes to the exact attempt that claimed this bug.
    pub fn with_bug_claim(&self, bug_id: i64, owner: &str) -> Self {
        let mut scoped = self.with_connection(self.conn.clone());
        scoped.bug_claim = Some(BugAnalysisClaim {
            bug_id,
            owner: owner.into(),
        });
        scoped
    }

    /// Checks ownership under the same write lock as the ensuing mutation.
    async fn begin_bug_write(&self, bug_id: i64) -> Result<libsql::Transaction> {
        if self
            .bug_claim
            .as_ref()
            .is_some_and(|claim| claim.bug_id != bug_id)
        {
            bail!("Analysis claim belongs to a different bug");
        }
        let tx = self
            .conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        if let Some(claim) = &self.bug_claim {
            let held = {
                let mut rows = tx
                    .query(
                        "SELECT 1 FROM bugs WHERE id = ? AND locked_by = ?
                     AND lease_expires_at >= ? AND pipeline_state IN ('running', 'succeeded')",
                        libsql::params![
                            claim.bug_id,
                            claim.owner.as_str(),
                            chrono::Utc::now().timestamp()
                        ],
                    )
                    .await?;
                rows.next().await?.is_some()
            };
            if !held {
                bail!("Bug analysis lease is no longer held");
            }
        }
        Ok(tx)
    }

    pub async fn get_oldest_message_timestamp(&self) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query("SELECT MIN(date) FROM messages WHERE date > 0", ())
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0).ok())
        } else {
            Ok(None)
        }
    }

    pub async fn get_message_details(&self, id: i64) -> Result<Option<MessageRow>> {
        let mut rows = self.conn.query(
            "SELECT m.id, m.message_id, m.thread_id, m.in_reply_to, m.author, m.subject, m.date, m.body, m.to_recipients, m.cc_recipients, m.git_blob_hash, m.mailing_list, p.diff, m.references_hdr 
             FROM messages m 
             LEFT JOIN patches p ON m.message_id = p.message_id
             WHERE m.id = ?",
             libsql::params![id],
        ).await?;

        let row_data = if let Ok(Some(row)) = rows.next().await {
            Some((
                row.get::<i64>(0)?,
                row.get::<String>(1)?,
                row.get::<Option<i64>>(2).ok().flatten(),
                row.get::<Option<String>>(3).ok().flatten(),
                row.get::<Option<String>>(4).ok().flatten(),
                row.get::<Option<String>>(5).ok().flatten(),
                row.get::<Option<i64>>(6).ok().flatten(),
                crate::compression::get_compressed_string_opt(&row, 7).unwrap_or(None),
                row.get::<Option<String>>(8).ok().flatten(),
                row.get::<Option<String>>(9).ok().flatten(),
                row.get::<Option<String>>(10).ok().flatten(),
                row.get::<Option<String>>(11).ok().flatten(),
                crate::compression::get_compressed_string_opt(&row, 12).unwrap_or(None),
                row.get::<Option<String>>(13).ok().flatten(),
            ))
        } else {
            None
        };

        if let Some((
            id,
            message_id,
            thread_id,
            in_reply_to,
            author,
            subject,
            date,
            body,
            to,
            cc,
            git_blob_hash,
            mailing_list,
            raw_diff,
            references_hdr,
        )) = row_data
        {
            // Fetch thread messages
            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT id, message_id, author, date, subject, in_reply_to FROM messages WHERE thread_id = ? AND subject != '(placeholder)' ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "id": m.get::<i64>(0)?,
                        "message_id": m.get::<String>(1)?,
                        "author": m.get::<Option<String>>(2).ok(),
                        "date": m.get::<Option<i64>>(3).ok(),
                        "subject": m.get::<Option<String>>(4).ok(),
                        "in_reply_to": m.get::<Option<String>>(5).ok(),
                    }));
                }
            }

            // For email-based patches, the diff is often just the body.
            // We don't want to show it twice in the UI.
            // For git commits, body is the commit message and diff is the actual diff.
            let diff = if let (Some(b), Some(d)) = (&body, &raw_diff) {
                if b == d { None } else { raw_diff.clone() }
            } else {
                raw_diff.clone()
            };

            Ok(Some(MessageRow {
                id,
                message_id,
                thread_id,
                in_reply_to,
                author,
                subject,
                date,
                body,
                to,
                cc,
                git_blob_hash,
                mailing_list,
                diff,
                references_hdr,
                thread: Some(messages),
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn get_message_details_by_msgid(&self, msg_id: &str) -> Result<Option<MessageRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM messages WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;

        let id = if let Ok(Some(row)) = rows.next().await {
            Some(row.get::<i64>(0)?)
        } else {
            None
        };

        if let Some(id) = id {
            self.get_message_details(id).await
        } else {
            Ok(None)
        }
    }

    pub fn get_msgid_candidates(msg_id: &str) -> Vec<String> {
        let trimmed = msg_id.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }

        let mut candidates = vec![trimmed.to_string()];

        let clean = trimmed.trim_matches(['<', '>']);
        if clean != trimmed {
            candidates.push(clean.to_string());
        } else {
            candidates.push(format!("<{}>", clean));
        }

        if let Some(stripped) = clean.strip_suffix("@sashiko.local") {
            if !stripped.is_empty() {
                candidates.push(stripped.to_string());
                candidates.push(format!("<{}>", stripped));
            }
        } else {
            candidates.push(format!("{}@sashiko.local", clean));
        }

        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.clone()));
        candidates
    }

    pub async fn get_patchset_details_by_msgid(
        &self,
        msg_id: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        let candidates = Self::get_msgid_candidates(msg_id);

        // 1. Try to find a patchset where this is the cover letter
        for clid in &candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM patchsets WHERE cover_letter_message_id = ? ORDER BY id DESC LIMIT 1",
                    libsql::params![clid.clone()],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0)?;
                return self.get_patchset_details(id, page, limit).await;
            }
        }

        // 2. Fallback: Find a patchset that contains this message as a patch
        for clid in &candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT patchset_id FROM patches WHERE message_id = ? ORDER BY id DESC LIMIT 1",
                    libsql::params![clid.clone()],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0)?;
                return self.get_patchset_details(id, page, limit).await;
            }
        }

        Ok(None)
    }

    pub async fn get_message_body(&self, msg_id: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT body, git_blob_hash, mailing_list FROM messages WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let body: Option<String> =
                crate::compression::get_compressed_string_opt(&row, 0).unwrap_or(None);
            if let Some(b) = body
                && !b.is_empty()
            {
                return Ok(Some(b));
            }
            // Try git blob
            let hash: Option<String> = row.get(1).ok();
            let group: Option<String> = row.get(2).ok();

            if let (Some(_h), Some(_g)) = (hash, group) {
                // We don't have easy access to git_ops::read_blob here without repo path.
                // The DB does not know about repo path logic; it must be passed in.
                // The Reviewer service has the repo path.
                // Return None if the body is empty in the DB, and let the caller handle the blob if needed.
                // The body is needed for the base-commit.
                // The body is populated in the DB if it is small.
                // Sashiko stores the body in the DB unless it is a large patch.
                // See "body_to_store" logic in main.rs:
                // `if is_git_hash { ("", Some(hash)) } else { (body, None) }`
                // If it is from a git archive, the body is empty in the DB.
                return Ok(None);
            }
            Ok(None)
        } else {
            Ok(None)
        }
    }

    pub async fn new(settings: &DatabaseSettings) -> Result<Self> {
        info!(
            "Connecting to database at {}",
            crate::utils::redact_secret(&settings.url)
        );

        let db = if settings.url.starts_with("libsql://") || settings.url.starts_with("https://") {
            Builder::new_remote(settings.url.clone(), settings.token.clone())
                .build()
                .await?
        } else {
            Builder::new_local(&settings.url).build().await?
        };

        let conn = db.connect()?;

        // Enable WAL mode for better concurrency
        // PRAGMA journal_mode returns a row (the new mode), so we must use query() instead of execute()
        let _ = conn
            .query("PRAGMA journal_mode=WAL;", ())
            .await?
            .next()
            .await;
        let _ = conn
            .query("PRAGMA busy_timeout = 5000;", ())
            .await?
            .next()
            .await;
        // Foreign keys are off by default in SQLite and must be re-enabled per
        // connection. Without this every ON DELETE CASCADE in the schema is
        // inert and orphaned child rows accumulate silently.
        conn.execute("PRAGMA foreign_keys = ON;", ()).await?;

        Ok(Self {
            conn,
            bug_actor: "system".into(),
            bug_tool: "sashiko".into(),
            bug_model: None,
            bug_claim: None,
        })
    }

    pub async fn migrate(&self) -> Result<()> {
        let current_version: u32 = {
            let mut rows = self.conn.query("PRAGMA user_version", ()).await?;
            if let Some(row) = rows.next().await? {
                row.get(0).unwrap_or(0)
            } else {
                0
            }
        };

        if current_version < 1 {
            info!("Applying database migration version 1 (initial)...");
            let schema = include_str!("migrations/001_initial.sql");
            self.conn.execute_batch(schema).await?;
            self.conn.execute("PRAGMA user_version = 1", ()).await?;
        }

        if current_version < 2 {
            info!("Applying database migration version 2 (bugs)...");
            let tx = self.conn.transaction().await?;
            tx.execute_batch(include_str!("migrations/002_bugs.sql"))
                .await?;
            tx.execute("PRAGMA user_version = 2", ()).await?;
            tx.commit().await?;
        }

        if current_version < 3 {
            info!("Applying database migration version 3 (email outbox kind)...");
            let tx = self.conn.transaction().await?;
            tx.execute_batch(include_str!("migrations/003_email_outbox_kind.sql"))
                .await?;
            tx.execute("PRAGMA user_version = 3", ()).await?;
            tx.commit().await?;
        }

        // Transition legacy linux_bug* tables from intermediate branch states if present.
        let has_legacy_linux_bugs = {
            let mut rows = self
                .conn
                .query(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'linux_bugs'",
                    (),
                )
                .await?;
            rows.next().await?.is_some()
        };
        if has_legacy_linux_bugs {
            info!("Dropping legacy linux_bug* tables and applying bugs schema...");
            let tx = self.conn.transaction().await?;
            tx.execute_batch(
                "DROP TABLE IF EXISTS linux_bug_vectors;
                 DROP TABLE IF EXISTS linux_bug_reviews;
                 DROP TABLE IF EXISTS linux_bug_subsystems;
                 DROP TABLE IF EXISTS linux_bug_enrichments;
                 DROP TABLE IF EXISTS linux_bugs;",
            )
            .await?;
            tx.execute_batch(include_str!("migrations/002_bugs.sql"))
                .await?;
            tx.commit().await?;
        }

        let has_bugs = {
            let mut rows = self
                .conn
                .query(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'bugs'",
                    (),
                )
                .await?;
            rows.next().await?.is_some()
        };
        if !has_bugs {
            info!("Applying database migration (bugs)...");
            let tx = self.conn.transaction().await?;
            tx.execute_batch(include_str!("migrations/002_bugs.sql"))
                .await?;
            tx.commit().await?;
        }

        // Version 4 runs after the compatibility blocks above rather than in
        // sequence with the others, because it rewrites rows in the bugs table
        // and those blocks are what create that table for databases left in an
        // intermediate branch state.
        if current_version < 4 {
            info!("Applying database migration version 4 (retire folded bug pipelines)...");
            let tx = self.conn.transaction().await?;
            tx.execute_batch(include_str!(
                "migrations/004_retire_folded_bug_pipelines.sql"
            ))
            .await?;
            tx.execute("PRAGMA user_version = 4", ()).await?;
            tx.commit().await?;
        }

        if current_version < 5 {
            info!("Applying database migration version 5 (Git patch IDs)...");
            let has_git_patch_id = {
                let mut columns = self.conn.query("PRAGMA table_info(patches)", ()).await?;
                let mut found = false;
                while let Some(row) = columns.next().await? {
                    let name: String = row.get(1)?;
                    found |= name == "git_patch_id";
                }
                found
            };
            let tx = self.conn.transaction().await?;
            if has_git_patch_id {
                // Builds predating the numbered migration may already have
                // the column while still reporting an older schema version.
                tx.execute(
                    "CREATE INDEX IF NOT EXISTS idx_patches_git_patch_id
                     ON patches(git_patch_id)",
                    (),
                )
                .await?;
            } else {
                tx.execute_batch(include_str!("migrations/005_git_patch_id.sql"))
                    .await?;
            }
            tx.execute("PRAGMA user_version = 5", ()).await?;
            tx.commit().await?;
        }

        info!("Database schema is up to date at version 5.");

        Ok(())
    }

    pub async fn get_mailing_list_id_by_name(&self, name: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM mailing_lists WHERE nntp_group = ?",
                libsql::params![name],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn add_message_to_mailing_list(
        &self,
        message_id: i64,
        mailing_list_id: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_mailing_lists (message_id, mailing_list_id) VALUES (?, ?)",
                libsql::params![message_id, mailing_list_id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_mailing_lists(&self) -> Result<Vec<(String, String)>> {
        let mut rows = self
            .conn
            .query("SELECT name, nntp_group FROM mailing_lists", ())
            .await?;
        let mut lists = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            lists.push((row.get(0)?, row.get(1)?));
        }
        Ok(lists)
    }

    pub async fn get_pending_review_id(
        &self,
        patchset_id: i64,
        patch_id: Option<i64>,
    ) -> Result<Option<i64>> {
        let mut rows = match patch_id {
            Some(pid) => {
                self.conn.query("SELECT id FROM reviews WHERE patchset_id = ? AND patch_id = ? AND status = 'Pending' LIMIT 1", libsql::params![patchset_id, pid]).await?
            }
            None => {
                self.conn.query("SELECT id FROM reviews WHERE patchset_id = ? AND patch_id IS NULL AND status = 'Pending' LIMIT 1", libsql::params![patchset_id]).await?
            }
        };
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn create_review(
        &self,
        patchset_id: i64,
        patch_id: Option<i64>,
        provider: &str,
        model: &str,
        baseline_id: Option<i64>,
        prompts_hash: Option<&str>,
    ) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "INSERT INTO reviews (patchset_id, patch_id, status, created_at, provider, model, baseline_id, prompts_hash)
             VALUES (?, ?, 'Pending', ?, ?, ?, ?, ?) RETURNING id",
                libsql::params![
                    patchset_id,
                    patch_id,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs() as i64,
                    provider,
                    model,
                    baseline_id,
                    prompts_hash
                ],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get review ID"))
        }
    }

    pub async fn has_successful_review(
        &self,
        patchset_id: i64,
        patch_id: i64,
        baseline_id: Option<i64>,
    ) -> Result<bool> {
        Ok(self
            .count_successful_reviews(patchset_id, patch_id, baseline_id)
            .await?
            > 0)
    }

    pub async fn count_successful_reviews(
        &self,
        patchset_id: i64,
        patch_id: i64,
        _baseline_id: Option<i64>,
    ) -> Result<usize> {
        let mut rows = self.conn
            .query(
                "SELECT COUNT(*) FROM reviews WHERE patchset_id = ? AND patch_id = ? AND status = 'Reviewed'",
                libsql::params![patchset_id, patch_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn has_failed_review(
        &self,
        patchset_id: i64,
        patch_id: i64,
        _baseline_id: Option<i64>,
    ) -> Result<bool> {
        let mut rows = self.conn
            .query(
                "SELECT 1 FROM reviews WHERE patchset_id = ? AND patch_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL",
                libsql::params![patchset_id, patch_id],
            )
            .await?;

        Ok(rows.next().await.ok().flatten().is_some())
    }

    pub async fn update_review_status(
        &self,
        review_id: i64,
        status: &str,
        logs: Option<&str>,
    ) -> Result<()> {
        if let Some(l) = logs {
            self.conn
                .execute(
                    "UPDATE reviews SET status = ?, logs = ? WHERE id = ?",
                    libsql::params![
                        status,
                        crate::compression::compress_string_if_needed(l),
                        review_id
                    ],
                )
                .await?;
        } else {
            self.conn
                .execute(
                    "UPDATE reviews SET status = ? WHERE id = ?",
                    libsql::params![status, review_id],
                )
                .await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn complete_review(
        &self,
        review_id: i64,
        status: &str,
        result: &str,
        summary: Option<&str>,
        interaction_id: Option<&str>,
        inline_review: Option<&str>,
        logs: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE reviews SET status = ?, result_description = ?, summary = ?, interaction_id = ?, inline_review = ?, logs = ? WHERE id = ?",
                libsql::params![status, result, summary, interaction_id, inline_review.map(crate::compression::compress_string_if_needed).unwrap_or(libsql::Value::Null), logs.map(crate::compression::compress_string_if_needed).unwrap_or(libsql::Value::Null), review_id],
            )
            .await?;
        Ok(())
    }

    pub async fn create_ai_interaction(&self, params: AiInteractionParams<'_>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO ai_interactions (id, parent_interaction_id, workflow_id, provider, model, input_context, output_raw, tokens_in, tokens_out, tokens_cached, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                params.id,
                params.parent_id,
                params.workflow_id,
                params.provider,
                params.model,
                crate::compression::compress_string_if_needed(params.input),
                crate::compression::compress_string_if_needed(params.output),
                params.tokens_in,
                params.tokens_out,
                params.tokens_cached,
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64
            ],
        ).await?;
        Ok(())
    }

    pub async fn create_tool_usage(&self, usage: ToolUsage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_usages (review_id, provider, model, tool_name, arguments, output_length, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                usage.review_id,
                usage.provider,
                usage.model,
                usage.tool_name,
                usage.arguments,
                usage.output_length,
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64
            ],
        ).await?;
        Ok(())
    }

    pub async fn update_tool_usage_length(
        &self,
        review_id: i64,
        tool_name: &str,
        arguments: &str,
        output_length: usize,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE tool_usages 
                 SET output_length = ? 
                 WHERE id = (
                     SELECT id FROM tool_usages 
                     WHERE review_id = ? AND tool_name = ? AND arguments = ? AND output_length = 0
                     ORDER BY id DESC LIMIT 1
                 )",
                libsql::params![output_length as i64, review_id, tool_name, arguments],
            )
            .await?;
        Ok(())
    }

    pub async fn create_finding(&self, finding: Finding) -> Result<()> {
        let val = finding.preexisting.map(|b| if b { 1 } else { 0 });
        let locations_val = finding
            .locations
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        self.conn
            .execute(
                "INSERT INTO findings (review_id, severity, severity_explanation, problem, preexisting, locations)
             VALUES (?, ?, ?, ?, ?, ?)",
                libsql::params![
                    finding.review_id,
                    finding.severity as i32,
                    finding.severity_explanation,
                    finding.problem,
                    val,
                    locations_val,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn create_bug(&self, bug: &NewBug) -> Result<i64> {
        self.create_bug_with_enrichment(bug, None).await
    }

    /// Queue the candidate and its original evidence atomically so a worker cannot
    /// pick up a raw bug before its discovery record has been saved.
    pub async fn create_bug_with_enrichment(
        &self,
        bug: &NewBug,
        enrichment: Option<&NewBugEnrichment>,
    ) -> Result<i64> {
        let tx = self.conn.transaction().await?;
        let scoped = self.with_connection((*tx).clone());
        let id = scoped.insert_bug(bug).await?;
        if let Some(enrichment) = enrichment {
            scoped.add_bug_enrichment(id, enrichment).await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    async fn insert_bug(&self, bug: &NewBug) -> Result<i64> {
        let now = if bug.reported_at > 0 {
            bug.reported_at
        } else {
            chrono::Utc::now().timestamp()
        };
        let assignee = bug
            .assignee
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let assigned_at = assignee.as_ref().map(|_| now);
        let mut rows = self
            .conn
            .query(
                "INSERT INTO bugs (
                    bugid, title, lifecycle_status, pipeline_state, reporter, reported_at,
                    assignee, assigned_at,
                    discovered_in_patchset_id, discovered_in_patch_id,
                    discovered_in_commit, source_ref, duplicate_of_id,
                    created_at, updated_at, audit_author, audit_tool, audit_model
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 RETURNING id",
                libsql::params![
                    bug.bugid.as_str(),
                    bug.title.as_str(),
                    bug.lifecycle_status.as_str(),
                    bug.pipeline_state.as_str(),
                    bug.reporter.as_str(),
                    now,
                    assignee,
                    assigned_at,
                    bug.discovered_in_patchset_id,
                    bug.discovered_in_patch_id,
                    bug.discovered_in_commit.clone(),
                    bug.source_ref.clone(),
                    bug.duplicate_of_id,
                    now,
                    now,
                    self.bug_actor.as_str(),
                    self.bug_tool.as_str(),
                    self.bug_model.clone(),
                ],
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            for sub in &bug.subsystems {
                let trimmed = sub.name.trim();
                if !trimmed.is_empty() {
                    self.conn
                        .execute(
                            UPSERT_BUG_SUBSYSTEM_SQL,
                            libsql::params![id, trimmed, sub.source.as_str()],
                        )
                        .await?;
                }
            }
            if let Some(vector_json) = bug.vector_json.as_deref() {
                self.store_bug_vector(id, vector_json).await?;
            }
            Ok(id)
        } else {
            bail!("Failed to insert bug: no id returned");
        }
    }

    /// Records a deduplication embedding for a bug.
    ///
    /// Keyed by the model that produced it, so switching embedding models adds a
    /// row rather than destroying the previous vector.
    async fn store_bug_vector(&self, bug_id: i64, vector_json: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO bug_vectors (bug_id, model, vector_json, created_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(bug_id, model) DO UPDATE SET
                     vector_json = excluded.vector_json,
                     created_at = excluded.created_at",
                libsql::params![
                    bug_id,
                    self.bug_model.clone().unwrap_or_default(),
                    vector_json,
                    chrono::Utc::now().timestamp(),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn add_bug_enrichment(
        &self,
        bug_id: i64,
        enrichment: &NewBugEnrichment,
    ) -> Result<i64> {
        if self.bug_claim.is_none() {
            return self.insert_bug_enrichment(bug_id, enrichment).await;
        }
        let tx = self.begin_bug_write(bug_id).await?;
        let id = self
            .with_connection((*tx).clone())
            .insert_bug_enrichment(bug_id, enrichment)
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    async fn insert_bug_enrichment(
        &self,
        bug_id: i64,
        enrichment: &NewBugEnrichment,
    ) -> Result<i64> {
        let compressed_content = enrichment
            .content
            .as_ref()
            .map(|c| crate::compression::compress_string_if_needed(c))
            .unwrap_or(libsql::Value::Null);
        let compressed_logs = enrichment
            .logs
            .as_ref()
            .map(|l| crate::compression::compress_string_if_needed(l))
            .unwrap_or(libsql::Value::Null);
        let data_json_str = enrichment
            .data_json
            .as_ref()
            .and_then(|d| serde_json::to_string(d).ok());
        let now = if enrichment.created_at > 0 {
            enrichment.created_at
        } else {
            chrono::Utc::now().timestamp()
        };

        let mut rows = self
            .conn
            .query(
                "INSERT INTO bug_enrichments (
                    bug_id, kind, tool, model, author, created_at, content, data_json,
                    tokens_in, tokens_out, tokens_cached, logs
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 RETURNING id",
                libsql::params![
                    bug_id,
                    enrichment.kind.as_str(),
                    if enrichment.tool.is_empty() {
                        self.bug_tool.as_str()
                    } else {
                        enrichment.tool.as_str()
                    },
                    enrichment.model.clone().or_else(|| self.bug_model.clone()),
                    enrichment
                        .author
                        .clone()
                        .or_else(|| Some(self.bug_actor.clone())),
                    now,
                    compressed_content,
                    data_json_str,
                    enrichment.tokens_in.map(|t| t as i64),
                    enrichment.tokens_out.map(|t| t as i64),
                    enrichment.tokens_cached.map(|t| t as i64),
                    compressed_logs,
                ],
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let eid: i64 = row.get(0)?;
            Ok(eid)
        } else {
            bail!("Failed to insert bug enrichment: no id returned");
        }
    }

    pub async fn get_bug_enrichments(&self, bug_id: i64) -> Result<Vec<BugEnrichment>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, bug_id, kind, tool, model, author, created_at, content, data_json,
                        tokens_in, tokens_out, tokens_cached, NULL as logs
                 FROM bug_enrichments
                 WHERE bug_id = ?
                 ORDER BY created_at ASC, id ASC",
                libsql::params![bug_id],
            )
            .await?;

        let mut list = Vec::new();
        while let Some(row) = rows.next().await? {
            list.push(Self::parse_bug_enrichment_row(&row)?);
        }
        Ok(list)
    }

    fn parse_bug_enrichment_row(row: &libsql::Row) -> Result<BugEnrichment> {
        let id: i64 = row.get(0)?;
        let bug_id: i64 = row.get(1)?;
        let kind: String = row.get(2)?;
        let tool: String = row.get(3)?;
        let model: Option<String> = row.get(4).ok().flatten();
        let author: Option<String> = row.get(5).ok().flatten();
        let created_at: i64 = row.get(6)?;
        let content: Option<String> = crate::compression::get_compressed_string_opt(row, 7)
            .unwrap_or(None)
            .or_else(|| row.get::<Option<String>>(7).ok().flatten());
        let data_json_str: Option<String> = row.get(8).ok().flatten();
        let data_json: Option<serde_json::Value> =
            data_json_str.and_then(|s| serde_json::from_str(&s).ok());
        let tokens_in: Option<usize> = row.get::<Option<i64>>(9).ok().flatten().map(|v| v as usize);
        let tokens_out: Option<usize> = row
            .get::<Option<i64>>(10)
            .ok()
            .flatten()
            .map(|v| v as usize);
        let tokens_cached: Option<usize> = row
            .get::<Option<i64>>(11)
            .ok()
            .flatten()
            .map(|v| v as usize);
        let logs: Option<String> = crate::compression::get_compressed_string_opt(row, 12)
            .unwrap_or(None)
            .or_else(|| row.get::<Option<String>>(12).ok().flatten());

        Ok(BugEnrichment {
            id,
            bug_id,
            kind,
            tool,
            model,
            author,
            created_at,
            content,
            data_json,
            tokens_in,
            tokens_out,
            tokens_cached,
            logs,
        })
    }

    pub async fn get_bug(&self, id: i64) -> Result<Option<Bug>> {
        let mut rows = self
            .conn
            .query(
                &format!("SELECT {BUG_ROW_COLUMNS} FROM bugs WHERE id = ?"),
                libsql::params![id],
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let mut bug = Self::parse_bug_row_core(&row)?;
            bug.subsystems = self
                .get_subsystems_for_bug(bug.id)
                .await
                .unwrap_or_default();
            bug.enrichments = self.get_bug_enrichments(bug.id).await?;
            Ok(Some(bug))
        } else {
            Ok(None)
        }
    }

    pub async fn get_bug_by_bugid(&self, bugid: &str) -> Result<Option<Bug>> {
        let mut rows = self
            .conn
            .query(
                &format!("SELECT {BUG_ROW_COLUMNS} FROM bugs WHERE bugid = ?"),
                libsql::params![bugid],
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let mut bug = Self::parse_bug_row_core(&row)?;
            bug.subsystems = self
                .get_subsystems_for_bug(bug.id)
                .await
                .unwrap_or_default();
            bug.enrichments = self.get_bug_enrichments(bug.id).await?;
            Ok(Some(bug))
        } else {
            Ok(None)
        }
    }

    pub async fn get_bug_by_slug(&self, slug: &str) -> Result<Option<Bug>> {
        self.get_bug_by_bugid(slug).await
    }

    pub async fn get_subsystems_for_bug(&self, bug_id: i64) -> Result<Vec<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT subsystem FROM bug_subsystems WHERE bug_id = ? ORDER BY subsystem ASC",
                libsql::params![bug_id],
            )
            .await?;
        let mut subs = Vec::new();
        while let Some(row) = rows.next().await? {
            subs.push(row.get(0)?);
        }
        Ok(subs)
    }

    /// Returns the MAINTAINERS section titles attributed to this bug.
    ///
    /// Rows whose provenance is a directory prefix or a caller-supplied string
    /// are excluded, because they name nobody and therefore confer no
    /// authority. Authorization must call this rather than read
    /// `Bug::subsystems`, which `parse_bug_row_core` always leaves empty.
    pub async fn authorizing_sections_for_bug(&self, bug_id: i64) -> Result<Vec<String>> {
        Ok(self
            .authorizing_sections_for_bugs(&[bug_id])
            .await?
            .remove(&bug_id)
            .unwrap_or_default())
    }

    /// Batch form of [`Database::authorizing_sections_for_bug`], so that
    /// filtering a page of bugs costs one query rather than one per bug.
    pub async fn authorizing_sections_for_bugs(
        &self,
        bug_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<String>>> {
        let mut sections: std::collections::HashMap<i64, Vec<String>> =
            std::collections::HashMap::new();
        if bug_ids.is_empty() {
            return Ok(sections);
        }
        let placeholders = vec!["?"; bug_ids.len()].join(", ");
        let sql = format!(
            "SELECT bug_id, subsystem FROM bug_subsystems
             WHERE source = '{}' AND bug_id IN ({})
             ORDER BY subsystem ASC",
            SubsystemSource::MaintainersSection.as_str(),
            placeholders
        );
        let params: Vec<libsql::Value> = bug_ids
            .iter()
            .map(|&id| libsql::Value::Integer(id))
            .collect();
        let mut rows = self.conn.query(&sql, params).await?;
        while let Some(row) = rows.next().await? {
            let bug_id: i64 = row.get(0)?;
            let subsystem: String = row.get(1)?;
            sections.entry(bug_id).or_default().push(subsystem);
        }
        Ok(sections)
    }

    /// Each candidate is a discovery; analysis stages never increase this count.
    /// UNION makes historical malformed duplicate graphs terminate safely.
    pub async fn bug_family(&self, id: i64, raw: bool) -> Result<Vec<Bug>> {
        let mut rows = self
            .conn
            .query(
                "WITH RECURSIVE ancestors(id, parent) AS (
                SELECT id, duplicate_of_id FROM bugs WHERE id = ?
                UNION SELECT b.id, b.duplicate_of_id FROM bugs b JOIN ancestors a ON b.id = a.parent
             ), family(id) AS (
                SELECT id FROM ancestors
                UNION SELECT b.id FROM bugs b JOIN family f ON b.duplicate_of_id = f.id
             ) SELECT id FROM family ORDER BY id",
                libsql::params![id],
            )
            .await?;
        let mut family = Vec::new();
        while let Some(row) = rows.next().await? {
            if let Some(mut bug) = self.get_bug(row.get(0)?).await? {
                if raw {
                    let mut records = self.conn.query("SELECT id, bug_id, kind, tool, model, author, created_at, content, data_json, tokens_in, tokens_out, tokens_cached, logs FROM bug_enrichments WHERE bug_id = ? ORDER BY created_at, id", libsql::params![bug.id]).await?;
                    bug.enrichments.clear();
                    while let Some(record) = records.next().await? {
                        bug.enrichments
                            .push(Self::parse_bug_enrichment_row(&record)?);
                    }
                }
                family.push(bug);
            }
        }
        Ok(family)
    }

    /// Fetch list-page evidence in one query without loading reports or payloads.
    pub async fn bug_discovery_summaries(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, serde_json::Value>> {
        let mut rows = self.conn.query(
            "WITH RECURSIVE ancestors(root, id, parent) AS (
                SELECT b.id, b.id, b.duplicate_of_id FROM bugs b JOIN json_each(?) requested ON b.id = requested.value
                UNION SELECT a.root, b.id, b.duplicate_of_id FROM bugs b JOIN ancestors a ON b.id = a.parent
             ), family(root, id) AS (
                SELECT root, id FROM ancestors
                UNION SELECT f.root, b.id FROM bugs b JOIN family f ON b.duplicate_of_id = f.id
             ) SELECT f.root,
                      COALESCE(
                          NULLIF(trim(e.model), ''),
                          (SELECT r.model FROM bug_reviews rb JOIN reviews r ON r.id = rb.review_id WHERE rb.bug_id = f.id AND r.model IS NOT NULL AND trim(r.model) != '' LIMIT 1),
                          (SELECT r.model FROM bug_reviews rb JOIN reviews r ON r.id = rb.review_id WHERE rb.bug_id = b.duplicate_of_id AND r.model IS NOT NULL AND trim(r.model) != '' LIMIT 1),
                          (SELECT r.model FROM reviews r WHERE r.patchset_id = b.discovered_in_patchset_id AND r.model IS NOT NULL AND trim(r.model) != '' LIMIT 1)
                      ) AS model,
                      COALESCE(
                          NULLIF(trim(e.tool), ''),
                          (SELECT 'sashiko:linux_patch_review' FROM bug_reviews rb JOIN reviews r ON r.id = rb.review_id WHERE rb.bug_id = f.id LIMIT 1),
                          (SELECT 'sashiko:linux_patch_review' FROM bug_reviews rb JOIN reviews r ON r.id = rb.review_id WHERE rb.bug_id = b.duplicate_of_id LIMIT 1),
                          (SELECT 'sashiko:linux_patch_review' FROM reviews r WHERE r.patchset_id = b.discovered_in_patchset_id LIMIT 1)
                      ) AS tool
               FROM family f
               JOIN bugs b ON b.id = f.id
               LEFT JOIN bug_enrichments e ON e.bug_id = f.id AND e.kind IN ('candidate', 'discovery')",
            libsql::params![serde_json::to_string(ids)?]).await?;
        #[derive(Default, Serialize)]
        struct Summary {
            count: usize,
            models: std::collections::BTreeSet<String>,
            tools: std::collections::BTreeSet<String>,
            unknown_models: usize,
        }
        let mut summaries: std::collections::HashMap<i64, Summary> =
            std::collections::HashMap::new();
        while let Some(row) = rows.next().await? {
            let summary = summaries.entry(row.get(0)?).or_default();
            summary.count += 1;
            if let Some(model) = row
                .get::<Option<String>>(1)?
                .filter(|s| !s.trim().is_empty())
            {
                summary.models.insert(model);
            } else {
                summary.unknown_models += 1;
            }
            if let Some(tool) = row
                .get::<Option<String>>(2)?
                .filter(|s| !s.trim().is_empty())
            {
                summary.tools.insert(tool);
            }
        }
        summaries
            .into_iter()
            .map(|(id, summary)| Ok((id, serde_json::to_value(summary)?)))
            .collect()
    }

    pub async fn resolve_bug_model_and_tool(
        &self,
        bug_id: i64,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT
                    COALESCE(
                        (SELECT r.model FROM bug_reviews rb JOIN reviews r ON r.id = rb.review_id WHERE rb.bug_id = b.id AND r.model IS NOT NULL AND trim(r.model) != '' LIMIT 1),
                        (SELECT r.model FROM bug_reviews rb JOIN reviews r ON r.id = rb.review_id WHERE rb.bug_id = b.duplicate_of_id AND r.model IS NOT NULL AND trim(r.model) != '' LIMIT 1),
                        (SELECT r.model FROM reviews r WHERE r.patchset_id = b.discovered_in_patchset_id AND r.model IS NOT NULL AND trim(r.model) != '' LIMIT 1)
                    ) AS model,
                    COALESCE(
                        (SELECT 'sashiko:linux_patch_review' FROM bug_reviews rb JOIN reviews r ON r.id = rb.review_id WHERE rb.bug_id = b.id LIMIT 1),
                        (SELECT 'sashiko:linux_patch_review' FROM bug_reviews rb JOIN reviews r ON r.id = rb.review_id WHERE rb.bug_id = b.duplicate_of_id LIMIT 1),
                        (SELECT 'sashiko:linux_patch_review' FROM reviews r WHERE r.patchset_id = b.discovered_in_patchset_id LIMIT 1)
                    ) AS tool
                 FROM bugs b WHERE b.id = ?",
                libsql::params![bug_id],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            let model: Option<String> = row.get(0).ok().flatten();
            let tool: Option<String> = row.get(1).ok().flatten();
            if model.is_none() && tool.is_none() {
                Ok(None)
            } else {
                Ok(Some((model, tool)))
            }
        } else {
            Ok(None)
        }
    }

    /// Builds evidence only from the family members authorized by the caller.
    pub async fn bug_evidence(&self, family: &[Bug]) -> Result<serde_json::Value> {
        struct RawDiscovery<'a> {
            bug_id: i64,
            bugid: String,
            reporter: String,
            reported_at: i64,
            record: Option<&'a BugEnrichment>,
            patchset_id: Option<i64>,
            patch_id: Option<i64>,
            commit: Option<String>,
            model: Option<String>,
            tool: Option<String>,
        }

        let mut raw_discoveries = Vec::new();
        let mut patch_ids_to_query = std::collections::HashSet::new();
        let mut bugs_needing_review_lookup = Vec::new();
        let mut activity = Vec::new();
        let mut models = std::collections::BTreeSet::new();
        let mut tools = std::collections::BTreeSet::new();
        let mut unknown_models = 0;

        // Interaction logs are deliberately not loaded with the family, so ask
        // the table which records carry one before assembling the events.
        let family_ids: Vec<i64> = family.iter().map(|b| b.id).collect();
        let mut logged_records = std::collections::HashSet::<i64>::new();
        if !family_ids.is_empty() {
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM bug_enrichments
                     WHERE bug_id IN (SELECT value FROM json_each(?))
                       AND logs IS NOT NULL AND length(logs) > 0",
                    libsql::params![serde_json::to_string(&family_ids)?],
                )
                .await?;
            while let Some(row) = rows.next().await? {
                logged_records.insert(row.get(0)?);
            }
        }

        for bug in family {
            let candidates: Vec<_> = bug
                .enrichments
                .iter()
                .filter(|e| {
                    e.kind == "candidate" || e.kind == "discovery" || e.kind == "raw_candidate"
                })
                .collect();
            // A legacy report without a candidate still represents one discovery.
            let records: Vec<Option<&BugEnrichment>> = if candidates.is_empty() {
                vec![None]
            } else {
                candidates.into_iter().map(Some).collect()
            };
            for record in records {
                let mut model = record
                    .and_then(|e| e.model.as_deref())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string());
                let mut tool = record
                    .map(|e| e.tool.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string());

                if (model.is_none() || tool.is_none())
                    && let Ok(Some((fallback_model, fallback_tool))) =
                        self.resolve_bug_model_and_tool(bug.id).await
                {
                    if model.is_none() {
                        model = fallback_model;
                    }
                    if tool.is_none() {
                        tool = fallback_tool;
                    }
                }

                if let Some(ref m) = model {
                    models.insert(m.clone());
                } else {
                    unknown_models += 1;
                }
                if let Some(ref t) = tool {
                    tools.insert(t.clone());
                }

                let patchset_id = record
                    .and_then(|e| e.data_json.as_ref())
                    .and_then(|d| d.get("patchset_id"))
                    .and_then(|v| v.as_i64())
                    .or(bug.discovered_in_patchset_id);
                let patch_id = record
                    .and_then(|e| e.data_json.as_ref())
                    .and_then(|d| d.get("patch_id"))
                    .and_then(|v| v.as_i64())
                    .or(bug.discovered_in_patch_id);
                let commit = record
                    .and_then(|e| e.data_json.as_ref())
                    .and_then(|d| d.get("commit_sha"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| bug.discovered_in_commit.clone());

                if let Some(pid) = patch_id {
                    patch_ids_to_query.insert(pid);
                }
                if patch_id.is_none() || patchset_id.is_none() {
                    bugs_needing_review_lookup.push(bug.id);
                }

                raw_discoveries.push(RawDiscovery {
                    bug_id: bug.id,
                    bugid: bug.bugid.clone(),
                    reporter: bug.reporter.clone(),
                    reported_at: bug.reported_at,
                    record,
                    patchset_id,
                    patch_id,
                    commit,
                    model,
                    tool,
                });
            }
            for enrichment in &bug.enrichments {
                let mut event = serde_json::to_value(enrichment)?;
                event["bugid"] = json!(bug.bugid);
                // The interaction log itself stays behind the raw endpoint, but
                // callers need to know whether one exists to offer a link to it.
                event["has_logs"] = json!(logged_records.contains(&enrichment.id));
                // Payloads and token accounting are only exposed by the raw endpoint.
                // BugEnrichment serializes as a JSON object.
                let obj = event
                    .as_object_mut()
                    .expect("serialized enrichment is an object");
                for key in [
                    "data_json",
                    "logs",
                    "tokens_in",
                    "tokens_out",
                    "tokens_cached",
                ] {
                    obj.remove(key);
                }
                if enrichment.kind == "audit"
                    && let Some(data) = &enrichment.data_json
                {
                    match data["field"].as_str() {
                        Some("duplicate_of_id") => {
                            event["content"] = json!("Duplicate relationship updated")
                        }
                        Some("status") => {
                            event["content"] = json!(format!(
                                "Status changed from {} to {}",
                                data["old"].as_str().unwrap_or("unknown"),
                                data["new"].as_str().unwrap_or("unknown")
                            ))
                        }
                        Some("title") => {
                            event["content"] = json!(format!(
                                "Title changed to {}",
                                data["new"].as_str().unwrap_or("untitled")
                            ))
                        }
                        _ => {}
                    }
                }
                // Old deduplication content could itself be a JSON payload.
                if enrichment.kind == "deduplication"
                    && let Some(content) = &enrichment.content
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(content)
                {
                    event["content"] = value
                        .get("reasoning")
                        .cloned()
                        .unwrap_or(json!("Matched an existing bug"));
                }
                activity.push(event);
            }
        }

        // If some discoveries have missing patch_id and patchset_id, query review_bugs
        if !bugs_needing_review_lookup.is_empty() {
            let mut review_links: std::collections::HashMap<i64, (Option<i64>, Option<i64>)> =
                std::collections::HashMap::new();
            let mut rows = self
                .conn
                .query(
                    "SELECT rb.bug_id, r.patchset_id, r.patch_id
                     FROM bug_reviews rb
                     JOIN reviews r ON r.id = rb.review_id
                     WHERE rb.bug_id IN (SELECT value FROM json_each(?))
                     ORDER BY r.id ASC",
                    libsql::params![serde_json::to_string(&bugs_needing_review_lookup)?],
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let b_id: i64 = row.get(0)?;
                let ps_id: Option<i64> = row.get(1).ok().flatten();
                let p_id: Option<i64> = row.get(2).ok().flatten();
                review_links.entry(b_id).or_insert((ps_id, p_id));
            }
            for raw in &mut raw_discoveries {
                if (raw.patch_id.is_none() || raw.patchset_id.is_none())
                    && let Some(&(ps_id, p_id)) = review_links.get(&raw.bug_id)
                {
                    if raw.patchset_id.is_none() {
                        raw.patchset_id = ps_id;
                    }
                    if raw.patch_id.is_none() {
                        raw.patch_id = p_id;
                        if let Some(pid) = p_id {
                            patch_ids_to_query.insert(pid);
                        }
                    }
                }
            }
        }

        // Query patches table to resolve part_index and ensure patchset_id is populated
        let mut patch_info: std::collections::HashMap<i64, (Option<i64>, Option<i64>)> =
            std::collections::HashMap::new();
        if !patch_ids_to_query.is_empty() {
            let pids: Vec<i64> = patch_ids_to_query.into_iter().collect();
            let mut rows = self
                .conn
                .query(
                    "SELECT id, patchset_id, part_index
                     FROM patches
                     WHERE id IN (SELECT value FROM json_each(?))",
                    libsql::params![serde_json::to_string(&pids)?],
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let pid: i64 = row.get(0)?;
                let ps_id: Option<i64> = row.get(1).ok().flatten();
                let part: Option<i64> = row.get(2).ok().flatten();
                patch_info.insert(pid, (ps_id, part));
            }
        }

        let mut discoveries = Vec::new();
        for raw in raw_discoveries {
            let mut patchset_id = raw.patchset_id;
            let mut patch_part = None;
            if let Some(pid) = raw.patch_id
                && let Some(&(p_ps_id, part)) = patch_info.get(&pid)
            {
                if patchset_id.is_none() {
                    patchset_id = p_ps_id;
                }
                patch_part = part;
            }

            let author = raw
                .record
                .and_then(|e| e.author.as_deref())
                .unwrap_or(&raw.reporter);
            let author = if author == "sashiko" && self.has_bug_actor() {
                self.bug_actor()
            } else {
                author
            };

            // Only discoveries that stored the payload handed to the workflow
            // can offer a raw input view.
            let has_input = raw
                .record
                .is_some_and(|e| e.data_json.is_some() || e.content.is_some());

            discoveries.push(json!({
                "bug_id": raw.bug_id,
                "bugid": raw.bugid,
                "enrichment_id": raw.record.map(|e| e.id),
                "author": author,
                "tool": raw.tool,
                "model": raw.model,
                "created_at": raw.record.map(|e| e.created_at).unwrap_or(raw.reported_at),
                "patchset_id": patchset_id,
                "patch_id": raw.patch_id,
                "patch_part": patch_part,
                "commit": raw.commit,
                "has_input": has_input,
                "legacy": raw.record.is_none()
            }));
        }

        activity.sort_by_key(|e| {
            (
                e["created_at"].as_i64().unwrap_or(0),
                e["id"].as_i64().unwrap_or(0),
            )
        });
        activity.reverse();
        Ok(
            json!({ "count": discoveries.len(), "models": models, "tools": tools,
            "unknown_models": unknown_models, "discoveries": discoveries, "activity": activity }),
        )
    }

    pub async fn change_bug_status_with_reason(
        &self,
        id: i64,
        status: BugLifecycleStatus,
        reason: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.transaction().await?;
        let now = chrono::Utc::now().timestamp();
        tx.execute("UPDATE bugs SET lifecycle_status = ?, updated_at = ?, audit_author = ?, audit_tool = ?, audit_model = ? WHERE id = ?",
            libsql::params![status.as_str(), now, self.bug_actor.as_str(), self.bug_tool.as_str(), self.bug_model.clone(), id]).await?;
        if let Some(reason) = reason.filter(|s| !s.trim().is_empty()) {
            tx.execute("INSERT INTO bug_enrichments (bug_id, kind, tool, author, model, created_at, content) VALUES (?, 'comment', ?, ?, ?, ?, ?)",
                libsql::params![id, self.bug_tool.as_str(), self.bug_actor.as_str(), self.bug_model.clone(), now, reason]).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records who is working on a bug, or clears the assignment when given
    /// `None`.
    ///
    /// The address is stored as plain text on purpose: Sashiko keeps no
    /// persistent user records, so there is nothing to reference.
    ///
    /// Both columns are written in one statement because the schema requires
    /// `assigned_at` to be set if and only if there is an assignee. An
    /// optional reason is recorded as a comment so the audit feed explains the
    /// handover rather than just noting that it happened.
    ///
    /// Returns false when no such bug exists, so the caller can tell a bad id
    /// apart from a successful assignment.
    pub async fn assign_bug(
        &self,
        id: i64,
        assignee: Option<&str>,
        reason: Option<&str>,
    ) -> Result<bool> {
        let assignee = assignee.map(str::trim).filter(|s| !s.is_empty());
        let now = chrono::Utc::now().timestamp();
        let tx = self.conn.transaction().await?;
        let updated = tx
            .execute(
                "UPDATE bugs
                    SET assignee = ?1,
                        assigned_at = CASE WHEN ?1 IS NULL THEN NULL ELSE ?2 END,
                        updated_at = ?2,
                        audit_author = ?3,
                        audit_tool = ?4,
                        audit_model = ?5
                  WHERE id = ?6",
                libsql::params![
                    assignee,
                    now,
                    self.bug_actor.as_str(),
                    self.bug_tool.as_str(),
                    self.bug_model.clone(),
                    id,
                ],
            )
            .await?;
        if updated == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        if let Some(reason) = reason.map(str::trim).filter(|s| !s.is_empty()) {
            tx.execute(
                "INSERT INTO bug_enrichments (bug_id, kind, tool, author, model, created_at, content) VALUES (?, 'comment', ?, ?, ?, ?, ?)",
                libsql::params![id, self.bug_tool.as_str(), self.bug_actor.as_str(), self.bug_model.clone(), now, reason],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn get_bug_logs(&self, id: i64) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT tool, logs FROM bug_enrichments
                 WHERE bug_id = ? AND logs IS NOT NULL
                 ORDER BY created_at ASC, id ASC",
                libsql::params![id],
            )
            .await?;
        let mut combined = Vec::new();
        while let Some(row) = rows.next().await? {
            let tool: String = row.get(0)?;
            let logs_opt: Option<String> = crate::compression::get_compressed_string_opt(&row, 1)
                .unwrap_or(None)
                .or_else(|| row.get::<Option<String>>(1).ok().flatten());
            if let Some(logs_str) = logs_opt {
                if let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&logs_str) {
                    combined.extend(entries);
                } else if let Ok(val) = serde_json::from_str::<serde_json::Value>(&logs_str) {
                    combined.push(val);
                } else {
                    combined.push(serde_json::json!({
                        "role": tool,
                        "parts": [{"text": logs_str}]
                    }));
                }
            }
        }
        if combined.is_empty() {
            Ok(None)
        } else {
            Ok(serde_json::to_string(&combined).ok())
        }
    }

    pub async fn get_bug_logs_by_bugid(&self, bugid: &str) -> Result<Option<String>> {
        if let Some(bug) = self.get_bug_by_bugid(bugid).await? {
            self.get_bug_logs(bug.id).await
        } else {
            Ok(None)
        }
    }

    pub async fn get_bug_logs_by_slug(&self, slug: &str) -> Result<Option<String>> {
        self.get_bug_logs_by_bugid(slug).await
    }

    /// Claims the oldest bug awaiting analysis, taking a lease on it.
    ///
    /// The claim is a single statement so that two workers racing for the same
    /// bug cannot both win: SQLite serialises writers, so the loser's subquery
    /// no longer selects the row. The previous read-then-write version could
    /// hand the same bug to both.
    ///
    /// Claimable bugs are those awaiting a first attempt, those whose last
    /// attempt failed, and those whose lease has expired because the worker
    /// holding it died. Bugs that have exhausted `max_attempts` are skipped;
    /// [`Self::abandon_exhausted_bugs`] moves them to the dead letter state.
    /// Bugs already folded into a canonical bug are skipped too: their finding
    /// lives on the canonical row, so analysing them again would spend the
    /// budget to rediscover something that is already recorded.
    ///
    /// `worker_id` identifies the holder so that a stuck lease can be traced
    /// back to a process.
    pub async fn claim_pending_bug(
        &self,
        worker_id: &str,
        lease_ttl_seconds: i64,
        max_attempts: i64,
    ) -> Result<Option<Bug>> {
        let now = chrono::Utc::now().timestamp();
        let mut rows = self
            .conn
            .query(
                "UPDATE bugs
                    SET pipeline_state = 'running',
                        locked_by = ?1,
                        lease_expires_at = ?2,
                        attempt_count = attempt_count + 1,
                        updated_at = ?3,
                        audit_author = 'system',
                        audit_tool = 'sashiko:linux_bug',
                        audit_model = NULL
                  WHERE id = (
                      SELECT id FROM bugs
                       WHERE attempt_count < ?4
                         AND lifecycle_status != 'duplicate'
                         AND (pipeline_state IN ('pending', 'failed')
                              OR (pipeline_state = 'running'
                                  AND (lease_expires_at IS NULL OR lease_expires_at < ?3)))
                       ORDER BY created_at ASC
                       LIMIT 1
                  )
                  RETURNING id",
                libsql::params![worker_id, now + lease_ttl_seconds, now, max_attempts],
            )
            .await?;
        match rows.next().await? {
            Some(row) => self.get_bug(row.get::<i64>(0)?).await,
            None => Ok(None),
        }
    }

    /// Releases the lease on a bug that finished analysis.
    ///
    /// The pipeline state is left alone: whoever completed the run has already
    /// recorded the outcome, and overwriting it here would race with them.
    pub async fn release_bug_lease(&self, id: i64) -> Result<()> {
        let tx = self.begin_bug_write(id).await?;
        tx.execute(
            "UPDATE bugs SET locked_by = NULL, lease_expires_at = NULL WHERE id = ?",
            libsql::params![id],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Extends the lease on a bug this worker is still analysing.
    ///
    /// Returns false when the claim is gone, which means another worker has
    /// already taken the bug over. The caller cannot win that race back, so it
    /// should stop rather than keep spending on work that will be discarded.
    ///
    /// The worker is matched on purpose: renewing by id alone would let a
    /// worker whose lease already lapsed steal the row back from whoever
    /// legitimately claimed it next.
    pub async fn renew_bug_lease(
        &self,
        id: i64,
        worker_id: &str,
        lease_ttl_seconds: i64,
    ) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let updated = self
            .conn
            .execute(
                "UPDATE bugs
                    SET lease_expires_at = ?1
                  WHERE id = ?2
                    AND locked_by = ?3
                    AND pipeline_state IN ('running', 'succeeded')
                    AND lease_expires_at >= ?4",
                libsql::params![now + lease_ttl_seconds, id, worker_id, now],
            )
            .await?;
        Ok(updated > 0)
    }

    /// Moves bugs that have used up their attempts into the dead letter state.
    ///
    /// Abandoned bugs are never claimed again. Requeueing one is a deliberate
    /// operator action, so that a bug which reliably crashes the worker cannot
    /// quietly consume the analysis budget forever.
    pub async fn abandon_exhausted_bugs(&self, max_attempts: i64) -> Result<usize> {
        let now = chrono::Utc::now().timestamp();
        let count = self
            .conn
            .execute(
                "UPDATE bugs
                    SET pipeline_state = 'abandoned',
                        locked_by = NULL,
                        lease_expires_at = NULL,
                        updated_at = ?1,
                        audit_author = 'system',
                        audit_tool = 'sashiko:linux_bug',
                        audit_model = NULL
                  WHERE attempt_count >= ?2
                    AND (pipeline_state IN ('pending', 'failed')
                         OR (pipeline_state = 'running'
                             AND (lease_expires_at IS NULL OR lease_expires_at < ?1)))",
                libsql::params![now, max_attempts],
            )
            .await?;
        if count > 0 {
            info!(
                "Abandoned {} bugs that exhausted their analysis attempts",
                count
            );
        }
        Ok(count as usize)
    }

    /// Requeues bugs that are marked running but hold no valid lease.
    ///
    /// Only the pipeline state is touched. Triage state is owned by humans and
    /// must survive a crash untouched.
    ///
    /// Claiming already reclaims expired leases on its own, so this exists to
    /// make the requeue visible in the bug list rather than leaving a dead
    /// worker's bugs displayed as running until someone happens to claim them.
    ///
    /// A missing lease counts as reclaimable alongside an expired one. Claiming
    /// sets the state and the lease in one statement, so a running bug without
    /// a lease is always the residue of a release that skipped the state, and
    /// matching only on `lease_expires_at < now` would silently skip it forever
    /// because a NULL comparison is never true.
    pub async fn recover_stale_running_bugs(&self) -> Result<usize> {
        let now = chrono::Utc::now().timestamp();
        let count = self
            .conn
            .execute(
                "UPDATE bugs
                    SET pipeline_state = 'pending',
                        locked_by = NULL,
                        lease_expires_at = NULL,
                        updated_at = ?1,
                        audit_author = 'system',
                        audit_tool = 'sashiko:linux_bug',
                        audit_model = NULL
                  WHERE pipeline_state = 'running'
                    AND (lease_expires_at IS NULL OR lease_expires_at < ?1)",
                libsql::params![now],
            )
            .await?;
        if count > 0 {
            info!("Requeued {} bugs whose analysis lease expired", count);
        }
        Ok(count as usize)
    }

    /// Updates the triage state of a bug.
    pub async fn set_bug_lifecycle_status(
        &self,
        id: i64,
        status: BugLifecycleStatus,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE bugs SET lifecycle_status = ?, updated_at = ?, audit_author = ?, audit_tool = ?, audit_model = ? WHERE id = ?",
                libsql::params![status.as_str(), now, self.bug_actor.as_str(), self.bug_tool.as_str(), self.bug_model.clone(), id],
            )
            .await?;
        Ok(())
    }

    /// Updates the analysis execution state of a bug.
    pub async fn set_bug_pipeline_state(&self, id: i64, state: BugPipelineState) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE bugs SET pipeline_state = ?, updated_at = ?, audit_author = ?, audit_tool = ?, audit_model = ? WHERE id = ?",
                libsql::params![state.as_str(), now, self.bug_actor.as_str(), self.bug_tool.as_str(), self.bug_model.clone(), id],
            )
            .await?;
        Ok(())
    }

    /// Records that an analysis attempt failed.
    ///
    /// The triage state is left untouched: a crashed run says nothing about
    /// whether the underlying defect is real.
    pub async fn fail_bug_analysis(&self, id: i64, error: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let tx = self.begin_bug_write(id).await?;
        let changed = tx
            .execute(
                "UPDATE bugs
                 SET pipeline_state = ?, last_error = ?, locked_by = NULL,
                     lease_expires_at = NULL, updated_at = ?,
                     audit_author = ?, audit_tool = ?, audit_model = ?
                 WHERE id = ? AND (? = 0 OR pipeline_state = 'running')",
                libsql::params![
                    BugPipelineState::Failed.as_str(),
                    error,
                    now,
                    self.bug_actor.as_str(),
                    self.bug_tool.as_str(),
                    self.bug_model.clone(),
                    id,
                    self.bug_claim.is_some() as i64,
                ],
            )
            .await?;
        if self.bug_claim.is_some() && changed == 0 {
            bail!("A completed analysis cannot be marked failed");
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn update_bug_title(&self, id: i64, title: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE bugs SET title = ?, updated_at = ?, audit_author = ?, audit_tool = ?, audit_model = ? WHERE id = ?",
                libsql::params![title, now, self.bug_actor.as_str(), self.bug_tool.as_str(), self.bug_model.clone(), id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_bug_vector(&self, id: i64, vector_json: &str) -> Result<()> {
        self.store_bug_vector(id, vector_json).await
    }

    pub async fn update_bug_subsystems(
        &self,
        id: i64,
        subsystems: &[AttributedSubsystem],
    ) -> Result<()> {
        let tx = self.conn.transaction().await?;
        self.with_connection((*tx).clone())
            .replace_bug_subsystems(id, subsystems)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn replace_bug_subsystems(
        &self,
        id: i64,
        subsystems: &[AttributedSubsystem],
    ) -> Result<()> {
        self.conn.execute("UPDATE bugs SET audit_author = ?, audit_tool = ?, audit_model = ?, updated_at = ? WHERE id = ?", libsql::params![self.bug_actor.as_str(), self.bug_tool.as_str(), self.bug_model.clone(), chrono::Utc::now().timestamp(), id]).await?;
        let wanted: std::collections::BTreeMap<&str, SubsystemSource> = subsystems
            .iter()
            .map(|s| (s.name.trim(), s.source))
            .filter(|(name, _)| !name.is_empty())
            .collect();

        // The read is drained and the cursor dropped before any write, because
        // libsql refuses to commit a transaction that still has a statement in
        // progress, and deleting while iterating leaves the cursor open.
        let existing: Vec<String> = {
            let mut rows = self
                .conn
                .query(
                    "SELECT subsystem FROM bug_subsystems WHERE bug_id = ?",
                    libsql::params![id],
                )
                .await?;
            let mut found = Vec::new();
            while let Some(row) = rows.next().await? {
                found.push(row.get::<String>(0)?);
            }
            found
        };

        for sub in existing {
            if !wanted.contains_key(sub.as_str()) {
                self.conn
                    .execute(
                        "DELETE FROM bug_subsystems WHERE bug_id = ? AND subsystem = ?",
                        libsql::params![id, sub],
                    )
                    .await?;
            }
        }
        for (name, source) in wanted {
            self.conn
                .execute(
                    UPSERT_BUG_SUBSYSTEM_SQL,
                    libsql::params![id, name, source.as_str()],
                )
                .await?;
        }
        Ok(())
    }

    /// Commits the verdict, report, projections and successful state together.
    pub async fn update_bug_outcome(
        &self,
        id: i64,
        params: UpdateBugOutcomeParams<'_>,
    ) -> Result<()> {
        let tx = self.begin_bug_write(id).await?;
        self.with_connection((*tx).clone())
            .write_bug_outcome(id, params)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn write_bug_outcome(&self, id: i64, params: UpdateBugOutcomeParams<'_>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        if let Some(title) = params.problem {
            self.update_bug_title(id, title).await?;
        }
        if let Some(subsystems) = params.subsystems {
            self.replace_bug_subsystems(id, subsystems).await?;
        }
        if let Some(vector) = params.vector_json {
            self.update_bug_vector(id, vector).await?;
        }
        // A verdict initializes triage; it must not undo a human decision
        // made while this analysis was pending or running.
        self.conn
            .execute(
                "UPDATE bugs SET lifecycle_status = ?, updated_at = ?,
                    audit_author = ?, audit_tool = ?, audit_model = ?
             WHERE id = ? AND lifecycle_status = 'new'",
                libsql::params![
                    params.lifecycle_status.as_str(),
                    now,
                    self.bug_actor.as_str(),
                    self.bug_tool.as_str(),
                    self.bug_model.clone(),
                    id
                ],
            )
            .await?;
        if params.verified_on_sha.is_some() || params.locations.is_some() {
            let is_valid = params.lifecycle_status != BugLifecycleStatus::Dismissed;
            let refutation = if !is_valid {
                params.severity_explanation.map(|s| s.to_string())
            } else {
                None
            };
            let data = serde_json::json!({
                "verified_on_sha": params.verified_on_sha,
                "is_valid": is_valid,
                "refutation_evidence": refutation,
                "locations": params.locations,
                "source_files": params.source_files,
            });
            self.insert_bug_enrichment(
                id,
                &NewBugEnrichment {
                    kind: "verification".to_string(),
                    tool: self.bug_tool.clone(),
                    created_at: now,
                    content: params.severity_explanation.map(|s| s.to_string()),
                    data_json: Some(data),
                    ..Default::default()
                },
            )
            .await?;
        }

        if let Some(intro) = params.introduced_in_commit {
            self.insert_bug_enrichment(
                id,
                &NewBugEnrichment {
                    kind: "origin_discovery".to_string(),
                    tool: self.bug_tool.clone(),
                    created_at: now,
                    content: Some(intro.to_string()),
                    data_json: Some(serde_json::json!({
                        "introducing_commit_sha": intro,
                    })),
                    ..Default::default()
                },
            )
            .await?;
        }

        if params.severity != Severity::Unknown {
            self.insert_bug_enrichment(
                id,
                &NewBugEnrichment {
                    kind: "severity_calibration".to_string(),
                    tool: self.bug_tool.clone(),
                    created_at: now,
                    content: params.severity_explanation.map(|s| s.to_string()),
                    data_json: Some(serde_json::json!({
                        "severity": params.severity.as_str(),
                        "severity_int": params.severity as i32,
                        "subsystems": params
                            .subsystems
                            .map(|subs| subs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()),
                    })),
                    ..Default::default()
                },
            )
            .await?;
        }

        if !params.inline_review.is_empty() || params.logs.is_some() {
            self.insert_bug_enrichment(
                id,
                &NewBugEnrichment {
                    kind: if params.inline_review.is_empty() {
                        "analysis"
                    } else {
                        "report"
                    }
                    .to_string(),
                    tool: self.bug_tool.clone(),
                    created_at: now,
                    content: Some(params.inline_review.to_string()),
                    data_json: Some(serde_json::json!({
                        "format": "lkml_markdown",
                    })),
                    tokens_in: params.tokens_in,
                    tokens_out: params.tokens_out,
                    tokens_cached: params.tokens_cached,
                    logs: params.logs.map(|s| s.to_string()),
                    ..Default::default()
                },
            )
            .await?;
        }

        // Success and every outcome record become visible together at commit.
        self.set_bug_pipeline_state(id, BugPipelineState::Succeeded)
            .await?;
        Ok(())
    }

    pub async fn list_bugs(&self, params: ListBugsParams<'_>) -> Result<(Vec<Bug>, usize)> {
        let limit_val = params.limit.unwrap_or(50) as i64;
        let page_val = params.page.unwrap_or(1) as i64;
        let offset_val = limit_val * (page_val.saturating_sub(1));

        let mut conditions: Vec<std::borrow::Cow<'static, str>> = Vec::new();
        let mut query_params = Vec::new();

        // The scope predicate goes in before the caller's own filters so that a
        // subsystem filter can only narrow what the principal may already see.
        // Filtering in Rust after the query would corrupt the pagination count.
        if let BugVisibility::Sections(scope) = params.visibility {
            let titles: Vec<&str> = scope
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            if titles.is_empty() {
                return Ok((Vec::new(), 0));
            }
            let placeholders = vec!["?"; titles.len()].join(", ");
            conditions.push(
                format!(
                    "id IN (SELECT bug_id FROM bug_subsystems
                            WHERE source = '{}' AND subsystem COLLATE NOCASE IN ({}))",
                    SubsystemSource::MaintainersSection.as_str(),
                    placeholders
                )
                .into(),
            );
            for title in titles {
                query_params.push(libsql::Value::Text(title.to_string()));
            }
        }

        if let Some(subs) = params.subsystems {
            let valid_subs: Vec<&str> = subs
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            if valid_subs.is_empty() {
                return Ok((Vec::new(), 0));
            }
            let placeholders = vec!["?"; valid_subs.len()].join(", ");
            conditions.push(
                format!(
                    "id IN (SELECT bug_id FROM bug_subsystems WHERE subsystem IN ({}))",
                    placeholders
                )
                .into(),
            );
            for s in valid_subs {
                query_params.push(libsql::Value::Text(s.to_string()));
            }
        } else if let Some(sub) = params.subsystem.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if sub.contains(',') {
                let parts: Vec<&str> = sub
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                if parts.is_empty() {
                    return Ok((Vec::new(), 0));
                }
                let placeholders = vec!["?"; parts.len()].join(", ");
                conditions.push(
                    format!(
                        "id IN (SELECT bug_id FROM bug_subsystems WHERE subsystem IN ({}))",
                        placeholders
                    )
                    .into(),
                );
                for p in parts {
                    query_params.push(libsql::Value::Text(p.to_string()));
                }
            } else {
                conditions.push(
                    "id IN (SELECT bug_id FROM bug_subsystems WHERE subsystem = ? OR subsystem LIKE ?)".into(),
                );
                query_params.push(libsql::Value::Text(sub.to_string()));
                query_params.push(libsql::Value::Text(format!("{}/%", sub)));
            }
        }

        if let Some(min_sev) = params.min_severity
            && min_sev != Severity::Unknown
        {
            // severity_int is projected from the severity_calibration enrichment
            // by trigger, so this is an indexed comparison rather than a
            // correlated subquery over the enrichment log.
            conditions.push("severity_int >= ?".into());
            query_params.push(libsql::Value::Integer(min_sev as i64));
        }

        if let Some(status) = params.lifecycle_status {
            conditions.push("lifecycle_status = ?".into());
            query_params.push(libsql::Value::Text(status.as_str().to_string()));
        }

        if let Some(state) = params.pipeline_state {
            conditions.push("pipeline_state = ?".into());
            query_params.push(libsql::Value::Text(state.as_str().to_string()));
        }

        match params.assignee {
            Some(AssigneeFilter::Unassigned) => conditions.push("assignee IS NULL".into()),
            Some(AssigneeFilter::Is(who)) => {
                conditions.push("assignee = ?".into());
                query_params.push(libsql::Value::Text(who.trim().to_string()));
            }
            None => {}
        }

        if let Some(q) = params.search.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            conditions.push("(title LIKE ? OR bugid LIKE ?)".into());
            let pattern = format!("%{}%", q);
            query_params.push(libsql::Value::Text(pattern.clone()));
            query_params.push(libsql::Value::Text(pattern));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let sort_dir = match params.sort_order.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("asc") => "ASC",
            _ => "DESC",
        };
        let order_clause = match params.sort_by.map(|s| s.to_ascii_lowercase()).as_deref() {
            // Sorts straight off the projected column, which idx_bugs_severity covers.
            Some("severity") => format!("ORDER BY severity_int {}, id {}", sort_dir, sort_dir),
            Some("title") | Some("problem") => {
                format!("ORDER BY title {}, created_at DESC, id DESC", sort_dir)
            }
            Some("status") | Some("lifecycle_status") => format!(
                "ORDER BY lifecycle_status {}, created_at DESC, id DESC",
                sort_dir
            ),
            Some("pipeline_state") => format!(
                "ORDER BY pipeline_state {}, created_at DESC, id DESC",
                sort_dir
            ),
            Some("assignee") => format!(
                "ORDER BY assignee IS NULL, assignee {}, created_at DESC, id DESC",
                sort_dir
            ),
            Some("id") => format!("ORDER BY id {}", sort_dir),
            Some("bugid") => format!("ORDER BY bugid {}, id {}", sort_dir, sort_dir),
            Some("created_at") => format!("ORDER BY created_at {}, id {}", sort_dir, sort_dir),
            Some("discoveries") | Some("findings") => {
                format!(
                    "ORDER BY COALESCE((
                        WITH RECURSIVE ancestors(root, id, parent) AS (
                            SELECT bugs.id, bugs.id, bugs.duplicate_of_id
                            UNION SELECT a.root, b.id, b.duplicate_of_id FROM bugs b JOIN ancestors a ON b.id = a.parent
                        ), family(root, id) AS (
                            SELECT root, id FROM ancestors
                            UNION SELECT f.root, b.id FROM bugs b JOIN family f ON b.duplicate_of_id = f.id
                        )
                        SELECT COUNT(*)
                        FROM family f
                        JOIN bugs b ON b.id = f.id
                        LEFT JOIN bug_enrichments e ON e.bug_id = f.id AND e.kind IN ('candidate', 'discovery')
                    ), 1) {}, id {}",
                    sort_dir, sort_dir
                )
            }
            _ => format!("ORDER BY created_at {}, id {}", sort_dir, sort_dir),
        };

        let count_sql = format!("SELECT COUNT(*) FROM bugs {}", where_clause);
        let mut count_rows = self.conn.query(&count_sql, query_params.clone()).await?;
        let total: usize = if let Some(row) = count_rows.next().await? {
            row.get::<i64>(0).unwrap_or(0) as usize
        } else {
            0
        };

        let select_sql = format!(
            "SELECT {BUG_ROW_COLUMNS}
             FROM bugs
             {}
             {}
             LIMIT ? OFFSET ?",
            where_clause, order_clause
        );

        query_params.push(libsql::Value::Integer(limit_val));
        query_params.push(libsql::Value::Integer(offset_val));

        let mut rows = self.conn.query(&select_sql, query_params).await?;
        let mut bugs = Vec::new();
        while let Some(row) = rows.next().await? {
            bugs.push(Self::parse_bug_row_core(&row)?);
        }

        if !bugs.is_empty() {
            let bug_ids: Vec<i64> = bugs.iter().map(|b| b.id).collect();
            let placeholders = vec!["?"; bug_ids.len()].join(", ");

            // Batch fetch subsystems
            let subs_sql = format!(
                "SELECT bug_id, subsystem FROM bug_subsystems WHERE bug_id IN ({}) ORDER BY subsystem ASC",
                placeholders
            );
            let subs_params: Vec<libsql::Value> = bug_ids
                .iter()
                .map(|&id| libsql::Value::Integer(id))
                .collect();
            let mut subs_rows = self.conn.query(&subs_sql, subs_params).await?;
            let mut subs_map: std::collections::HashMap<i64, Vec<String>> =
                std::collections::HashMap::new();
            while let Some(row) = subs_rows.next().await? {
                let bug_id: i64 = row.get(0)?;
                let sub: String = row.get(1)?;
                subs_map.entry(bug_id).or_default().push(sub);
            }

            // Batch fetch enrichments (NULL as logs)
            let enrichments_sql = format!(
                "SELECT id, bug_id, kind, tool, model, author, created_at, content, data_json,
                        tokens_in, tokens_out, tokens_cached, NULL as logs
                 FROM bug_enrichments
                 WHERE bug_id IN ({})
                 ORDER BY created_at ASC, id ASC",
                placeholders
            );
            let enr_params: Vec<libsql::Value> = bug_ids
                .iter()
                .map(|&id| libsql::Value::Integer(id))
                .collect();
            let mut enr_rows = self.conn.query(&enrichments_sql, enr_params).await?;
            let mut enrichments_map: std::collections::HashMap<i64, Vec<BugEnrichment>> =
                std::collections::HashMap::new();
            while let Some(row) = enr_rows.next().await? {
                let enrichment = Self::parse_bug_enrichment_row(&row)?;
                enrichments_map
                    .entry(enrichment.bug_id)
                    .or_default()
                    .push(enrichment);
            }

            for bug in &mut bugs {
                if let Some(subs) = subs_map.remove(&bug.id) {
                    bug.subsystems = subs;
                }
                if let Some(enrs) = enrichments_map.remove(&bug.id) {
                    bug.enrichments = enrs;
                }
            }
        }

        Ok((bugs, total))
    }

    /// Counts open bugs per subsystem, over only the bugs the principal may
    /// read. Counting every bug would turn this endpoint into an oracle for the
    /// existence of bugs in subsystems the caller has no authority over.
    pub async fn get_subsystems_bug_counts(
        &self,
        lifecycle_status: Option<BugLifecycleStatus>,
        visibility: BugVisibility<'_>,
    ) -> Result<Vec<(String, usize)>> {
        let st = lifecycle_status.unwrap_or(BugLifecycleStatus::Open);
        let mut params: Vec<libsql::Value> = vec![libsql::Value::Text(st.as_str().to_string())];
        let scope_clause = match visibility {
            BugVisibility::Unrestricted => String::new(),
            BugVisibility::Sections(scope) => {
                let titles: Vec<&str> = scope
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                if titles.is_empty() {
                    return Ok(Vec::new());
                }
                let placeholders = vec!["?"; titles.len()].join(", ");
                for title in &titles {
                    params.push(libsql::Value::Text((*title).to_string()));
                }
                format!(
                    " AND b.id IN (SELECT bug_id FROM bug_subsystems
                                   WHERE source = '{}' AND subsystem COLLATE NOCASE IN ({}))",
                    SubsystemSource::MaintainersSection.as_str(),
                    placeholders
                )
            }
        };
        let sql = format!(
            "SELECT bs.subsystem, COUNT(DISTINCT b.id) AS bug_count
                   FROM bug_subsystems bs
                   JOIN bugs b ON bs.bug_id = b.id
                   WHERE b.lifecycle_status = ?{}
                   GROUP BY bs.subsystem
                   HAVING bug_count > 0
                   ORDER BY bug_count DESC, bs.subsystem ASC",
            scope_clause
        );
        let mut rows = self.conn.query(&sql, params).await?;
        let mut results = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let name: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            if count > 0 {
                results.push((name, count as usize));
            }
        }
        Ok(results)
    }

    /// Records that a review surfaced a bug.
    ///
    /// A review that already discovered this bug keeps that credit. Two
    /// candidates raised by one review can be folded together, and the fold
    /// links the surviving bug back to the very same review as a rediscovery.
    /// Replacing the row outright would let that second link overwrite the
    /// first and leave a genuinely new bug looking like nobody found it, so
    /// the flag only ever moves from false to true.
    pub async fn link_review_to_bug(
        &self,
        review_id: i64,
        bug_id: i64,
        is_newly_discovered: bool,
    ) -> Result<()> {
        let Some(claim) = &self.bug_claim else {
            return self
                .insert_bug_review(review_id, bug_id, is_newly_discovered)
                .await;
        };
        let tx = self.begin_bug_write(claim.bug_id).await?;
        self.with_connection((*tx).clone())
            .insert_bug_review(review_id, bug_id, is_newly_discovered)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn insert_bug_review(
        &self,
        review_id: i64,
        bug_id: i64,
        is_newly_discovered: bool,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO bug_reviews (review_id, bug_id, is_newly_discovered)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(review_id, bug_id) DO UPDATE
                    SET is_newly_discovered =
                        MAX(is_newly_discovered, excluded.is_newly_discovered)",
                libsql::params![review_id, bug_id, if is_newly_discovered { 1 } else { 0 }],
            )
            .await?;
        Ok(())
    }

    /// [Reliability Framework] Safely transitions a bug into a Duplicate state while
    /// atomically migrating all associated review linkages to the pre-existing canonical bug.
    /// This single Transaction boundary guarantees tearing cannot occur during deduplication.
    /// Returns false when automatic deduplication preserves existing triage.
    pub async fn mark_bug_as_duplicate(&self, params: MarkDuplicateBugParams<'_>) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let tx = self.begin_bug_write(params.ephemeral_id).await?;

        // Both probes are scoped so their cursors close before the writes
        // below. libsql refuses to commit a transaction that still has a
        // statement in progress, and these handles would otherwise stay alive
        // until the end of the function.
        let canonical_exists = {
            let mut target = tx
                .query(
                    "SELECT id FROM bugs WHERE id = ? AND duplicate_of_id IS NULL",
                    libsql::params![params.canonical_id],
                )
                .await?;
            target.next().await?.is_some()
        };
        if params.ephemeral_id == params.canonical_id || !canonical_exists {
            bail!("Choose an existing canonical bug, distinct from this bug");
        }
        let source_status = {
            let mut source = tx
                .query(
                    "SELECT lifecycle_status FROM bugs WHERE id = ?",
                    libsql::params![params.ephemeral_id],
                )
                .await?;
            source
                .next()
                .await?
                .map(|row| row.get::<String>(0))
                .transpose()?
        }
        .ok_or_else(|| anyhow::anyhow!("Bug not found"))?;
        if params.preserve_triage && source_status != BugLifecycleStatus::New.as_str() {
            tx.rollback().await?;
            return Ok(false);
        }

        // Folding a bug into a canonical one ends its pipeline, so the
        // analysis state is retired in the same statement as the triage state.
        // Leaving it behind strands the row: the dedup stage returns before the
        // workflow records an outcome, and the caller then drops the lease, so
        // the bug would keep a 'running' state that no worker can reclaim
        // because every recovery query matches on an expired lease.
        //
        // 'succeeded' rather than 'abandoned' because reaching a duplicate is a
        // completed triage result, not a dead letter, and no analysis work is
        // still owed once the finding lives on the canonical bug.
        tx.execute(
            "UPDATE bugs SET lifecycle_status = 'duplicate', duplicate_of_id = ?,
                    pipeline_state = 'succeeded',
                    locked_by = CASE WHEN ? THEN locked_by ELSE NULL END,
                    lease_expires_at = CASE WHEN ? THEN lease_expires_at ELSE NULL END,
                    updated_at = ?, audit_author = ?, audit_tool = ?, audit_model = ?
              WHERE id = ?",
            libsql::params![
                params.canonical_id,
                self.bug_claim.is_some() as i64,
                self.bug_claim.is_some() as i64,
                now,
                self.bug_actor.as_str(),
                self.bug_tool.as_str(),
                self.bug_model.clone(),
                params.ephemeral_id
            ],
        )
        .await?;

        let compressed_logs = params
            .logs
            .map(crate::compression::compress_string_if_needed)
            .unwrap_or(libsql::Value::Null);

        tx.execute(
            "INSERT INTO bug_enrichments (
                bug_id, kind, tool, author, model, created_at, content, tokens_in, tokens_out, tokens_cached, logs
             ) VALUES (?, 'deduplication', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                params.ephemeral_id,
                self.bug_tool.as_str(), self.bug_actor.as_str(), self.bug_model.clone(),
                now,
                params.reasoning,
                params.tokens_in.map(|t| t as i64),
                params.tokens_out.map(|t| t as i64),
                params.tokens_cached.map(|t| t as i64),
                compressed_logs,
            ],
        )
        .await?;

        // is_newly_discovered is written explicitly. A migrated link records a
        // review that rediscovered an existing bug, so omitting the column and
        // taking the schema default of 1 would report every fold as a fresh
        // discovery on the canonical bug.
        tx.execute(
            "INSERT OR IGNORE INTO bug_reviews (review_id, bug_id, is_newly_discovered)
             SELECT review_id, ?1, 0 FROM bug_reviews WHERE bug_id = ?2",
            libsql::params![params.canonical_id, params.ephemeral_id],
        )
        .await?;
        tx.execute(
            "DELETE FROM bug_reviews WHERE bug_id = ?",
            libsql::params![params.ephemeral_id],
        )
        .await?;

        tx.commit().await?;
        Ok(true)
    }

    pub async fn migrate_review_bugs(&self, from_bug_id: i64, to_bug_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO bug_reviews (review_id, bug_id, is_newly_discovered)
             SELECT review_id, ?, 0 FROM bug_reviews WHERE bug_id = ?",
                libsql::params![to_bug_id, from_bug_id],
            )
            .await?;
        self.conn
            .execute(
                "DELETE FROM bug_reviews WHERE bug_id = ?",
                libsql::params![from_bug_id],
            )
            .await?;
        Ok(())
    }

    pub async fn list_duplicates_for_bug(&self, canonical_id: i64) -> Result<Vec<Bug>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM bugs WHERE duplicate_of_id = ? ORDER BY id ASC",
                libsql::params![canonical_id],
            )
            .await?;
        let mut list = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            if let Some(bug) = self.get_bug(id).await? {
                list.push(bug);
            }
        }
        Ok(list)
    }

    pub async fn list_bugs_for_review(&self, review_id: i64) -> Result<Vec<(Bug, bool)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT pb.id, rpb.is_newly_discovered
                 FROM bugs pb
                 JOIN bug_reviews rpb ON pb.id = rpb.bug_id
                 WHERE rpb.review_id = ?
                 ORDER BY pb.id ASC",
                libsql::params![review_id],
            )
            .await?;

        let mut list = Vec::new();
        while let Some(row) = rows.next().await? {
            let bug_id: i64 = row.get(0)?;
            let is_newly_discovered: i64 = row.get(1).unwrap_or(1);
            if let Some(bug) = self.get_bug(bug_id).await? {
                list.push((bug, is_newly_discovered != 0));
            }
        }

        list.sort_by(|(a, _), (b, _)| {
            b.severity()
                .cmp(&a.severity())
                .then_with(|| a.id.cmp(&b.id))
        });

        Ok(list)
    }

    pub async fn list_bugs_for_patchset(&self, patchset_id: i64) -> Result<Vec<(Bug, bool)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT DISTINCT pb.id, rpb.is_newly_discovered
                 FROM bugs pb
                 JOIN bug_reviews rpb ON pb.id = rpb.bug_id
                 JOIN reviews r ON rpb.review_id = r.id
                 WHERE r.patchset_id = ?
                 ORDER BY pb.id ASC",
                libsql::params![patchset_id],
            )
            .await?;

        let mut list = Vec::new();
        while let Some(row) = rows.next().await? {
            let bug_id: i64 = row.get(0)?;
            let is_newly_discovered: i64 = row.get(1).unwrap_or(1);
            if let Some(bug) = self.get_bug(bug_id).await? {
                list.push((bug, is_newly_discovered != 0));
            }
        }

        list.sort_by(|(a, _), (b, _)| {
            b.severity()
                .cmp(&a.severity())
                .then_with(|| a.id.cmp(&b.id))
        });

        Ok(list)
    }

    /// Loads the deduplication corpus: every bug that is still a candidate for
    /// being matched against, together with its embedding.
    ///
    /// This is the only read path that pulls vectors, which is why they live in
    /// a side table rather than on the core row.
    pub async fn list_all_bugs_for_vector_search(&self) -> Result<Vec<Bug>> {
        let mut rows = self
            .conn
            .query(
                "SELECT b.id, v.vector_json
                 FROM bugs b
                 LEFT JOIN bug_vectors v ON v.bug_id = b.id
                 WHERE b.lifecycle_status IN ('new', 'open', 'fixed')
                 ORDER BY b.id ASC",
                (),
            )
            .await?;

        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            let vector_json: Option<String> = row.get(1).ok().flatten();
            ids.push((id, vector_json));
        }

        let mut list = Vec::new();
        for (id, vector_json) in ids {
            if let Some(mut bug) = self.get_bug(id).await? {
                bug.vector_json = vector_json;
                list.push(bug);
            }
        }

        Ok(list)
    }

    /// Parses a row selected with [`BUG_ROW_COLUMNS`]. The column order here and
    /// the order in that constant must be kept in step.
    fn parse_bug_row_core(row: &libsql::Row) -> Result<Bug> {
        let id: i64 = row.get(0)?;
        let bugid: String = row.get(1)?;
        let title: String = row.get(2)?;
        let lifecycle_status: String = row.get(3)?;
        let pipeline_state: String = row.get(4)?;
        let reporter: String = row.get(5)?;
        let reported_at: i64 = row.get(6)?;
        let assignee: Option<String> = row.get(7).ok().flatten();
        let assigned_at: Option<i64> = row.get(8).ok().flatten();
        let discovered_in_patchset_id: Option<i64> = row.get(9).ok().flatten();
        let discovered_in_patch_id: Option<i64> = row.get(10).ok().flatten();
        let discovered_in_commit: Option<String> = row.get(11).ok().flatten();
        let source_ref: Option<String> = row.get(12).ok().flatten();
        let duplicate_of_id: Option<i64> = row.get(13).ok().flatten();
        let created_at: i64 = row.get(14)?;
        let updated_at: i64 = row.get(15)?;

        Ok(Bug {
            id,
            bugid,
            title,
            lifecycle_status: lifecycle_status.parse()?,
            pipeline_state: pipeline_state.parse()?,
            reporter,
            reported_at,
            assignee,
            assigned_at,
            discovered_in_patchset_id,
            discovered_in_patch_id,
            discovered_in_commit,
            source_ref,
            vector_json: None,
            duplicate_of_id,
            created_at,
            updated_at,
            subsystems: Vec::new(),
            enrichments: Vec::new(),
        })
    }

    pub async fn parse_bug_row(&self, row: &libsql::Row) -> Result<Bug> {
        let mut bug = Self::parse_bug_row_core(row)?;
        bug.subsystems = self
            .get_subsystems_for_bug(bug.id)
            .await
            .unwrap_or_default();
        bug.enrichments = self.get_bug_enrichments(bug.id).await?;
        Ok(bug)
    }

    pub async fn get_timeline_stats(&self, subsystem_id: Option<i64>) -> Result<serde_json::Value> {
        let mut messages_data = Vec::new();

        if let Some(sid) = subsystem_id {
            let sql_msgs =
                "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, count(*) FROM messages m
             JOIN messages_subsystems ms ON m.id = ms.message_id
             WHERE ms.subsystem_id = ?
             GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql_msgs, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    messages_data.push(json!({"day": day, "count": count}));
                }
            }
        } else {
            let sql_msgs = "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, count(*) FROM messages GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql_msgs, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    messages_data.push(json!({"day": day, "count": count}));
                }
            }
        }

        let mut patchsets_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql = "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, status, count(*) FROM patchsets p
             JOIN patchsets_subsystems ps ON p.id = ps.patchset_id
             WHERE ps.subsystem_id = ?
             GROUP BY day, status ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: Option<String> = row.get(1).ok();
                    let count: i64 = row.get(2)?;
                    patchsets_data.push(
                        json!({"day": day, "status": status.unwrap_or_default(), "count": count}),
                    );
                }
            }
        } else {
            let sql = "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, status, count(*) FROM patchsets GROUP BY day, status ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: Option<String> = row.get(1).ok();
                    let count: i64 = row.get(2)?;
                    patchsets_data.push(
                        json!({"day": day, "status": status.unwrap_or_default(), "count": count}),
                    );
                }
            }
        }

        // Patches stats (individual patches)
        let mut patches_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql =
                "SELECT strftime('%Y-%m-%d', m.date, 'unixepoch') as day, count(*) FROM patches p
              JOIN messages m ON p.message_id = m.message_id
              JOIN patches_subsystems ps ON p.id = ps.patch_id
              WHERE ps.subsystem_id = ?
              GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    patches_data.push(json!({"day": day, "count": count}));
                }
            }
        } else {
            let sql =
                "SELECT strftime('%Y-%m-%d', m.date, 'unixepoch') as day, count(*) FROM patches p
              JOIN messages m ON p.message_id = m.message_id
              GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    patches_data.push(json!({"day": day, "count": count}));
                }
            }
        }

        // Reviews stats (outcomes over time)
        let mut reviews_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                r.status,
                COUNT(*) as count
            FROM reviews r
            JOIN patchsets_subsystems ps ON r.patchset_id = ps.patchset_id
            WHERE ps.subsystem_id = ?
            GROUP BY day, status
            ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    reviews_data.push(json!({"day": day, "status": status, "count": count}));
                }
            }
        } else {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                r.status,
                COUNT(*) as count
            FROM reviews r
            GROUP BY day, status
            ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    reviews_data.push(json!({"day": day, "status": status, "count": count}));
                }
            }
        }

        // Findings stats
        let mut findings_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                CASE f.severity 
                    WHEN 1 THEN 'low' 
                    WHEN 2 THEN 'medium' 
                    WHEN 3 THEN 'high' 
                    WHEN 4 THEN 'critical' 
                    ELSE 'unknown' 
                END as severity,
                COUNT(*) as count
            FROM findings f
            JOIN reviews r ON f.review_id = r.id
            JOIN patchsets_subsystems ps ON r.patchset_id = ps.patchset_id
            WHERE ps.subsystem_id = ?
            GROUP BY day, severity
            ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let severity: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    findings_data.push(json!({"day": day, "severity": severity, "count": count}));
                }
            }
        } else {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                CASE f.severity 
                    WHEN 1 THEN 'low' 
                    WHEN 2 THEN 'medium' 
                    WHEN 3 THEN 'high' 
                    WHEN 4 THEN 'critical' 
                    ELSE 'unknown' 
                END as severity,
                COUNT(*) as count
            FROM findings f
            JOIN reviews r ON f.review_id = r.id
            GROUP BY day, severity
            ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let severity: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    findings_data.push(json!({"day": day, "severity": severity, "count": count}));
                }
            }
        }

        Ok(json!({
            "messages": messages_data,
            "patchsets": patchsets_data,
            "patches": patches_data,
            "reviews": reviews_data,
            "findings": findings_data
        }))
    }

    pub async fn get_review_stats(&self) -> Result<serde_json::Value> {
        let mut total_rows = self
            .conn
            .query(
                "SELECT count(*) FROM reviews WHERE status NOT IN ('Pending', 'In Review')",
                (),
            )
            .await?;
        let total_reviews: i64 = if let Ok(Some(row)) = total_rows.next().await {
            row.get(0).unwrap_or(0)
        } else {
            0
        };

        let mut failed_rows = self
            .conn
            .query(
                "SELECT count(*) FROM reviews WHERE status NOT IN ('Pending', 'In Review') AND (lower(status) LIKE '%failed%' OR lower(status) LIKE '%error%')",
                (),
            )
            .await?;
        let total_failures: i64 = if let Ok(Some(row)) = failed_rows.next().await {
            row.get(0).unwrap_or(0)
        } else {
            0
        };

        let sql = "WITH last_reviews AS (
            SELECT * FROM reviews ORDER BY id DESC LIMIT 1000
        )
        SELECT
            r.provider,
            r.model,
            r.status,
            count(*),
            sum(COALESCE(ai.tokens_in, 0)),
            sum(COALESCE(ai.tokens_out, 0)),
            sum(COALESCE(ai.tokens_cached, 0))
        FROM last_reviews r
        LEFT JOIN ai_interactions ai INDEXED BY idx_ai_interactions_tokens ON r.interaction_id = ai.id
        GROUP BY r.provider, r.model, r.status";

        let mut rows = self.conn.query(sql, ()).await?;
        let mut stats = Vec::new();
        #[allow(clippy::similar_names)]
        while let Ok(Some(row)) = rows.next().await {
            let provider: Option<String> = row.get(0).ok();
            let model: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let count: i64 = row.get(3)?;
            let tokens_in: i64 = row.get(4).unwrap_or(0);
            let tokens_out: i64 = row.get(5).unwrap_or(0);
            let tokens_cached: i64 = row.get(6).unwrap_or(0);

            stats.push(json!({
                "provider": provider.unwrap_or_default(),
                "model": model.unwrap_or_default(),
                "status": status.unwrap_or_default(),
                "count": count,
                "tokens_in": tokens_in,
                "tokens_out": tokens_out,
                "tokens_cached": tokens_cached
            }));
        }

        Ok(json!({
            "total_reviews": total_reviews,
            "total_failures": total_failures,
            "reviews": stats
        }))
    }

    pub async fn get_tool_usage_stats(&self) -> Result<serde_json::Value> {
        let sql = "WITH last_reviews AS ( \
                       SELECT id FROM reviews ORDER BY id DESC LIMIT 1000 \
                   ) \
                   SELECT tu.provider, tu.model, tu.tool_name, count(*), avg(tu.output_length) \
                   FROM tool_usages tu \
                   JOIN last_reviews r ON tu.review_id = r.id \
                   GROUP BY tu.provider, tu.model, tu.tool_name";
        let mut rows = self.conn.query(sql, ()).await?;
        let mut stats = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let provider: Option<String> = row.get(0).ok();
            let model: Option<String> = row.get(1).ok();
            let tool_name: Option<String> = row.get(2).ok();
            let count: i64 = row.get(3)?;
            let avg_len: f64 = row.get(4).unwrap_or(0.0);
            stats.push(json!({
                "provider": provider.unwrap_or_default(),
                "model": model.unwrap_or_default(),
                "tool": tool_name.unwrap_or_default(),
                "count": count,
                "avg_output_length": avg_len
            }));
        }
        Ok(json!(stats))
    }

    pub async fn begin_transaction(&self) -> Result<()> {
        self.conn.execute("BEGIN IMMEDIATE", ()).await?;
        Ok(())
    }

    pub async fn commit_transaction(&self) -> Result<()> {
        self.conn.execute("COMMIT", ()).await?;
        Ok(())
    }

    // People & Recipients
    pub async fn ensure_person(&self, name: Option<&str>, email: &str) -> Result<i64> {
        let email = email.trim();
        // Try to insert
        self.conn
            .execute(
                "INSERT OR IGNORE INTO people (name, email) VALUES (?, ?)",
                libsql::params![name, email],
            )
            .await?;

        // If a name is provided and the existing record has none, update it.
        // For now, keep it simple. Just get ID.
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM people WHERE email = ?",
                libsql::params![email],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to ensure person: {}", email))
        }
    }

    pub async fn add_message_recipient(
        &self,
        message_id: i64,
        person_id: i64,
        recipient_type: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_recipients (message_id, person_id, recipient_type) VALUES (?, ?, ?)",
                libsql::params![message_id, person_id, recipient_type],
            )
            .await?;
        Ok(())
    }

    // Subsystems
    pub async fn ensure_subsystem(&self, name: &str, mailing_list_address: &str) -> Result<i64> {
        // Try to insert
        self.conn
            .execute(
                "INSERT OR IGNORE INTO subsystems (name, mailing_list_address) VALUES (?, ?)",
                libsql::params![name, mailing_list_address],
            )
            .await?;

        // Get ID
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM subsystems WHERE mailing_list_address = ?",
                libsql::params![mailing_list_address],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            // Fallback: Get ID by name (Collision on name with different address)
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM subsystems WHERE name = ?",
                    libsql::params![name],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                Ok(row.get(0)?)
            } else {
                Err(anyhow::anyhow!("Failed to ensure subsystem"))
            }
        }
    }

    pub async fn add_subsystem_to_message(
        &self,
        message_id_db: i64,
        subsystem_id: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_subsystems (message_id, subsystem_id) VALUES (?, ?)",
                libsql::params![message_id_db, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn add_subsystem_to_thread(&self, thread_id: i64, subsystem_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO threads_subsystems (thread_id, subsystem_id) VALUES (?, ?)",
                libsql::params![thread_id, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn add_subsystem_to_patch(&self, patch_id: i64, subsystem_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO patches_subsystems (patch_id, subsystem_id) VALUES (?, ?)",
                libsql::params![patch_id, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn add_subsystem_to_patchset(
        &self,
        patchset_id: i64,
        subsystem_id: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO patchsets_subsystems (patchset_id, subsystem_id) VALUES (?, ?)",
                libsql::params![patchset_id, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_message_id_by_msg_id(&self, msg_id: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM messages WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn ensure_mailing_list(&self, name: &str, group: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO mailing_lists (name, nntp_group, last_article_num) VALUES (?, ?, 0)
                 ON CONFLICT(nntp_group) DO UPDATE SET name = excluded.name",
                libsql::params![name, group],
            )
            .await?;
        Ok(())
    }

    pub async fn get_last_article_num(&self, group: &str) -> Result<u64> {
        let mut rows = self
            .conn
            .query(
                "SELECT last_article_num FROM mailing_lists WHERE nntp_group = ?",
                libsql::params![group],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let num: i64 = row.get(0)?;
            Ok(num as u64)
        } else {
            Ok(0)
        }
    }

    pub async fn update_last_article_num(&self, group: &str, num: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE mailing_lists SET last_article_num = ? WHERE nntp_group = ?",
                libsql::params![num as i64, group],
            )
            .await?;
        Ok(())
    }

    pub async fn create_thread(
        &self,
        root_message_id: &str,
        subject: &str,
        date: i64,
    ) -> Result<i64> {
        let mut rows = self.conn
            .query(
                "INSERT INTO threads (root_message_id, subject, last_updated) VALUES (?, ?, ?) RETURNING id",
                libsql::params![root_message_id, subject, date],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get thread ID"))
        }
    }

    pub async fn get_thread_id_for_message(&self, message_id: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT thread_id FROM messages WHERE message_id = ?",
                libsql::params![message_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn ensure_thread_for_message(&self, message_id: &str, date: i64) -> Result<i64> {
        // 1. Check if message exists
        if let Some(tid) = self.get_thread_id_for_message(message_id).await? {
            return Ok(tid);
        }

        // 2. Not found, create new thread and placeholder message
        let thread_id = self
            .create_thread(message_id, "(placeholder)", date)
            .await?;

        self.create_message(
            message_id,
            thread_id,
            None,
            "unknown",
            "(placeholder)",
            date,
            "",
            "",
            "",
            None,
            None,
        )
        .await?;

        Ok(thread_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_message(
        &self,
        message_id: &str,
        thread_id: i64,
        in_reply_to: Option<&str>,
        author: &str,
        subject: &str,
        date: i64,
        body: &str,
        to: &str,
        cc: &str,
        git_blob_hash: Option<&str>,
        mailing_list: Option<&str>,
    ) -> Result<()> {
        self.create_message_with_references(
            message_id,
            thread_id,
            in_reply_to,
            author,
            subject,
            date,
            body,
            to,
            cc,
            git_blob_hash,
            mailing_list,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_message_with_references(
        &self,
        message_id: &str,
        thread_id: i64,
        in_reply_to: Option<&str>,
        author: &str,
        subject: &str,
        date: i64,
        body: &str,
        to: &str,
        cc: &str,
        git_blob_hash: Option<&str>,
        mailing_list: Option<&str>,
        references_hdr: Option<&str>,
    ) -> Result<()> {
        // Check for thread merge (Thread split resolution)
        if let Ok(Some(old_thread_id)) = self.get_thread_id_for_message(message_id).await
            && old_thread_id != thread_id
        {
            info!("Merging thread {} into {}", old_thread_id, thread_id);
            // 1. Move messages
            self.conn
                .execute(
                    "UPDATE messages SET thread_id = ? WHERE thread_id = ?",
                    libsql::params![thread_id, old_thread_id],
                )
                .await?;

            // 2. Move patchsets
            self.conn
                .execute(
                    "UPDATE patchsets SET thread_id = ? WHERE thread_id = ?",
                    libsql::params![thread_id, old_thread_id],
                )
                .await?;

            // 3. Merge subsystems
            self.conn
                .execute(
                    "UPDATE OR IGNORE threads_subsystems SET thread_id = ? WHERE thread_id = ?",
                    libsql::params![thread_id, old_thread_id],
                )
                .await?;
            // Delete any remaining (conflicting) subsystem mappings for the old thread
            self.conn
                .execute(
                    "DELETE FROM threads_subsystems WHERE thread_id = ?",
                    libsql::params![old_thread_id],
                )
                .await?;

            // 5. Delete old thread
            self.conn
                .execute(
                    "DELETE FROM threads WHERE id = ?",
                    libsql::params![old_thread_id],
                )
                .await?;
        }

        // Use INSERT OR REPLACE to handle updating placeholders.
        // We want to preserve thread_id if it was set by placeholder (which is correct).
        // Actually, if we are "creating" the real message now, we should overwrite the placeholder fields.
        // Ensure the same thread_id is kept if it exists.
        // The caller (main.rs) resolves thread_id before calling create_message.
        // If we found a placeholder, we use its thread_id.
        // So here we just upsert.

        // Blindly replacing might change the thread_id if a different one is passed.
        // But main.rs logic should ensure consistency.
        // Use INSERT OR REPLACE.
        self.conn.execute(
            "INSERT INTO messages (message_id, thread_id, in_reply_to, author, subject, date, body, to_recipients, cc_recipients, git_blob_hash, mailing_list, references_hdr) 
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(message_id) DO UPDATE SET
                thread_id=excluded.thread_id,
                in_reply_to=excluded.in_reply_to,
                author=excluded.author,
                subject=excluded.subject,
                date=excluded.date,
                body=excluded.body,
                to_recipients=excluded.to_recipients,
                cc_recipients=excluded.cc_recipients,
                git_blob_hash=excluded.git_blob_hash,
                mailing_list=excluded.mailing_list,
                references_hdr=excluded.references_hdr",
            libsql::params![message_id, thread_id, in_reply_to, author, subject, date, crate::compression::compress_string_if_needed(body), to, cc, git_blob_hash, mailing_list, references_hdr],
        ).await?;
        Ok(())
    }

    pub async fn create_baseline(
        &self,
        repo_url: Option<&str>,
        branch: Option<&str>,
        commit: Option<&str>,
    ) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM baselines WHERE repo_url IS ? AND branch IS ? AND last_known_commit IS ?",
                libsql::params![repo_url, branch, commit],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            return Ok(row.get(0)?);
        }

        let mut rows = self.conn
            .query(
                "INSERT INTO baselines (repo_url, branch, last_known_commit) VALUES (?, ?, ?) RETURNING id",
                libsql::params![repo_url, branch, commit],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get baseline ID"))
        }
    }

    pub async fn get_baseline_commit(&self, id: i64) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT last_known_commit FROM baselines WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0).ok())
        } else {
            Ok(None)
        }
    }

    /// Record `baseline_id` as the series base of `patchset_id` unless a
    /// lower-numbered part already supplied one. A part re-ingested with
    /// a corrected baseline replaces its own earlier answer.
    ///
    /// Only the first patch's parent is the series base; a later patch's
    /// parent is just the patch before it. An unset baseline takes any
    /// part's, since the cover letter wins the lowest index but usually
    /// carries no base-commit trailer.
    ///
    /// `part_index` is None when the part that supplied `baseline_id` is
    /// unknown, as for a row written before the column existed. Unknown
    /// ranks below every part: any part displaces it, and it displaces
    /// only an unset or equally unknown baseline.
    async fn record_series_baseline(
        &self,
        patchset_id: i64,
        baseline_id: i64,
        part_index: Option<u32>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET baseline_id = ?, baseline_part_index = ?
                 WHERE id = ?
                   AND (baseline_id IS NULL
                        OR baseline_part_index IS NULL
                        OR ? <= baseline_part_index)",
                libsql::params![baseline_id, part_index, patchset_id, part_index],
            )
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_patchset(
        &self,
        thread_id: i64,
        cover_letter_message_id: Option<&str>,
        message_id: &str,
        subject: &str,
        author: &str,
        date: i64,
        total_parts: u32,
        parser_version: i32,
        to: &str,
        cc: &str,
        version: Option<u32>,
        part_index: u32,
        baseline_id: Option<i64>,
        strict_author: bool,
        skip_filters: Option<&Vec<String>>,
        only_filters: Option<&Vec<String>>,
    ) -> Result<Option<i64>> {
        let skip_filters_json = skip_filters.map(|f| serde_json::to_string(f).unwrap_or_default());
        let only_filters_json = only_filters.map(|f| serde_json::to_string(f).unwrap_or_default());
        // 1. Try to find by cover_letter_message_id first (handles placeholders from API/Fetcher)
        let mut clid_candidates = Vec::new();
        if let Some(clid) = cover_letter_message_id {
            clid_candidates.push((clid.to_string(), false));
        }
        // Fallback for single-patch git imports where placeholder is
        // sha@sashiko.local but the actual cover letter becomes the sha
        // itself.  Only add the fallback when no explicit cover letter
        // was provided AND the patch is a singleton (total == 1).
        // Multi-part ranges always have an explicit cover letter ID,
        // and the fallback would incorrectly match patchsets from
        // unrelated submissions that happen to share a commit SHA.
        if cover_letter_message_id.is_none() || total_parts == 1 {
            clid_candidates.push((format!("{}@sashiko.local", message_id), true));
        }

        for (clid, scope_to_thread) in clid_candidates {
            // When using the @sashiko.local fallback, scope the query
            // to the same thread to avoid cross-patchset contamination.
            let query = if scope_to_thread {
                "SELECT id, date, author, subject, subject_index, total_parts, status FROM patchsets WHERE cover_letter_message_id = ? AND thread_id = ?"
            } else {
                "SELECT id, date, author, subject, subject_index, total_parts, status FROM patchsets WHERE cover_letter_message_id = ?"
            };
            let mut rows = if scope_to_thread {
                self.conn
                    .query(query, libsql::params![clid.clone(), thread_id])
                    .await?
            } else {
                self.conn
                    .query(query, libsql::params![clid.clone()])
                    .await?
            };
            while let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0)?;
                let existing_subject: String = row.get(3)?;
                let existing_status: String = row.get(6).unwrap_or_else(|_| "Unknown".to_string());

                let is_placeholder =
                    existing_subject == "(placeholder)" || existing_status == "Fetching";

                let existing_version = crate::patch::parse_subject_version(&existing_subject);
                let v_new = version.unwrap_or(1);
                let v_old = existing_version.unwrap_or(1);
                let versions_compatible = v_new == v_old;

                let index_collision = if part_index == 0 {
                    false
                } else {
                    let mut p_rows = self
                        .conn
                        .query(
                            "SELECT 1 FROM patches WHERE patchset_id = ? AND part_index = ? AND message_id != ?",
                            libsql::params![id, part_index, message_id],
                        )
                        .await?;
                    p_rows.next().await.ok().flatten().is_some()
                };

                if index_collision || (!is_placeholder && !versions_compatible) {
                    continue;
                }

                // Found it! Use this ID. We'll update its fields below.
                let subject_index: u32 = row.get(4).unwrap_or(9999);
                let existing_total: u32 = row.get(5).unwrap_or(1);

                // Prevent downgrading a series to a singleton if we already have multiple parts.
                // This handles cases where a singleton root (1/1) overwrites a series (N/N) inferred from replies.
                let final_total = if total_parts == 1 && existing_total > 1 {
                    existing_total
                } else {
                    total_parts
                };

                // We proceed to update this record with the full metadata
                self.conn.execute(
                    "UPDATE patchsets SET thread_id = ?, author = ?, total_parts = ?, parser_version = ?, to_recipients = ?, cc_recipients = ? WHERE id = ?",
                    libsql::params![thread_id, author, final_total, parser_version, to, cc, id],
                ).await?;

                if let Some(real_clid) = cover_letter_message_id {
                    self.conn
                        .execute(
                            "UPDATE patchsets SET cover_letter_message_id = ? WHERE id = ?",
                            libsql::params![real_clid, id],
                        )
                        .await?;
                }

                if let Some(bid) = baseline_id {
                    self.record_series_baseline(id, bid, Some(part_index))
                        .await?;
                }

                // Update subject if this is a better index (e.g. going from placeholder to real subject)
                if part_index < subject_index {
                    self.conn
                        .execute(
                            "UPDATE patchsets SET subject = ?, subject_index = ? WHERE id = ?",
                            libsql::params![subject, part_index, id],
                        )
                        .await?;
                }

                self.conn.execute(
                    "UPDATE patchsets SET status = 'Incomplete' WHERE id = ? AND status = 'Fetching'",
                    libsql::params![id],
                ).await?;

                self.conn.execute(
                    "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status IN ('Incomplete', 'Fetching')",
                    libsql::params![id],
                ).await?;

                return Ok(Some(id));
            }
        }

        // 2. Normal matching logic: Find candidate patchsets in this thread OR matching author/time
        // We expand the search window to finding ANY patchset by this author in the last 24h
        let window_start = date - 86400;
        let window_end = date + 86400;
        let mut rows = self
            .conn
            .query(
                "SELECT id, date, author, subject, subject_index, total_parts, received_parts, cover_letter_message_id, thread_id, baseline_id, baseline_part_index FROM patchsets
                 WHERE thread_id = ? OR (author = ? AND date BETWEEN ? AND ?)",
                libsql::params![thread_id, author, window_start, window_end],
            )
            .await?;

        let mut matches = Vec::new();

        while let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let existing_date: i64 = row.get(1)?;
            let existing_author: String = row.get(2)?;
            let existing_subject: String = row.get(3)?;
            let existing_subject_index: u32 = row.get(4).unwrap_or(9999);
            let existing_total: u32 = row.get(5).unwrap_or(1);
            let existing_received: u32 = row.get(6).unwrap_or(0);
            let existing_cover_id: Option<String> = row.get(7).ok();
            let existing_thread_id: Option<i64> = row.get(8).ok();
            let existing_baseline_id: Option<i64> = row.get(9).ok();
            let existing_baseline_part: Option<u32> = row.get(10).ok();

            // Check if this message is already part of this patchset (Duplicate processing)
            // 1. Check if it is the cover letter.
            let is_cover_duplicate = existing_cover_id.as_deref() == Some(message_id);

            // 2. Check if it is an existing patch.
            let is_patch_duplicate = if !is_cover_duplicate {
                let mut p_rows = self
                    .conn
                    .query(
                        "SELECT 1 FROM patches WHERE patchset_id = ? AND message_id = ?",
                        libsql::params![id, message_id],
                    )
                    .await?;
                p_rows.next().await.ok().flatten().is_some()
            } else {
                false
            };

            let is_duplicate = is_cover_duplicate || is_patch_duplicate;

            // If the patchset is already full, do not merge more patches into it,
            // UNLESS it is a duplicate of a message already in the set.
            // This prevents merging unrelated patchsets that happen to look similar (same author/size).
            if existing_received >= existing_total && !is_duplicate && part_index != 0 {
                continue;
            }

            // Parse version from existing subject
            let existing_version = crate::patch::parse_subject_version(&existing_subject);

            // Clean subjects for comparison
            let clean_new = crate::patch::clean_subject(subject);
            let clean_old = crate::patch::clean_subject(&existing_subject);

            // Check for index collision
            // If the patchset already contains a patch with this index (and different message_id), it's a collision.
            // This prevents merging [PATCH 1/2] Series A and [PATCH 1/2] Series B.
            let index_collision = if part_index == 0 {
                existing_cover_id.is_some()
                    && existing_cover_id.as_deref() != Some(message_id)
                    && existing_subject_index == 0
            } else {
                let mut p_rows = self
                    .conn
                    .query(
                        "SELECT 1 FROM patches WHERE patchset_id = ? AND part_index = ? AND message_id != ?",
                        libsql::params![id, part_index, message_id],
                    )
                    .await?;
                p_rows.next().await.ok().flatten().is_some()
            };

            let mut existing_msgid_prefix = None;
            if let Some(ref cover_id) = existing_cover_id {
                existing_msgid_prefix =
                    Some(cover_id.split('-').next().unwrap_or(cover_id).to_string());
            } else {
                let mut p_rows = self
                    .conn
                    .query(
                        "SELECT message_id FROM patches WHERE patchset_id = ? LIMIT 1",
                        libsql::params![id],
                    )
                    .await?;
                if let Ok(Some(p_row)) = p_rows.next().await {
                    let pid: String = p_row.get(0)?;
                    existing_msgid_prefix = Some(pid.split('-').next().unwrap_or(&pid).to_string());
                }
            }

            let new_msgid_prefix = message_id.split('-').next().unwrap_or(message_id);
            let msgid_prefix_match = existing_msgid_prefix.as_deref() == Some(new_msgid_prefix)
                && new_msgid_prefix.len() > 10;

            // Matching logic:
            // 1. Author matches OR it's a multi-part series with matching total_parts (trusting thread context)
            //    BUT strict_author enforces strict author matching (for Email/NNTP).
            // 2. Time must be close (within 24 hours / 86400s)
            // 3. Total parts must match
            // 4. Versions must match (treating None as v1)
            // 5. For singletons (total=1), Subject must match (fuzzy) to avoid merging unrelated patches

            let v_new = version.unwrap_or(1);
            let v_old = existing_version.unwrap_or(1);
            let versions_compatible = v_new == v_old;

            let is_singleton = total_parts == 1;
            // For singletons, we require the subject to be somewhat similar to avoid merging unrelated patches.
            let subject_match = if is_singleton {
                if subject == existing_subject {
                    true
                } else {
                    // Allow merging 0/1 (cover) and 1/1 (patch) even if subjects differ
                    if (part_index == 0 && existing_subject_index == 1)
                        || (part_index == 1 && existing_subject_index == 0)
                    {
                        true
                    } else {
                        clean_new == clean_old
                    }
                }
            } else {
                // For series:
                // If we are replacing/matching the SAME index as the one that defined the patchset subject,
                // we require the subjects to match.
                // e.g. [PATCH 1/2] Series A vs [PATCH 1/2] Series B -> Mismatch.
                if part_index == existing_subject_index {
                    clean_new == clean_old
                } else {
                    true // For other parts (1/N vs 2/N), subjects differ naturally.
                }
            };

            // Relaxed author check logic
            let author_match = crate::patch::authors_match(&existing_author, author);
            let series_match = (total_parts > 1 && total_parts == existing_total)
                || existing_total == 1
                || total_parts == 1;

            let author_or_series_match = if strict_author {
                author_match
            } else {
                author_match || series_match
            };

            // Prefix matching (to separate different series from same author)
            let same_thread = existing_thread_id == Some(thread_id);
            let prefix_match = if same_thread {
                true // Trust thread
            } else {
                let new_prefixes = crate::patch::get_subject_prefixes(subject);
                let old_prefixes = crate::patch::get_subject_prefixes(&existing_subject);
                new_prefixes == old_prefixes
            };

            // Thread Enforcement: To prevent cross-thread "stealing" of patches for resends of the same series,
            // we strictly require multi-part series patches to belong to the same thread,
            // unless they share a git send-email Message-ID prefix indicating they were sent together unthreaded.
            let thread_compatible = same_thread || is_singleton || msgid_prefix_match;

            if author_or_series_match
                && (!strict_author || (date - existing_date).abs() < 86400)
                && (versions_compatible || same_thread)
                && (total_parts == existing_total || existing_total == 1 || total_parts == 1)
                && subject_match
                && prefix_match
                && thread_compatible
                && !index_collision
            {
                matches.push((
                    id,
                    existing_subject_index,
                    existing_baseline_id,
                    existing_baseline_part,
                ));
            }
        }

        if !matches.is_empty() {
            // Sort matches to pick the "best" one to keep (e.g. oldest ID or one with lowest subject index)
            // Let's keep the one with the lowest ID (created first)
            matches.sort_by_key(|k| k.0);

            let target_id = matches[0].0;
            let mut current_subject_index = matches[0].1;

            // If we have multiple matches, merge others into target_id
            if matches.len() > 1 {
                let tx = self.conn.transaction().await?;
                for (merge_from_id, merge_subject_index, merge_baseline_id, merge_baseline_part) in
                    matches.iter().skip(1)
                {
                    let merge_from_id = *merge_from_id;
                    info!("Merging patchset {} into {}", merge_from_id, target_id);

                    // Reassign patches: first remove duplicates that already exist on target_id
                    // to prevent unique constraint conflicts and lingering foreign key references.
                    tx.execute(
                        "DELETE FROM patches WHERE patchset_id = ? AND message_id IN (SELECT message_id FROM patches WHERE patchset_id = ?)",
                        libsql::params![merge_from_id, target_id],
                    )
                    .await?;

                    // Reassign remaining patches
                    tx.execute(
                        "UPDATE OR IGNORE patches SET patchset_id = ? WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
                    )
                    .await?;

                    // Reassign reviews
                    tx.execute(
                        "UPDATE reviews SET patchset_id = ? WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
                    )
                    .await?;

                    // Merge subsystems
                    tx.execute(
                        "INSERT OR IGNORE INTO patchsets_subsystems (patchset_id, subsystem_id)
                         SELECT ?, subsystem_id FROM patchsets_subsystems WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
                    )
                    .await?;
                    tx.execute(
                        "DELETE FROM patchsets_subsystems WHERE patchset_id = ?",
                        libsql::params![merge_from_id],
                    )
                    .await?;

                    // If the merged patchset had a better subject index, track it
                    if *merge_subject_index < current_subject_index {
                        current_subject_index = *merge_subject_index;
                    }

                    // A baseline from a lower-numbered part than the target's
                    // is lost when the row is deleted below, with no part left
                    // to supply it again.
                    if let Some(bid) = *merge_baseline_id {
                        tx.execute(
                            "UPDATE patchsets SET baseline_id = ?, baseline_part_index = ?
                             WHERE id = ?
                               AND (baseline_id IS NULL
                                    OR baseline_part_index IS NULL
                                    OR ? <= baseline_part_index)",
                            libsql::params![
                                bid,
                                *merge_baseline_part,
                                target_id,
                                *merge_baseline_part
                            ],
                        )
                        .await?;
                    }

                    // Delete the merged patchset
                    tx.execute(
                        "DELETE FROM patchsets WHERE id = ?",
                        libsql::params![merge_from_id],
                    )
                    .await?;
                }
                tx.commit().await?;
            }

            // Update the target patchset
            self.conn.execute(
                "UPDATE patchsets SET author = ?, total_parts = ?, parser_version = ?, to_recipients = ?, cc_recipients = ? WHERE id = ?",
                libsql::params![author, total_parts, parser_version, to, cc, target_id],
            ).await?;

            if skip_filters_json.is_some() || only_filters_json.is_some() {
                self.conn.execute(
                    "UPDATE patchsets SET skip_filters = COALESCE(?, skip_filters), only_filters = COALESCE(?, only_filters) WHERE id = ?",
                    libsql::params![skip_filters_json.clone(), only_filters_json.clone(), target_id],
                ).await?;
            }

            if let Some(bid) = baseline_id {
                self.record_series_baseline(target_id, bid, Some(part_index))
                    .await?;
            }

            // Conditionally update subject
            // Note: We check against the best index found among all merged sets OR the new part_index
            if part_index < current_subject_index {
                self.conn
                    .execute(
                        "UPDATE patchsets SET subject = ?, subject_index = ? WHERE id = ?",
                        libsql::params![subject, part_index, target_id],
                    )
                    .await?;
            } else if matches.len() > 1 {
                // If we merged, we might need to update the subject index of the target to the best one we found.
                // But we don't have the subject string from the merged one easily available here.
                // However, the existing target subject is likely fine unless part_index is better.
                // Update subject_index to be correct if a better one was merged.
                // Actually, if matches[i].1 was better, we should have used its subject.
                // But that's complicated. Assuming the target (oldest) usually has the cover letter or we eventually find it.
                // Simplification: We only update if CURRENT patch is better.
                // If we merged a patchset that HAD the cover letter, we ideally want that subject.
                // But we lost it.
                // TODO: Optimize merge subject selection. For now, this is better than duplicates.
            }

            if let Some(clid) = cover_letter_message_id {
                self.conn
                    .execute(
                        "UPDATE patchsets SET cover_letter_message_id = ? WHERE id = ?",
                        libsql::params![clid, target_id],
                    )
                    .await?;
            }

            // Recalculate received parts for target (in case we merged)
            self.conn
            .execute(
                "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                libsql::params![target_id, target_id],
            )
            .await?;

            self.conn.execute(
                "UPDATE patchsets SET status = 'Incomplete' WHERE id = ? AND status = 'Fetching'",
                libsql::params![target_id],
            ).await?;

            self.conn.execute(
                "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status IN ('Incomplete', 'Fetching')",
                libsql::params![target_id],
            ).await?;

            return Ok(Some(target_id));
        }

        // No match found, create new patchset
        let mut rows = self.conn
            .query(
                "INSERT INTO patchsets (thread_id, cover_letter_message_id, subject, author, date, total_parts, received_parts, status, parser_version, to_recipients, cc_recipients, subject_index, baseline_id, baseline_part_index, skip_filters, only_filters)
                 VALUES (?, ?, ?, ?, ?, ?, 0, 'Incomplete', ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
                libsql::params![thread_id, cover_letter_message_id, subject, author, date, total_parts, parser_version, to, cc, part_index, baseline_id, baseline_id.map(|_| part_index), skip_filters_json.clone(), only_filters_json.clone()],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            Ok(Some(id))
        } else {
            Err(anyhow::anyhow!(
                "Failed to retrieve patchset ID after insert"
            ))
        }
    }

    /// Inserts or updates a patch when no stable Git patch ID is available.
    ///
    /// Re-ingesting the same message with an unchanged diff preserves any
    /// existing Git patch ID. If the diff changed, the old ID is cleared so
    /// it cannot refer to different patch content.
    pub async fn create_patch(
        &self,
        patchset_id: i64,
        message_id: &str,
        part_index: u32,
        diff: &str,
    ) -> Result<i64> {
        self.create_patch_with_git_patch_id(patchset_id, message_id, part_index, diff, None)
            .await
    }

    /// Inserts or updates a patch and associates its stable Git patch ID.
    pub async fn create_patch_with_git_patch_id(
        &self,
        patchset_id: i64,
        message_id: &str,
        part_index: u32,
        diff: &str,
        git_patch_id: Option<&str>,
    ) -> Result<i64> {
        // Check if index collision occurs for this patchset
        let collision_exists: bool = {
            let mut rows = self
                .conn
                .query(
                    "SELECT 1 FROM patches WHERE patchset_id = ? AND part_index = ? AND message_id != ?",
                    libsql::params![patchset_id, part_index, message_id],
                )
                .await?;
            rows.next().await.ok().flatten().is_some()
        };

        if collision_exists {
            return Err(anyhow::anyhow!(
                "Index collision: index {} already exists in patchset {}",
                part_index,
                patchset_id
            ));
        }

        // Check if the patch exists, preserving its patch ID only when the
        // decompressed content is unchanged.
        let old_patch: Option<(String, Option<String>)> = {
            let mut rows = self
                .conn
                .query(
                    "SELECT diff, git_patch_id FROM patches
                     WHERE patchset_id = ? AND message_id = ?",
                    libsql::params![patchset_id, message_id],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                Some((
                    crate::compression::get_compressed_string(&row, 0)?,
                    row.get(1).ok(),
                ))
            } else {
                None
            }
        };
        let existing_in_patchset = old_patch.is_some();
        let git_patch_id = git_patch_id.map(str::to_owned).or_else(|| {
            old_patch
                .as_ref()
                .filter(|(old_diff, _)| old_diff == diff)
                .and_then(|(_, patch_id)| patch_id.clone())
        });

        // Insert or update within THIS patchset.
        self.conn
            .execute(
                "INSERT INTO patches
                    (patchset_id, message_id, part_index, diff, git_patch_id)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(patchset_id, message_id) DO UPDATE SET
                    part_index=excluded.part_index,
                    diff=excluded.diff,
                    git_patch_id=excluded.git_patch_id",
                libsql::params![
                    patchset_id,
                    message_id,
                    part_index,
                    crate::compression::compress_string_if_needed(diff),
                    git_patch_id
                ],
            )
            .await?;

        // Update received_parts to match the physical patch count in this patchset
        if !existing_in_patchset {
            self.conn
                .execute(
                    "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                    libsql::params![patchset_id, patchset_id],
                )
                .await?;
        }

        // Check if complete and update status
        // We transition from 'Incomplete' OR 'Fetching' to 'Pending' (ready for review)
        self.conn
            .execute(
                "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status IN ('Incomplete', 'Fetching')",
                libsql::params![patchset_id],
            )
            .await?;

        // Get the patch ID for this patch in this patchset
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patches WHERE patchset_id = ? AND message_id = ?",
                libsql::params![patchset_id, message_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get patch ID"))
        }
    }

    fn build_search(
        &self,
        query: Option<String>,
        mailing_list: Option<String>,
        target: &str,
    ) -> (String, Vec<String>) {
        let mut conditions = Vec::new();
        let mut params = Vec::new();

        // Always exclude placeholders
        conditions.push("subject != '(placeholder)'".to_string());

        if let Some(list) = mailing_list
            && !list.is_empty()
        {
            if target == "patchset" {
                // Filter patchsets where any patch OR the cover letter is in the mailing list
                // We use p.id to avoid ambiguity with joined tables (e.g. subsystems.id)
                conditions.push(
                    "p.id IN (
                        SELECT patchset_id FROM patches p2 
                        JOIN messages m ON p2.message_id = m.message_id 
                        JOIN messages_mailing_lists mml ON m.id = mml.message_id 
                        JOIN mailing_lists ml ON mml.mailing_list_id = ml.id 
                        WHERE ml.nntp_group = ?
                        UNION
                        SELECT ps.id FROM patchsets ps 
                        JOIN messages m ON ps.cover_letter_message_id = m.message_id 
                        JOIN messages_mailing_lists mml ON m.id = mml.message_id 
                        JOIN mailing_lists ml ON mml.mailing_list_id = ml.id 
                        WHERE ml.nntp_group = ?
                    )"
                    .to_string(),
                );
                params.push(list.clone());
                params.push(list);
            } else {
                // Filter messages
                conditions.push("id IN (SELECT message_id FROM messages_mailing_lists mml JOIN mailing_lists ml ON mml.mailing_list_id = ml.id WHERE ml.nntp_group = ?)".to_string());
                params.push(list);
            }
        }

        if let Some(q) = query {
            let q = q.trim();
            if !q.is_empty() {
                if let Some(val) = q.strip_prefix("author:") {
                    conditions.push("author LIKE ?".to_string());
                    params.push(format!("%{}%", val.trim()));
                } else if let Some(val) = q.strip_prefix("subject:") {
                    conditions.push("subject LIKE ?".to_string());
                    params.push(format!("%{}%", val.trim()));
                } else if let Some(val) = q.strip_prefix("date:") {
                    conditions.push("datetime(date, 'unixepoch') LIKE ?".to_string());
                    params.push(format!("%{}%", val.trim()));
                } else if let Some(val) = q.strip_prefix("subsystem:") {
                    let sub_query = if target == "patchset" {
                        "p.id IN (SELECT patchset_id FROM patchsets_subsystems ps JOIN subsystems s ON ps.subsystem_id = s.id WHERE s.name LIKE ?)"
                    } else {
                        "id IN (SELECT message_id FROM messages_subsystems ms JOIN subsystems s ON ms.subsystem_id = s.id WHERE s.name LIKE ?)"
                    };
                    conditions.push(sub_query.to_string());
                    params.push(format!("%{}%", val.trim()));
                } else {
                    conditions.push("(subject LIKE ? OR author LIKE ?)".to_string());
                    params.push(format!("%{}%", q));
                    params.push(format!("%{}%", q));
                }
            }
        }

        if conditions.is_empty() {
            (String::new(), vec![])
        } else {
            (format!("WHERE {}", conditions.join(" AND ")), params)
        }
    }

    pub async fn set_patchset_embargo_until(&self, id: i64, embargo_until: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET embargo_until = ? WHERE id = ?",
                libsql::params![embargo_until, id],
            )
            .await?;
        Ok(())
    }

    pub async fn clear_patchset_embargo(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets
                 SET embargo_until = NULL, embargo_release_started_at = NULL
                 WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_patchsets(
        &self,
        limit: usize,
        offset: usize,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<Vec<PatchsetRow>> {
        let (where_clause, params) = self.build_search(query, mailing_list, "patchset");
        // We use p.* alias implicitely by using unqualified names in WHERE which is fine given no collisions.
        // But for clarity/safety we should alias in FROM.
        // build_search returns "WHERE author ...".

        let sql = format!(
            "SELECT p.id, p.subject, p.status, p.thread_id, p.author, p.date, p.cover_letter_message_id, p.total_parts, p.received_parts, GROUP_CONCAT(s.name, ','),
             COALESCE(f.low, 0), COALESCE(f.medium, 0), COALESCE(f.high, 0), COALESCE(f.critical, 0), p.baseline_id, p.failed_reason, p.target_review_count, p.skip_filters, p.only_filters,
             p.embargo_until, p.mr_url, p.mr_title, p.mr_number, p.slug
             FROM (
                 SELECT id FROM patchsets p
                 {}
                 ORDER BY p.date DESC LIMIT ? OFFSET ?
             ) p_lim
             JOIN patchsets p ON p_lim.id = p.id
             LEFT JOIN patchsets_subsystems ps ON p.id = ps.patchset_id
             LEFT JOIN subsystems s ON ps.subsystem_id = s.id
             LEFT JOIN (
                SELECT r.patchset_id,
                    SUM(CASE WHEN f.severity = 1 AND COALESCE(f.preexisting, 0) = 0 THEN 1 ELSE 0 END) as low,
                    SUM(CASE WHEN f.severity = 2 AND COALESCE(f.preexisting, 0) = 0 THEN 1 ELSE 0 END) as medium,
                    SUM(CASE WHEN f.severity = 3 AND COALESCE(f.preexisting, 0) = 0 THEN 1 ELSE 0 END) as high,
                    SUM(CASE WHEN f.severity = 4 AND COALESCE(f.preexisting, 0) = 0 THEN 1 ELSE 0 END) as critical
                FROM reviews r
                JOIN findings f ON r.id = f.review_id
                WHERE r.status = 'Reviewed'
                GROUP BY r.patchset_id
             ) f ON p.id = f.patchset_id
             GROUP BY p.id
             ORDER BY p.date DESC",
            where_clause
        );

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }
        args.push(libsql::Value::Integer(limit as i64));
        args.push(libsql::Value::Integer(offset as i64));

        let mut rows = self.conn.query(&sql, args).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;

        let mut patchsets = Vec::new();
        loop {
            match rows.next().await {
                Ok(Some(row)) => {
                    let subsystems_str: Option<String> = row.get(9).ok();
                    let subsystems = if let Some(s) = subsystems_str {
                        s.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    } else {
                        Vec::new()
                    };

                    let embargo_until: Option<i64> = row.get(19).ok();
                    let is_embargoed = if let Some(until) = embargo_until {
                        until > now
                    } else {
                        false
                    };

                    let (low, medium, high, critical) = if is_embargoed {
                        (0, 0, 0, 0)
                    } else {
                        (
                            row.get::<Option<i64>>(10).ok().flatten().unwrap_or(0),
                            row.get::<Option<i64>>(11).ok().flatten().unwrap_or(0),
                            row.get::<Option<i64>>(12).ok().flatten().unwrap_or(0),
                            row.get::<Option<i64>>(13).ok().flatten().unwrap_or(0),
                        )
                    };

                    let mut status: Option<String> = row.get(2).ok();
                    if is_embargoed && status.as_deref() == Some("Reviewed") {
                        status = Some("Embargoed".to_string());
                    }

                    patchsets.push(PatchsetRow {
                        id: row.get(0).unwrap_or_default(),
                        subject: row.get(1).ok(),
                        status,
                        thread_id: row.get(3).ok(),
                        author: row.get(4).ok(),
                        date: row.get(5).ok(),
                        message_id: row.get(6).ok(),
                        total_parts: row.get(7).ok(),
                        received_parts: row.get(8).ok(),
                        mailing_lists: subsystems.clone(),
                        subsystems,
                        findings_low: Some(low),
                        findings_medium: Some(medium),
                        findings_high: Some(high),
                        findings_critical: Some(critical),
                        baseline_id: row.get(14).ok(),
                        failed_reason: row.get(15).ok(),
                        target_review_count: row.get(16).ok(),
                        skip_filters: row.get(17).ok(),
                        only_filters: row.get(18).ok(),
                        model_name: None,
                        prompts_git_hash: None,
                        baseline_logs: None,
                        provider: None,
                        embargo_until: row.get(19).ok(),
                        mr_url: row.get(20).ok(),
                        mr_title: row.get(21).ok(),
                        mr_number: row.get(22).ok(),
                        slug: row.get(23).ok(),
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("Error fetching row: {:?}", e);
                    break;
                }
            }
        }
        Ok(patchsets)
    }

    pub async fn get_messages(
        &self,
        limit: usize,
        offset: usize,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<Vec<MessageRow>> {
        let (where_clause, params) = self.build_search(query, mailing_list, "message");
        let sql = format!(
            "SELECT id, message_id, thread_id, in_reply_to, author, subject, date, NULL as body, to_recipients, cc_recipients, git_blob_hash, mailing_list, references_hdr FROM messages {} ORDER BY date DESC LIMIT ? OFFSET ?",
            where_clause
        );

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }
        args.push(libsql::Value::Integer(limit as i64));
        args.push(libsql::Value::Integer(offset as i64));

        let mut rows = self.conn.query(&sql, args).await?;
        let mut messages = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            messages.push(MessageRow {
                id: row.get(0)?,
                message_id: row.get(1)?,
                thread_id: row.get(2).ok(),
                in_reply_to: row.get(3).ok(),
                author: row.get(4).ok(),
                subject: row.get(5).ok(),
                date: row.get(6).ok(),
                body: None,
                to: row.get(8).ok(),
                cc: row.get(9).ok(),
                git_blob_hash: row.get(10).ok(),
                mailing_list: row.get(11).ok(),
                diff: None,
                references_hdr: row.get(12).ok(),
                thread: None,
            });
        }
        Ok(messages)
    }

    pub async fn count_patchsets(
        &self,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<usize> {
        let (where_clause, params) = self.build_search(query, mailing_list, "patchset");
        // We must alias patchsets as p because build_search uses p.id for filters
        let sql = format!("SELECT COUNT(*) FROM patchsets p {}", where_clause);

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }

        let mut rows = self.conn.query(&sql, args).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn count_pending_patches(&self) -> Result<usize> {
        let mut rows = self.conn.query(
            "SELECT COUNT(p.id) FROM patches p JOIN patchsets ps ON p.patchset_id = ps.id 
             WHERE ps.status IN ('Pending', 'In Review') AND p.status IS NULL
             AND p.id NOT IN (SELECT patch_id FROM reviews WHERE status IN ('In Review', 'Applying') AND patch_id IS NOT NULL)",
            ()
        ).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn count_reviewing_patches(&self) -> Result<usize> {
        let mut rows = self.conn.query(
            "SELECT COUNT(DISTINCT patch_id) FROM reviews WHERE status IN ('In Review', 'Applying') AND patch_id IS NOT NULL",
            ()
        ).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn count_messages(
        &self,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<usize> {
        let (where_clause, params) = self.build_search(query, mailing_list, "message");
        let sql = format!("SELECT COUNT(*) FROM messages {}", where_clause);

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }

        let mut rows = self.conn.query(&sql, args).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn get_patchset_details(
        &self,
        id: i64,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.subject, p.status, p.to_recipients, p.cc_recipients,
                    p.author, p.date, p.cover_letter_message_id, p.thread_id,
                    p.total_parts, p.received_parts, p.failed_reason,
                    p.model_name, p.prompts_git_hash, p.baseline_logs, p.baseline_id, p.provider,
                    p.embargo_until, p.mr_url, p.slug
                FROM patchsets p
                WHERE p.id = ?",
                libsql::params![id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let pid: i64 = row.get(0)?;
            let subject: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let to: Option<String> = row.get(3).ok();
            let cc: Option<String> = row.get(4).ok();
            let author: Option<String> = row.get(5).ok();
            let date: Option<i64> = row.get(6).ok();
            let mid: Option<String> = row.get(7).ok();
            let thread_id: Option<i64> = row.get(8).ok();
            let total_parts: Option<u32> = row.get(9).ok();
            let received_parts: Option<u32> = row.get(10).ok();
            let failed_reason: Option<String> = row.get(11).ok();
            let model_name: Option<String> = row.get(12).ok();
            let prompts_git_hash: Option<String> = row.get(13).ok();
            let baseline_logs: Option<String> =
                crate::compression::get_compressed_string_opt(&row, 14).unwrap_or(None);
            let baseline_id: Option<i64> = row.get(15).ok();
            let provider: Option<String> = row.get(16).ok();
            let embargo_until: Option<i64> = row.get(17).ok();
            let mr_url: Option<String> = row.get(18).ok();
            let slug: Option<String> = row.get(19).ok();
            // Fetch baseline details if needed
            let baseline = if let Some(bid) = baseline_id {
                let mut browse = self
                    .conn
                    .query(
                        "SELECT repo_url, branch, last_known_commit FROM baselines WHERE id = ?",
                        libsql::params![bid],
                    )
                    .await?;
                if let Ok(Some(brow)) = browse.next().await {
                    Some(serde_json::json!({
                       "repo_url": brow.get::<Option<String>>(0).ok(),
                       "branch": brow.get::<Option<String>>(1).ok(),
                       "commit": brow.get::<Option<String>>(2).ok(),
                    }))
                } else {
                    None
                }
            } else {
                None
            };

            // Calculate pagination
            let limit_val = limit.unwrap_or(50);
            let page_val = page.unwrap_or(1);
            let offset_val = limit_val * (page_val.saturating_sub(1));

            // Fetch subsystems
            let mut subsystems = Vec::new();
            let mut sub_rows = self
                .conn
                .query(
                    "SELECT s.name FROM subsystems s
                 JOIN patchsets_subsystems ps ON s.id = ps.subsystem_id
                 WHERE ps.patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            while let Ok(Some(row)) = sub_rows.next().await {
                subsystems.push(row.get::<String>(0)?);
            }

            let mut total_patches = 0;
            let mut count_rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM patches WHERE patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            if let Ok(Some(row)) = count_rows.next().await {
                total_patches = row.get::<i64>(0)?;
            }

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;

            let is_embargoed = if let Some(until) = embargo_until {
                until > now
            } else {
                false
            };

            // Fetch patches with subject and msg_db_id
            let mut patches = Vec::new();
            let mut patch_ids = Vec::new();
            let mut patch_rows = self
                .conn
                .query(
                    "SELECT p.id, p.message_id, p.part_index, m.id, m.subject, p.status, p.apply_error, 
                            eo.status as email_status, eo.to_addresses, eo.cc_addresses
                 FROM patches p
                 LEFT JOIN messages m ON p.message_id = m.message_id
                 LEFT JOIN email_outbox eo ON eo.patch_id = p.id
                 WHERE p.patchset_id = ? 
                 ORDER BY p.part_index ASC
                 LIMIT ? OFFSET ?",
                    libsql::params![pid, limit_val, offset_val],
                )
                .await?;
            #[allow(clippy::similar_names)]
            while let Ok(Some(p)) = patch_rows.next().await {
                let p_id: i64 = p.get(0)?;
                patch_ids.push(p_id);
                let mut p_status = p.get::<Option<String>>(5).ok().flatten();
                if is_embargoed && p_status.as_deref() == Some("Reviewed") {
                    p_status = Some("Embargoed".to_string());
                }
                patches.push(serde_json::json!({
                    "id": p_id,
                    "message_id": p.get::<String>(1)?,
                    "part_index": p.get::<Option<i64>>(2).ok(),
                    "msg_db_id": p.get::<Option<i64>>(3).ok(),
                    "subject": p.get::<Option<String>>(4).ok(),
                    "status": p_status,
                    "apply_error": p.get::<Option<String>>(6).ok(),
                    "email_status": p.get::<Option<String>>(7).ok(),
                    "email_to": p.get::<Option<String>>(8).ok(),
                    "email_cc": p.get::<Option<String>>(9).ok(),
                }));
            }

            // Fetch reviews
            let mut reviews = Vec::new();
            let mut in_clause = patch_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            if in_clause.is_empty() {
                in_clause = "-1".to_string(); // Fallback so SQL doesn't error
            }
            let query_str = format!(
                "SELECT r.summary, r.created_at, ai.input_context, ai.output_raw, 
                        r.result_description, r.status, r.inline_review, r.logs, ai.tokens_in, ai.tokens_out, r.patch_id, r.id, ai.tokens_cached
                 FROM reviews r
                 LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
                 WHERE r.patchset_id = ? AND (r.patch_id IS NULL OR r.patch_id IN ({}))
                 ORDER BY r.created_at ASC", in_clause);

            let mut params = vec![libsql::Value::Integer(pid)];
            for &pid_val in &patch_ids {
                params.push(libsql::Value::Integer(pid_val));
            }

            let mut rev_rows = self.conn.query(&query_str, params).await?;

            while let Ok(Some(r)) = rev_rows.next().await {
                reviews.push(serde_json::json!({
                    "summary": r.get::<Option<String>>(0).ok(),
                    "created_at": r.get::<Option<i64>>(1).ok(),
                    "output": crate::compression::get_compressed_string_opt(&r, 3).unwrap_or(None),
                    "result": r.get::<Option<String>>(4).ok(),
                    "status": r.get::<Option<String>>(5).ok(),
                    "inline_review": crate::compression::get_compressed_string_opt(&r, 6).unwrap_or(None),
                    "logs": crate::compression::get_compressed_string_opt(&r, 7).unwrap_or(None),
                    "tokens_in": r.get::<Option<u32>>(8).ok(),
                    "tokens_out": r.get::<Option<u32>>(9).ok(),
                    "patch_id": r.get::<Option<i64>>(10).ok(),
                    "id": r.get::<i64>(11).ok(),
                    "tokens_cached": r.get::<Option<u32>>(12).ok(),
                    "model": model_name.clone(),
                    "provider": provider.clone(),
                    "prompts_hash": prompts_git_hash.clone(),
                    "baseline": baseline.clone()
                }));
            }

            // Fetch thread messages
            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT id, message_id, author, date, subject, in_reply_to FROM messages WHERE thread_id = ? AND subject != '(placeholder)' ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "id": m.get::<i64>(0)?,
                        "message_id": m.get::<String>(1)?,
                        "author": m.get::<Option<String>>(2).ok(),
                        "date": m.get::<Option<i64>>(3).ok(),
                        "subject": m.get::<Option<String>>(4).ok(),
                        "in_reply_to": m.get::<Option<String>>(5).ok(),
                    }));
                }
            }

            let mut final_status = status;
            if is_embargoed && final_status.as_deref() == Some("Reviewed") {
                final_status = Some("Embargoed".to_string());
            }

            let reviews = if is_embargoed { Vec::new() } else { reviews };

            let bugs = self.list_bugs_for_patchset(pid).await.unwrap_or_default();
            let bugs_json = bugs
                .into_iter()
                .map(|(bug, is_new)| bug_reference_json(&bug, is_new))
                .collect::<Vec<_>>();

            Ok(Some(serde_json::json!({
                "id": pid,
                "message_id": mid,
                "subject": subject,
                "author": author,
                "date": date,
                "status": final_status,
                "failed_reason": failed_reason,
                "to": to,
                "cc": cc,
                "total_parts": total_parts,
                "total_patches_in_db": total_patches,
                "page": page_val,
                "limit": limit_val,
                "received_parts": received_parts,
                "reviews": reviews,
                "bugs": bugs_json,
                "patches": patches,
                "thread": messages,
                "mailing_lists": subsystems.clone(),
                "subsystems": subsystems,
                "model_name": model_name,
                "prompts_git_hash": prompts_git_hash,
                "baseline_logs": baseline_logs,
                "baseline": baseline,
                "provider": provider,
                "embargo_until": embargo_until,
                "mr_url": mr_url,
                "slug": slug
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_patchset_summary(
        &self,
        id: i64,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.subject, p.status, p.to_recipients, p.cc_recipients,
                    p.author, p.date, p.cover_letter_message_id, p.thread_id,
                    p.total_parts, p.received_parts, p.failed_reason,
                    p.model_name, p.prompts_git_hash, p.baseline_logs, p.baseline_id, p.provider,
                    p.embargo_until, p.mr_url, p.slug
                FROM patchsets p
                WHERE p.id = ?",
                libsql::params![id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let pid: i64 = row.get(0)?;
            let subject: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let to: Option<String> = row.get(3).ok();
            let cc: Option<String> = row.get(4).ok();
            let author: Option<String> = row.get(5).ok();
            let date: Option<i64> = row.get(6).ok();
            let mid: Option<String> = row.get(7).ok();
            let thread_id: Option<i64> = row.get(8).ok();
            let total_parts: Option<u32> = row.get(9).ok();
            let received_parts: Option<u32> = row.get(10).ok();
            let failed_reason: Option<String> = row.get(11).ok();
            let model_name: Option<String> = row.get(12).ok();
            let prompts_git_hash: Option<String> = row.get(13).ok();
            let baseline_logs: Option<String> =
                crate::compression::get_compressed_string_opt(&row, 14).unwrap_or(None);
            let baseline_id: Option<i64> = row.get(15).ok();
            let provider: Option<String> = row.get(16).ok();
            let embargo_until: Option<i64> = row.get(17).ok();
            let mr_url: Option<String> = row.get(18).ok();
            let slug: Option<String> = row.get(19).ok();
            let baseline = if let Some(bid) = baseline_id {
                let mut browse = self
                    .conn
                    .query(
                        "SELECT repo_url, branch, last_known_commit FROM baselines WHERE id = ?",
                        libsql::params![bid],
                    )
                    .await?;
                if let Ok(Some(brow)) = browse.next().await {
                    Some(serde_json::json!({
                       "repo_url": brow.get::<Option<String>>(0).ok(),
                       "branch": brow.get::<Option<String>>(1).ok(),
                       "commit": brow.get::<Option<String>>(2).ok(),
                    }))
                } else {
                    None
                }
            } else {
                None
            };

            let limit_val = limit.unwrap_or(50);
            let page_val = page.unwrap_or(1);
            let offset_val = limit_val * (page_val.saturating_sub(1));

            let mut subsystems = Vec::new();
            let mut sub_rows = self
                .conn
                .query(
                    "SELECT s.name FROM subsystems s
                 JOIN patchsets_subsystems ps ON s.id = ps.subsystem_id
                 WHERE ps.patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            while let Ok(Some(row)) = sub_rows.next().await {
                subsystems.push(row.get::<String>(0)?);
            }

            let mut total_patches = 0;
            let mut count_rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM patches WHERE patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            if let Ok(Some(row)) = count_rows.next().await {
                total_patches = row.get::<i64>(0)?;
            }

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;

            let is_embargoed = if let Some(until) = embargo_until {
                until > now
            } else {
                false
            };

            let mut patches = Vec::new();
            let mut patch_ids = Vec::new();
            let mut patch_rows = self
                .conn
                .query(
                    "SELECT p.id, p.message_id, p.part_index, m.id, m.subject, p.status, p.apply_error, 
                            eo.status as email_status, eo.to_addresses, eo.cc_addresses
                 FROM patches p
                 LEFT JOIN messages m ON p.message_id = m.message_id
                 LEFT JOIN email_outbox eo ON eo.patch_id = p.id
                 WHERE p.patchset_id = ? 
                 ORDER BY p.part_index ASC
                 LIMIT ? OFFSET ?",
                    libsql::params![pid, limit_val, offset_val],
                )
                .await?;

            #[allow(clippy::similar_names)]
            while let Ok(Some(p)) = patch_rows.next().await {
                let p_id: i64 = p.get(0)?;
                patch_ids.push(p_id);
                let mut p_status = p.get::<Option<String>>(5).ok().flatten();
                if is_embargoed && p_status.as_deref() == Some("Reviewed") {
                    p_status = Some("Embargoed".to_string());
                }
                patches.push(serde_json::json!({
                    "id": p_id,
                    "message_id": p.get::<String>(1)?,
                    "part_index": p.get::<Option<i64>>(2).ok(),
                    "msg_db_id": p.get::<Option<i64>>(3).ok(),
                    "subject": p.get::<Option<String>>(4).ok(),
                    "status": p_status,
                    "apply_error": p.get::<Option<String>>(6).ok(),
                    "email_status": p.get::<Option<String>>(7).ok(),
                    "email_to": p.get::<Option<String>>(8).ok(),
                    "email_cc": p.get::<Option<String>>(9).ok(),
                }));
            }

            let mut reviews = Vec::new();
            let mut in_clause = patch_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            if in_clause.is_empty() {
                in_clause = "-1".to_string();
            }
            let query_str = format!(
                "SELECT r.summary, r.created_at, ai.output_raw, 
                        r.result_description, r.status, r.inline_review, ai.tokens_in, ai.tokens_out, r.patch_id, r.id, ai.tokens_cached
                 FROM reviews r
                 LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
                 WHERE r.patchset_id = ? AND (r.patch_id IS NULL OR r.patch_id IN ({}))
                 ORDER BY r.created_at ASC", in_clause);

            let mut params = vec![libsql::Value::Integer(pid)];
            for &pid_val in &patch_ids {
                params.push(libsql::Value::Integer(pid_val));
            }

            let mut rev_rows = self.conn.query(&query_str, params).await?;

            while let Ok(Some(r)) = rev_rows.next().await {
                reviews.push(serde_json::json!({
                    "summary": r.get::<Option<String>>(0).ok(),
                    "created_at": r.get::<Option<i64>>(1).ok(),
                    "output": crate::compression::get_compressed_string_opt(&r, 2).unwrap_or(None),
                    "result": r.get::<Option<String>>(3).ok(),
                    "status": r.get::<Option<String>>(4).ok(),
                    "inline_review": crate::compression::get_compressed_string_opt(&r, 5).unwrap_or(None),
                    "tokens_in": r.get::<Option<u32>>(6).ok(),
                    "tokens_out": r.get::<Option<u32>>(7).ok(),
                    "patch_id": r.get::<Option<i64>>(8).ok(),
                    "id": r.get::<i64>(9).ok(),
                    "tokens_cached": r.get::<Option<u32>>(10).ok(),
                    "model": model_name.clone(),
                    "provider": provider.clone(),
                    "prompts_hash": prompts_git_hash.clone(),
                    "baseline": baseline.clone()
                }));
            }

            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT id, message_id, author, date, subject, in_reply_to FROM messages WHERE thread_id = ? AND subject != '(placeholder)' ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "id": m.get::<i64>(0)?,
                        "message_id": m.get::<String>(1)?,
                        "author": m.get::<Option<String>>(2).ok(),
                        "date": m.get::<Option<i64>>(3).ok(),
                        "subject": m.get::<Option<String>>(4).ok(),
                        "in_reply_to": m.get::<Option<String>>(5).ok(),
                    }));
                }
            }

            let mut final_status = status;
            if is_embargoed && final_status.as_deref() == Some("Reviewed") {
                final_status = Some("Embargoed".to_string());
            }

            let reviews = if is_embargoed { Vec::new() } else { reviews };

            Ok(Some(serde_json::json!({
                "id": pid,
                "message_id": mid,
                "subject": subject,
                "author": author,
                "date": date,
                "status": final_status,
                "failed_reason": failed_reason,
                "to": to,
                "cc": cc,
                "total_parts": total_parts,
                "total_patches_in_db": total_patches,
                "page": page_val,
                "limit": limit_val,
                "received_parts": received_parts,
                "reviews": reviews,
                "patches": patches,
                "thread": messages,
                "mailing_lists": subsystems.clone(),
                "subsystems": subsystems,
                "model_name": model_name,
                "prompts_git_hash": prompts_git_hash,
                "baseline_logs": baseline_logs,
                "baseline": baseline,
                "provider": provider,
                "embargo_until": embargo_until,
                "mr_url": mr_url,
                "slug": slug
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_patchset_summary_by_msgid(
        &self,
        msg_id: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        let candidates = Self::get_msgid_candidates(msg_id);

        for clid in &candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM patchsets WHERE cover_letter_message_id = ? ORDER BY id DESC LIMIT 1",
                    libsql::params![clid.clone()],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0)?;
                return self.get_patchset_summary(id, page, limit).await;
            }
        }

        for clid in &candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT patchset_id FROM patches WHERE message_id = ? ORDER BY id DESC LIMIT 1",
                    libsql::params![clid.clone()],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0)?;
                return self.get_patchset_summary(id, page, limit).await;
            }
        }

        Ok(None)
    }

    pub async fn get_patchset_details_by_slug(
        &self,
        slug: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patchsets WHERE slug = ?",
                libsql::params![slug],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self.get_patchset_details(id, page, limit).await;
        }

        Ok(None)
    }

    pub async fn get_patchset_summary_by_slug(
        &self,
        slug: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patchsets WHERE slug = ?",
                libsql::params![slug],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self.get_patchset_summary(id, page, limit).await;
        }

        Ok(None)
    }

    pub async fn get_review_details(&self, id: i64) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT r.id, r.model, r.summary, r.created_at, ai.input_context, ai.output_raw, 
                        b.repo_url, b.branch, b.last_known_commit,
                        r.provider, r.prompts_hash, r.result_description,
                        r.status, r.inline_review, r.logs, ai.tokens_in, ai.tokens_out, r.patch_id, ai.tokens_cached
             FROM reviews r
             LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
             LEFT JOIN baselines b ON r.baseline_id = b.id
             WHERE r.id = ?",
                libsql::params![id],
            )
            .await?;

        if let Ok(Some(r)) = rows.next().await {
            let bugs = self.list_bugs_for_review(id).await.unwrap_or_default();
            let bugs_json = bugs
                .into_iter()
                .map(|(bug, is_new)| bug_reference_json(&bug, is_new))
                .collect::<Vec<_>>();

            Ok(Some(serde_json::json!({
                "id": r.get::<i64>(0)?,
                "model": r.get::<Option<String>>(1).ok(),
                "summary": r.get::<Option<String>>(2).ok(),
                "created_at": r.get::<Option<i64>>(3).ok(),
                "input": crate::compression::get_compressed_string_opt(&r, 4).unwrap_or(None),
                "output": crate::compression::get_compressed_string_opt(&r, 5).unwrap_or(None),
                "baseline": {
                    "repo_url": r.get::<Option<String>>(6).ok(),
                    "branch": r.get::<Option<String>>(7).ok(),
                    "commit": r.get::<Option<String>>(8).ok(),
                },
                "provider": r.get::<Option<String>>(9).ok(),
                "prompts_hash": r.get::<Option<String>>(10).ok(),
                "result": r.get::<Option<String>>(11).ok(),
                "status": r.get::<Option<String>>(12).ok(),
                "inline_review": crate::compression::get_compressed_string_opt(&r, 13).unwrap_or(None),
                "logs": crate::compression::get_compressed_string_opt(&r, 14).unwrap_or(None),
                "tokens_in": r.get::<Option<u32>>(15).ok(),
                "tokens_out": r.get::<Option<u32>>(16).ok(),
                "patch_id": r.get::<Option<i64>>(17).ok(),
                "tokens_cached": r.get::<Option<u32>>(18).ok(),
                "bugs": bugs_json,
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_latest_review_for_patchset(
        &self,
        patchset_id: i64,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM reviews WHERE patchset_id = ? ORDER BY created_at DESC LIMIT 1",
                libsql::params![patchset_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            self.get_review_details(id).await
        } else {
            Ok(None)
        }
    }

    pub async fn get_patch_diffs(
        &self,
        patchset_id: i64,
    ) -> Result<Vec<(i64, i64, String, String, String, i64, String)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.part_index, p.diff, m.subject, m.author, m.date, m.message_id 
             FROM patches p 
             JOIN messages m ON p.message_id = m.message_id 
             WHERE p.patchset_id = ? 
             ORDER BY p.part_index ASC",
                libsql::params![patchset_id],
            )
            .await?;

        let mut diffs = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let index: i64 = row.get(1).unwrap_or(0);
            let diff: String = crate::compression::get_compressed_string(&row, 2)?;
            let subject: String = row.get(3).unwrap_or_default();
            let author: String = row.get(4).unwrap_or_default();
            let date: i64 = row.get(5).unwrap_or(0);
            let message_id: String = row.get(6)?;
            diffs.push((id, index, diff, subject, author, date, message_id));
        }
        Ok(diffs)
    }

    pub async fn get_patch_by_git_patch_id(
        &self,
        git_patch_id: &str,
    ) -> Result<Option<(String, String, String, String, i64)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.message_id, p.diff, m.subject, m.author, m.date
                 FROM patches p
                 JOIN messages m ON p.message_id = m.message_id
                 WHERE p.git_patch_id = ?
                 ORDER BY m.date DESC
                 LIMIT 1",
                libsql::params![git_patch_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(Some((
                row.get(0)?,
                crate::compression::get_compressed_string(&row, 1)?,
                row.get(2).unwrap_or_default(),
                row.get(3).unwrap_or_default(),
                row.get(4).unwrap_or(0),
            )))
        } else {
            Ok(None)
        }
    }

    pub async fn get_pending_patchsets(&self, limit: usize) -> Result<Vec<PatchsetRow>> {
        let mut rows = self.conn.query(
            "SELECT id, subject, status, thread_id, author, date, cover_letter_message_id, total_parts, received_parts, baseline_id, failed_reason, target_review_count, skip_filters, only_filters, embargo_until, slug
             FROM patchsets WHERE status = 'Pending' ORDER BY date ASC LIMIT ?",
            libsql::params![limit as i64],
        ).await?;

        let mut patchsets = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            patchsets.push(PatchsetRow {
                id: row.get(0).unwrap_or_default(),
                subject: row.get(1).ok(),
                status: row.get(2).ok(),
                thread_id: row.get(3).ok(),
                author: row.get(4).ok(),
                date: row.get(5).ok(),
                message_id: row.get(6).ok(),
                total_parts: row.get(7).ok(),
                received_parts: row.get(8).ok(),
                mailing_lists: Vec::new(),
                subsystems: Vec::new(),
                findings_low: None,
                findings_medium: None,
                findings_high: None,
                findings_critical: None,
                baseline_id: row.get(9).ok(),
                failed_reason: row.get(10).ok(),
                target_review_count: row.get(11).ok(),
                skip_filters: row.get(12).ok(),
                only_filters: row.get(13).ok(),
                model_name: None,
                prompts_git_hash: None,
                baseline_logs: None,
                provider: None,
                embargo_until: row.get(14).ok(),
                mr_url: None,
                mr_title: None,
                mr_number: None,
                slug: row.get(15).ok(),
            });
        }
        Ok(patchsets)
    }

    pub async fn get_releasable_embargoed_patchsets(
        &self,
        now: i64,
        limit: usize,
    ) -> Result<Vec<PatchsetRow>> {
        let sql = format!(
            "SELECT p.id, p.subject, p.status, p.thread_id, p.author, p.date, p.cover_letter_message_id, p.total_parts, p.received_parts, p.baseline_id, p.failed_reason, p.target_review_count, p.skip_filters, p.only_filters, p.embargo_until
             FROM patchsets p
             WHERE p.status = 'Reviewed' AND p.embargo_until IS NOT NULL
             AND (p.embargo_release_started_at IS NULL OR p.embargo_release_started_at <= ?)
             AND (
                 p.embargo_until <= ?
                 OR ({CLEAN_PATCHSET_PREDICATE})
             )
             ORDER BY CASE WHEN p.embargo_until <= ? THEN 0 ELSE 1 END, p.date ASC LIMIT ?"
        );
        let mut rows = self
            .conn
            .query(&sql, libsql::params![now - 600, now, now, limit as i64])
            .await?;

        let mut patchsets = Vec::new();
        loop {
            match rows.next().await {
                Ok(Some(row)) => {
                    patchsets.push(PatchsetRow {
                        id: row.get(0).unwrap_or_default(),
                        subject: row.get(1).ok(),
                        status: row.get(2).ok(),
                        thread_id: row.get(3).ok(),
                        author: row.get(4).ok(),
                        date: row.get(5).ok(),
                        message_id: row.get(6).ok(),
                        total_parts: row.get(7).ok(),
                        received_parts: row.get(8).ok(),
                        mailing_lists: Vec::new(),
                        subsystems: Vec::new(),
                        findings_low: None,
                        findings_medium: None,
                        findings_high: None,
                        findings_critical: None,
                        baseline_id: row.get(9).ok(),
                        failed_reason: row.get(10).ok(),
                        target_review_count: row.get(11).ok(),
                        skip_filters: row.get(12).ok(),
                        only_filters: row.get(13).ok(),
                        model_name: None,
                        prompts_git_hash: None,
                        baseline_logs: None,
                        provider: None,
                        embargo_until: row.get(14).ok(),
                        mr_url: None,
                        mr_title: None,
                        mr_number: None,
                        slug: None,
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("Error fetching row: {:?}", e);
                    break;
                }
            }
        }
        Ok(patchsets)
    }

    pub async fn get_patchset_review_outcome(
        &self,
        patchset_id: i64,
    ) -> Result<PatchsetReviewOutcome> {
        let mut rows = self
            .conn
            .query(
                "SELECT status, COALESCE(target_review_count, 1) FROM patchsets WHERE id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(PatchsetReviewOutcome::Incomplete);
        };
        let status: String = row.get(0).unwrap_or_default();
        let target_review_count: i64 = row.get(1).unwrap_or(1);
        if status != ReviewStatus::Reviewed.as_str() {
            return Ok(PatchsetReviewOutcome::Incomplete);
        }

        let mut no_ai_rows = self
            .conn
            .query(
                "SELECT 1 FROM reviews
                 WHERE patchset_id = ? AND status = 'Skipped'
                   AND result_description = 'Skipped AI review via --no-ai'
                 LIMIT 1",
                libsql::params![patchset_id],
            )
            .await?;
        if no_ai_rows.next().await?.is_some() {
            return Ok(PatchsetReviewOutcome::Incomplete);
        }

        let mut incomplete_rows = self
            .conn
            .query(
                "SELECT 1
                 FROM patches p
                 WHERE p.patchset_id = ?
                   AND COALESCE(p.status, '') != 'Skipped'
                   AND NOT EXISTS (
                       SELECT 1 FROM reviews skipped
                       WHERE skipped.patch_id = p.id
                         AND skipped.status = 'Skipped'
                         AND skipped.result_description = 'Skipped: touches only ignored files'
                   )
                   AND (
                       SELECT COUNT(*) FROM reviews r
                       WHERE r.patch_id = p.id AND r.status = 'Reviewed'
                   ) < ?
                 LIMIT 1",
                libsql::params![patchset_id, target_review_count],
            )
            .await?;
        if incomplete_rows.next().await?.is_some() {
            return Ok(PatchsetReviewOutcome::Incomplete);
        }

        let mut reviewed_rows = self
            .conn
            .query(
                "SELECT 1 FROM reviews WHERE patchset_id = ? AND status = 'Reviewed' LIMIT 1",
                libsql::params![patchset_id],
            )
            .await?;
        if reviewed_rows.next().await?.is_none() {
            return Ok(PatchsetReviewOutcome::Incomplete);
        }

        let mut finding_rows = self
            .conn
            .query(
                "SELECT 1 FROM findings f
                 JOIN reviews r ON r.id = f.review_id
                 WHERE r.patchset_id = ? AND r.status = 'Reviewed'
                 LIMIT 1",
                libsql::params![patchset_id],
            )
            .await?;
        if finding_rows.next().await?.is_some() {
            Ok(PatchsetReviewOutcome::HasFindings)
        } else {
            Ok(PatchsetReviewOutcome::Clean)
        }
    }

    pub async fn claim_patchset_embargo_release(&self, id: i64, now: i64) -> Result<bool> {
        let sql = format!(
            "UPDATE patchsets AS p SET embargo_release_started_at = ?
             WHERE p.id = ? AND p.status = 'Reviewed' AND p.embargo_until IS NOT NULL
               AND (p.embargo_release_started_at IS NULL OR p.embargo_release_started_at <= ?)
               AND (p.embargo_until <= ? OR ({CLEAN_PATCHSET_PREDICATE}))"
        );
        let updated = self
            .conn
            .execute(&sql, libsql::params![now, id, now - 600, now])
            .await?;
        Ok(updated == 1)
    }

    pub async fn clear_patchset_embargo_release_claim(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET embargo_release_started_at = NULL WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_completed_reviews_for_release(
        &self,
        patchset_id: i64,
    ) -> Result<Vec<ReleaseReview>> {
        let mut rows = self
            .conn
            .query(
                "SELECT r.id, r.patch_id, r.inline_review, r.summary, m.message_id, p.part_index
             FROM reviews r
             JOIN patches p ON r.patch_id = p.id
             JOIN messages m ON p.message_id = m.message_id
             WHERE r.patchset_id = ? AND r.status = 'Reviewed'",
                libsql::params![patchset_id],
            )
            .await?;

        let mut temp_reviews = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let review_id: i64 = row.get(0)?;
            let patch_id: i64 = row.get(1)?;
            let inline_review: String = crate::compression::get_compressed_string_opt(&row, 2)
                .unwrap_or(None)
                .unwrap_or_default();
            let summary: String = crate::compression::get_compressed_string_opt(&row, 3)
                .unwrap_or(None)
                .unwrap_or_default();
            let patch_message_id: String = row.get(4).unwrap_or_default();
            let index: i64 = row.get(5).unwrap_or_default();
            temp_reviews.push((
                review_id,
                patch_id,
                inline_review,
                summary,
                patch_message_id,
                index,
            ));
        }

        let mut reviews = Vec::new();
        for (review_id, patch_id, inline_review, summary, patch_message_id, index) in temp_reviews {
            // Fetch findings for this review
            let mut findings_rows = self.conn.query(
                "SELECT severity, problem, severity_explanation, preexisting, locations FROM findings WHERE review_id = ?",
                libsql::params![review_id],
            ).await?;

            let mut findings = Vec::new();
            while let Ok(Some(f_row)) = findings_rows.next().await {
                let severity_int: i64 = f_row.get(0).unwrap_or(1);
                let severity = match severity_int {
                    4 => "Critical",
                    3 => "High",
                    2 => "Medium",
                    _ => "Low",
                }
                .to_string();
                let problem: String = f_row.get(1).unwrap_or_default();
                let severity_explanation: Option<String> = f_row.get(2).ok();
                let int: Option<i64> = f_row.get(3).ok();
                let preexisting = int.map(|val| val != 0);
                let locations_str: Option<String> = f_row.get(4).ok();
                let locations =
                    locations_str.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

                findings.push(json!({
                    "severity": severity,
                    "problem": problem,
                    "severity_explanation": severity_explanation,
                    "preexisting": preexisting,
                    "locations": locations,
                }));
            }

            reviews.push(ReleaseReview {
                id: review_id,
                patch_id,
                patch_message_id,
                index,
                inline_review,
                summary,
                findings,
            });
        }
        Ok(reviews)
    }

    pub async fn update_patchset_status(&self, id: i64, status: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET status = ? WHERE id = ?",
                libsql::params![status, id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_patch_status(&self, patch_id: i64, status: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patches SET status = ? WHERE id = ?",
                libsql::params![status, patch_id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_patchset_status(&self, id: i64) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT status FROM patchsets WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn cancel_patchset(&self, id: i64, force: bool) -> Result<bool> {
        let query = if force {
            "UPDATE patchsets SET status = 'Cancelled' WHERE id = ? AND status IN ('Pending', 'Incomplete', 'In Review')"
        } else {
            "UPDATE patchsets SET status = 'Cancelled' WHERE id = ? AND status IN ('Pending', 'Incomplete')"
        };
        let count = self.conn.execute(query, libsql::params![id]).await?;
        Ok(count > 0)
    }

    pub async fn rerun_patchset(&self, id: i64) -> Result<()> {
        // 1. Get current status of the patchset
        let mut rows = self
            .conn
            .query(
                "SELECT status FROM patchsets WHERE id = ?",
                libsql::params![id],
            )
            .await?;

        let mut current_status = None;
        if let Ok(Some(row)) = rows.next().await {
            let status: String = row.get(0)?;
            current_status = Some(status);
        }

        let should_increment = current_status.as_deref() == Some("Reviewed");

        // 2. Reset patchset status to Pending
        self.conn
            .execute(
                "UPDATE patchsets SET status = 'Pending' WHERE id = ?",
                libsql::params![id],
            )
            .await?;

        // 3. Increment target_review_count only if it was previously Reviewed
        if should_increment {
            self.conn
                .execute(
                    "UPDATE patchsets SET target_review_count = COALESCE(target_review_count, 1) + 1 WHERE id = ?",
                    libsql::params![id],
                )
                .await?;
        }

        // 4. Delete associated tool usages and findings for failed reviews that block retrying
        self.conn
            .execute(
                "DELETE FROM tool_usages WHERE review_id IN (
                    SELECT id FROM reviews WHERE patchset_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL
                )",
                libsql::params![id],
            )
            .await?;

        self.conn
            .execute(
                "DELETE FROM findings WHERE review_id IN (
                    SELECT id FROM reviews WHERE patchset_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL
                )",
                libsql::params![id],
            )
            .await?;

        // 5. Delete failed reviews that block retrying (infra failures)
        self.conn
            .execute(
                "DELETE FROM reviews WHERE patchset_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL",
                libsql::params![id],
            )
            .await?;

        Ok(())
    }

    pub async fn rerun_patch(&self, patchset_id: i64, _patch_id: i64) -> Result<()> {
        // NOTE: Currently we only support re-running the entire patchset to trigger more reviews.
        // Even if the user requested a specific patch, we increment the set's target count
        // to allow the reviewer service to proceed.
        self.rerun_patchset(patchset_id).await
    }

    pub async fn has_patchset_by_msgid(&self, msgid: &str) -> Result<bool> {
        let candidates = Self::get_msgid_candidates(msgid);
        for clid in &candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT 1 FROM patchsets WHERE cover_letter_message_id = ? AND status NOT IN ('Failed', 'Cancelled', 'Failed To Apply', 'FailedToApply') LIMIT 1",
                    libsql::params![clid.clone()],
                )
                .await?;
            if rows.next().await.ok().flatten().is_some() {
                return Ok(true);
            }

            let mut p_rows = self
                .conn
                .query(
                    "SELECT 1 FROM patches p JOIN patchsets ps ON p.patchset_id = ps.id WHERE p.message_id = ? AND ps.status NOT IN ('Failed', 'Cancelled', 'Failed To Apply', 'FailedToApply') LIMIT 1",
                    libsql::params![clid.clone()],
                )
                .await?;
            if p_rows.next().await.ok().flatten().is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_fetching_patchset(
        &self,
        root_msg_id: &str,
        subject: &str,
        skip_filters: Option<&Vec<String>>,
        only_filters: Option<&Vec<String>>,
        mr_url: Option<&str>,
        mr_title: Option<&str>,
        mr_number: Option<i64>,
        slug: Option<&str>,
    ) -> Result<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;

        let clid_candidates = Self::get_msgid_candidates(root_msg_id);

        let skip_filters_json = skip_filters.map(|f| serde_json::to_string(f).unwrap_or_default());
        let only_filters_json = only_filters.map(|f| serde_json::to_string(f).unwrap_or_default());

        // 1. Check if it already exists
        for clid in clid_candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT id, status FROM patchsets WHERE cover_letter_message_id = ?",
                    libsql::params![clid.clone()],
                )
                .await?;

            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0)?;
                let status: String = row.get(1).unwrap_or_default();

                // Only reset to Fetching if it failed or is currently fetching.
                // We don't want to reset if it is already Incomplete, Pending, or Reviewed.
                if status == "Failed"
                    || status == "Fetching"
                    || status == "Cancelled"
                    || status == "Failed To Apply"
                    || status == "FailedToApply"
                {
                    self.conn.execute(
                        "UPDATE patchsets SET status = 'Fetching', failed_reason = NULL, skip_filters = ?, only_filters = ?, mr_url = ?, mr_title = ?, mr_number = ?, slug = ? WHERE id = ?",
                        libsql::params![skip_filters_json.clone(), only_filters_json.clone(), mr_url, mr_title, mr_number, slug, id]
                    ).await?;
                }
                return Ok(id);
            }
        }

        // 2. Ensure a placeholder thread and message exist to satisfy Foreign Key constraints
        let thread_id = self.ensure_thread_for_message(root_msg_id, now).await?;

        // 3. Create the fetching patchset
        let mut rows = self.conn
            .query(
                "INSERT INTO patchsets (thread_id, cover_letter_message_id, subject, status, date, skip_filters, only_filters, mr_url, mr_title, mr_number, slug)
                     VALUES (?, ?, ?, 'Fetching', ?, ?, ?, ?, ?, ?, ?) RETURNING id",
                libsql::params![thread_id, root_msg_id, subject, now, skip_filters_json, only_filters_json, mr_url, mr_title, mr_number, slug],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get patchset ID"))
        }
    }
    pub async fn update_patchset_error(&self, root_msg_id: &str, error: &str) -> Result<()> {
        let candidates = Self::get_msgid_candidates(root_msg_id);
        for clid in candidates {
            let res = self
                .conn
                .execute(
                    "UPDATE patchsets SET status = 'Failed', failed_reason = ? WHERE cover_letter_message_id = ?",
                    libsql::params![error, clid],
                )
                .await?;
            if res > 0 {
                return Ok(());
            }
        }
        Ok(())
    }

    pub async fn update_patchset_baseline_info(
        &self,
        id: i64,
        baseline_id: Option<i64>,
        model_name: Option<&str>,
        prompts_hash: Option<&str>,
        logs: Option<&str>,
        provider: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET baseline_id = ?, model_name = ?, prompts_git_hash = ?, baseline_logs = ?, provider = ? WHERE id = ?",
                libsql::params![baseline_id, model_name, prompts_hash, logs.map(crate::compression::compress_string_if_needed).unwrap_or(libsql::Value::Null), provider, id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_patch_application_status(
        &self,
        patchset_id: i64,
        part_index: i64,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE patches SET status = ?, apply_error = ? WHERE patchset_id = ? AND part_index = ?",
            libsql::params![status, error, patchset_id, part_index],
        ).await?;
        Ok(())
    }

    pub async fn reset_reviewing_status(&self) -> Result<u64> {
        let status_pending = ReviewStatus::Pending.as_str();
        // Reset Patchsets
        let count_ps = self
            .conn
            .execute(
                format!(
                    "UPDATE patchsets SET status = '{}' WHERE status IN ('In Review', 'Reviewing')",
                    status_pending
                )
                .as_str(),
                (),
            )
            .await?;

        // Reset Reviews
        let count_rev = self
            .conn
            .execute(
                format!(
                    "UPDATE reviews SET status = '{}' WHERE status = 'In Review'",
                    status_pending
                )
                .as_str(),
                (),
            )
            .await?;

        Ok(count_ps + count_rev)
    }

    pub async fn get_patchset_counts_by_status(
        &self,
    ) -> Result<std::collections::HashMap<String, usize>> {
        let mut rows = self
            .conn
            .query("SELECT status, COUNT(*) FROM patchsets GROUP BY status", ())
            .await?;

        let mut counts = std::collections::HashMap::new();
        while let Ok(Some(row)) = rows.next().await {
            let status: Option<String> = row.get(0).ok();
            let count: i64 = row.get(1)?;
            let status_key = status.unwrap_or_else(|| "Unknown".to_string());
            counts.insert(status_key, count as usize);
        }
        Ok(counts)
    }
}

impl Database {
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_email_outbox(
        &self,
        patch_id: i64,
        status: &str,
        to_addresses: &str,
        cc_addresses: &str,
        subject: &str,
        in_reply_to: &str,
        references_hdr: &str,
        body: &str,
    ) -> Result<()> {
        // Prevent duplicate emails for the same patch
        let mut rows = self
            .conn
            .query(
                "SELECT 1 FROM email_outbox WHERE patch_id = ?",
                libsql::params![patch_id],
            )
            .await?;

        if let Ok(Some(_)) = rows.next().await {
            tracing::info!(
                "Email outbox entry already exists for patch_id {}, skipping to prevent duplicates.",
                patch_id
            );
            return Ok(());
        }

        let created_at = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO email_outbox (patch_id, status, to_addresses, cc_addresses, subject, in_reply_to, references_hdr, body, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                patch_id,
                status,
                to_addresses,
                cc_addresses,
                subject,
                in_reply_to,
                references_hdr,
                body,
                created_at,
            ],
        ).await?;
        Ok(())
    }

    pub async fn lock_pending_email(&self) -> Result<Option<EmailOutboxRow>> {
        let now = chrono::Utc::now().timestamp();
        let mut rows = self.conn.query(
            "UPDATE email_outbox 
             SET status = 'Sending', locked_at = ? 
             WHERE id = (SELECT id FROM email_outbox WHERE status = 'Pending' LIMIT 1)
             RETURNING id, patch_id, kind, status, to_addresses, cc_addresses, subject, in_reply_to, references_hdr, body, locked_at, error_log, created_at",
            libsql::params![now]
        ).await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let patch_id: Option<i64> = row.get::<i64>(1).ok();
            let kind = EmailKind::from_stored(&row.get::<String>(2)?);
            let status: String = row.get(3)?;
            let to_addresses: String = row.get(4)?;
            let cc_addresses: String = row.get(5)?;
            let subject: String = row.get(6)?;
            let in_reply_to: String = row.get(7)?;
            let references_hdr: String = row.get(8)?;
            let body: String = row.get(9)?;
            let locked_at: Option<i64> = row.get(10).ok();
            let error_log: Option<String> = row.get(11).ok();
            let created_at: i64 = row.get(12)?;

            Ok(Some(EmailOutboxRow {
                id,
                patch_id,
                kind,
                status,
                to_addresses,
                cc_addresses,
                subject,
                in_reply_to,
                references_hdr,
                body,
                locked_at,
                error_log,
                created_at,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn mark_email_sent(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE email_outbox SET status = 'Sent', locked_at = NULL WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(())
    }

    pub async fn mark_email_failed(&self, id: i64, error_log: &str) -> Result<()> {
        self.conn.execute("UPDATE email_outbox SET status = 'Failed', error_log = ?, locked_at = NULL WHERE id = ?", libsql::params![error_log.to_string(), id]).await?;
        Ok(())
    }

    pub async fn sweep_ghost_emails(&self) -> Result<u64> {
        let ten_mins_ago = chrono::Utc::now().timestamp() - 600;
        let count = self.conn.execute(
            "UPDATE email_outbox SET status = 'Pending', locked_at = NULL WHERE status = 'Sending' AND locked_at < ?",
            libsql::params![ten_mins_ago]
        ).await?;
        Ok(count)
    }

    // -- Patchwork outbox operations --

    pub async fn insert_patchwork_outbox(
        &self,
        patch_msg_id: &str,
        api_url: &str,
        check_state: &str,
        description: &str,
        target_url: &str,
        context: &str,
    ) -> Result<()> {
        let mut rows = self
            .conn
            .query(
                "SELECT 1 FROM patchwork_outbox
                 WHERE patch_msg_id = ? AND api_url = ? AND context = ?",
                libsql::params![patch_msg_id, api_url, context],
            )
            .await?;
        if rows.next().await?.is_some() {
            return Ok(());
        }

        let created_at = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT INTO patchwork_outbox (patch_msg_id, api_url, check_state, description, target_url, context, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                libsql::params![
                    patch_msg_id,
                    api_url,
                    check_state,
                    description,
                    target_url,
                    context,
                    created_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn lock_pending_patchwork(&self) -> Result<Option<PatchworkOutboxRow>> {
        let now = chrono::Utc::now().timestamp();
        let mut rows = self
            .conn
            .query(
                "UPDATE patchwork_outbox
                 SET status = 'Sending', locked_at = ?
                 WHERE id = (
                     SELECT id FROM patchwork_outbox
                     WHERE status = 'Pending'
                       AND (next_retry_at IS NULL OR next_retry_at <= ?)
                     LIMIT 1
                 )
                 RETURNING id, patch_msg_id, api_url, check_state, description, target_url, context, status, retry_count, next_retry_at, locked_at, error_log, created_at",
                libsql::params![now, now],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let patch_msg_id: String = row.get(1)?;
            let api_url: String = row.get(2)?;
            let check_state: String = row.get(3)?;
            let description: String = row.get(4)?;
            let target_url: String = row.get(5)?;
            let context: String = row.get(6)?;
            let status: String = row.get(7)?;
            let retry_count: i64 = row.get(8)?;
            let next_retry_at: Option<i64> = row.get::<i64>(9).ok();
            let locked_at: Option<i64> = row.get::<i64>(10).ok();
            let error_log: Option<String> = row.get::<String>(11).ok();
            let created_at: i64 = row.get(12)?;

            Ok(Some(PatchworkOutboxRow {
                id,
                patch_msg_id,
                api_url,
                check_state,
                description,
                target_url,
                context,
                status,
                retry_count,
                next_retry_at,
                locked_at,
                error_log,
                created_at,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn mark_patchwork_sent(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchwork_outbox SET status = 'Sent', locked_at = NULL WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(())
    }

    pub async fn mark_patchwork_failed(&self, id: i64, error_log: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchwork_outbox SET status = 'Failed', error_log = ?, locked_at = NULL WHERE id = ?",
                libsql::params![error_log.to_string(), id],
            )
            .await?;
        Ok(())
    }

    /// Mark a patchwork outbox entry for retry at a future timestamp.
    /// Increments retry_count, sets next_retry_at, and returns to
    /// Pending status so the worker loop continues without blocking.
    pub async fn set_patchwork_retry_at(&self, id: i64, next_retry_at: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchwork_outbox SET status = 'Pending', retry_count = retry_count + 1, next_retry_at = ?, locked_at = NULL WHERE id = ?",
                libsql::params![next_retry_at, id],
            )
            .await?;
        Ok(())
    }

    pub async fn sweep_ghost_patchwork(&self) -> Result<u64> {
        let ten_mins_ago = chrono::Utc::now().timestamp() - 600;
        let count = self.conn
            .execute(
                "UPDATE patchwork_outbox SET status = 'Pending', locked_at = NULL WHERE status = 'Sending' AND locked_at < ?",
                libsql::params![ten_mins_ago],
            )
            .await?;
        Ok(count)
    }

    /// Insert a patchwork notification email into the email outbox.
    ///
    /// Uses patch_id = NULL to avoid colliding with the per-patch dedup
    /// guard in insert_email_outbox(). The EmailWorker processes these
    /// rows normally since it picks up any row with status = 'Pending'.
    pub async fn insert_patchwork_notification(
        &self,
        status: &str,
        to_address: &str,
        subject: &str,
        in_reply_to: &str,
        references_hdr: &str,
        body: &str,
    ) -> Result<()> {
        let mut rows = self
            .conn
            .query(
                "SELECT 1 FROM email_outbox
                 WHERE patch_id IS NULL AND to_addresses = ? AND subject = ? AND in_reply_to = ?",
                libsql::params![
                    serde_json::to_string(&[to_address])
                        .map_err(|e| libsql::Error::Misuse(e.to_string()))?,
                    subject,
                    in_reply_to
                ],
            )
            .await?;
        if rows.next().await?.is_some() {
            return Ok(());
        }

        let created_at = chrono::Utc::now().timestamp();
        let to_json = serde_json::to_string(&[to_address])
            .map_err(|e| libsql::Error::Misuse(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO email_outbox (patch_id, status, to_addresses, cc_addresses, subject, in_reply_to, references_hdr, body, created_at)
                 VALUES (NULL, ?, ?, '[]', ?, ?, ?, ?, ?)",
                libsql::params![
                    status,
                    to_json,
                    subject,
                    in_reply_to,
                    references_hdr,
                    body,
                    created_at,
                ],
            )
            .await?;
        Ok(())
    }

    /// Queue a message addressed to a person rather than to a patch.
    ///
    /// There is deliberately no dedup guard. Two sign-in requests are two
    /// distinct messages, and suppressing the second would look to the
    /// recipient exactly like the feature being broken. Volume is bounded at
    /// the request endpoint instead.
    ///
    /// The caller supplies the status so that a dry-run deployment can park
    /// the row where the poller will not pick it up, which is how review mail
    /// already behaves; inheriting only the worker's dry-run check would leave
    /// rows sitting Pending forever.
    pub async fn insert_transactional_email(
        &self,
        kind: EmailKind,
        status: &str,
        to_address: &str,
        subject: &str,
        body: &str,
    ) -> Result<()> {
        let created_at = chrono::Utc::now().timestamp();
        let to_json = serde_json::to_string(&[to_address])
            .map_err(|e| libsql::Error::Misuse(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO email_outbox (patch_id, kind, status, to_addresses, cc_addresses, subject, in_reply_to, references_hdr, body, created_at)
                 VALUES (NULL, ?, ?, ?, '[]', ?, '', '', ?, ?)",
                libsql::params![
                    kind.as_str(),
                    status,
                    to_json,
                    subject,
                    body,
                    created_at,
                ],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// The bug schema ships as a single migration, so a fresh database reaches
    /// the final layout in one step. This pins that migrate is idempotent, that
    /// it leaves shared infrastructure tables alone, and that the resulting
    /// schema is immediately usable.
    #[tokio::test]
    async fn test_bug_schema_migrates_and_is_usable() -> Result<()> {
        let db = Database::new(&DatabaseSettings {
            url: ":memory:".into(),
            token: String::new(),
        })
        .await?;

        db.migrate().await?;
        // Re-running must be a no-op rather than recreating anything.
        db.migrate().await?;

        db.conn
            .execute_batch(
                "INSERT INTO people (name, email) VALUES ('Shared Row', 'shared@example.org');",
            )
            .await?;
        assert_eq!(
            db.conn
                .query("SELECT COUNT(*) FROM people", ())
                .await?
                .next()
                .await?
                .unwrap()
                .get::<i64>(0)?,
            1,
            "the bug migration must not touch shared infrastructure tables"
        );

        // The schema starts bugs in the untriaged, unanalysed state
        // and still records attributed audit history.
        let id = db
            .create_bug(&NewBug {
                bugid: "linux-v2".to_string(),
                title: "Fresh report".to_string(),
                lifecycle_status: BugLifecycleStatus::New,
                pipeline_state: BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 1,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                source_ref: None,
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![],
            })
            .await?;
        let scoped = db.with_bug_actor("new author", "web", None);
        scoped
            .change_bug_status_with_reason(id, BugLifecycleStatus::Closed, Some("Now fixed"))
            .await?;
        let bug = db.get_bug(id).await?.unwrap();
        assert_eq!(bug.lifecycle_status, BugLifecycleStatus::Closed);
        assert!(
            bug.enrichments
                .iter()
                .any(|e| e.kind == "audit" && e.author.as_deref() == Some("new author"))
        );
        Ok(())
    }

    /// Sign-in mail is addressed to a person rather than a patch, so it must
    /// escape the per-patch dedup guard: a second request is a second message,
    /// and swallowing it would look exactly like the feature being broken.
    #[tokio::test]
    async fn test_transactional_email_escapes_patch_dedup() -> Result<()> {
        let db = setup_db().await;

        for _ in 0..2 {
            db.insert_transactional_email(
                EmailKind::SignInLink,
                "Pending",
                "maintainer@example.org",
                "[sashiko] Your sign-in link",
                "body",
            )
            .await?;
        }

        let first = db
            .lock_pending_email()
            .await?
            .expect("first message queued");
        assert_eq!(first.kind, EmailKind::SignInLink);
        assert_eq!(first.patch_id, None);
        assert_eq!(first.to_addresses, r#"["maintainer@example.org"]"#);
        db.mark_email_sent(first.id).await?;

        let second = db
            .lock_pending_email()
            .await?
            .expect("second message queued");
        assert_eq!(second.kind, EmailKind::SignInLink);
        db.mark_email_sent(second.id).await?;

        // A dry-run deployment parks the row in a status the poller never
        // selects, rather than relying on the worker to drop it on the floor.
        db.insert_transactional_email(
            EmailKind::SignInLink,
            "Dry-Run",
            "maintainer@example.org",
            "[sashiko] Your sign-in link",
            "body",
        )
        .await?;
        assert!(db.lock_pending_email().await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_bug_discovery_family_and_attributed_audit() -> Result<()> {
        let db = setup_db().await;
        let mut ids = Vec::new();
        for (slug, tool, model) in [
            ("canonical", "reviewer-a", "model-a"),
            ("rediscovered", "reviewer-b", "model-b"),
            ("again", "reviewer-a", "model-a"),
        ] {
            let scoped = db.with_bug_actor("automation", tool, Some(model.into()));
            let id = scoped
                .create_bug(&serde_json::from_value(
                    json!({ "bugid": slug, "title": "net: missing check" }),
                )?)
                .await?;
            scoped
                .add_bug_enrichment(
                    id,
                    &NewBugEnrichment {
                        kind: "candidate".into(),
                        data_json: Some(json!({"raw": "original payload"})),
                        logs: Some("unstructured original log".into()),
                        ..Default::default()
                    },
                )
                .await?;
            ids.push(id);
        }
        let worker = db.with_bug_actor("automation", "bug-worker", Some("analysis-model".into()));
        worker
            .update_bug_outcome(
                ids[0],
                UpdateBugOutcomeParams {
                    lifecycle_status: BugLifecycleStatus::Dismissed,
                    verified_on_sha: Some("abcdef"),
                    logs: Some("[{\"role\":\"model\",\"parts\":[{\"text\":\"refuted\"}]}]"),
                    ..Default::default()
                },
            )
            .await?;
        assert!(db.get_bug_logs(ids[0]).await?.unwrap().contains("refuted"));
        // Multiple enrichment stages must not inflate the number of discoveries.
        assert_eq!(
            db.bug_evidence(&db.bug_family(ids[0], false).await?)
                .await?["count"],
            1
        );
        let human = db.with_bug_actor("maintainer@example.org", "web", None);
        human
            .change_bug_status_with_reason(
                ids[0],
                BugLifecycleStatus::Closed,
                Some("Confirmed fixed upstream"),
            )
            .await?;
        // Merge a child into another discovery, then merge that parent.
        human
            .mark_bug_as_duplicate(MarkDuplicateBugParams {
                ephemeral_id: ids[2],
                canonical_id: ids[1],
                reasoning: "Same cause",
                ..Default::default()
            })
            .await?;
        human
            .mark_bug_as_duplicate(MarkDuplicateBugParams {
                ephemeral_id: ids[1],
                canonical_id: ids[0],
                reasoning: "Same cause",
                ..Default::default()
            })
            .await?;
        let evidence = db
            .bug_evidence(&db.bug_family(ids[1], false).await?)
            .await?;
        assert_eq!(evidence["count"], 3);
        assert_eq!(evidence["models"], json!(["model-a", "model-b"]));
        assert_eq!(evidence["tools"], json!(["reviewer-a", "reviewer-b"]));
        let summaries = db.bug_discovery_summaries(&ids).await?;
        for id in &ids {
            assert_eq!(summaries[id]["count"], evidence["count"]);
            assert_eq!(summaries[id]["models"], evidence["models"]);
            assert_eq!(summaries[id]["tools"], evidence["tools"]);
        }
        let events = evidence["activity"].as_array().unwrap();
        assert!(
            events
                .iter()
                .all(|e| e.get("data_json").is_none() && e.get("logs").is_none())
        );
        let comment = events
            .iter()
            .find(|e| e["content"] == "Confirmed fixed upstream")
            .unwrap();
        assert_eq!(comment["author"], "maintainer@example.org");
        assert_eq!(comment["tool"], "web");
        assert!(comment["model"].is_null());
        let bug = db.get_bug(ids[0]).await?.unwrap();
        let closed = bug
            .enrichments
            .iter()
            .find(|e| {
                e.data_json
                    .as_ref()
                    .is_some_and(|d| d["field"] == "lifecycle_status" && d["new"] == "closed")
            })
            .unwrap();
        assert_eq!(closed.author.as_deref(), Some("maintainer@example.org"));
        assert_eq!(closed.tool, "web");
        assert_eq!(closed.model, None);
        let raw = db.bug_family(ids[0], true).await?;
        assert_eq!(raw.len(), 3);
        assert!(raw.iter().all(|b| {
            b.enrichments
                .iter()
                .any(|e| e.logs.as_deref() == Some("unstructured original log"))
        }));
        assert!(
            human
                .mark_bug_as_duplicate(MarkDuplicateBugParams {
                    ephemeral_id: ids[0],
                    canonical_id: ids[1],
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(
            human
                .mark_bug_as_duplicate(MarkDuplicateBugParams {
                    ephemeral_id: ids[0],
                    canonical_id: 99999,
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert_eq!(
            db.get_bug(ids[0]).await?.unwrap().lifecycle_status,
            BugLifecycleStatus::Closed
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_bug_attribution_scopes_and_subsystems() -> Result<()> {
        let db = setup_db().await;
        let alice = db.with_bug_actor("alice", "web", None);
        let bot = db.with_bug_actor("bot", "external-tool", Some("external-model".into()));
        let id = alice
            .create_bug(&serde_json::from_value(
                json!({"bugid": "scoped", "title": "test"}),
            )?)
            .await?;
        bot.update_bug_subsystems(id, &[AttributedSubsystem::from_maintainers("net")])
            .await?;
        alice.update_bug_title(id, "Changed by Alice").await?;
        bot.update_bug_vector(id, "{}").await?;
        let bug = db.get_bug(id).await?.unwrap();
        let title = bug
            .enrichments
            .iter()
            .find(|e| e.data_json.as_ref().is_some_and(|d| d["field"] == "title"))
            .unwrap();
        assert_eq!(title.author.as_deref(), Some("alice"));
        assert_eq!(title.model, None);
        let subsystem = bug
            .enrichments
            .iter()
            .find(|e| {
                e.data_json
                    .as_ref()
                    .is_some_and(|d| d["action"] == "subsystem_added")
            })
            .unwrap();
        assert_eq!(subsystem.author.as_deref(), Some("bot"));
        assert_eq!(subsystem.model.as_deref(), Some("external-model"));
        let count = bug.enrichments.len();
        bot.update_bug_subsystems(id, &[AttributedSubsystem::from_maintainers("net")])
            .await?;
        assert_eq!(db.get_bug(id).await?.unwrap().enrichments.len(), count);
        // A legacy report has no invented model attribution.
        let evidence = db.bug_evidence(&db.bug_family(id, false).await?).await?;
        assert_eq!(evidence["count"], 1);
        assert_eq!(evidence["unknown_models"], 1);
        assert_eq!(evidence["models"], json!([]));
        Ok(())
    }

    /// Reads the stored provenance for every subsystem attached to a bug.
    async fn stored_subsystem_sources(db: &Database, bug_id: i64) -> Result<Vec<(String, String)>> {
        let mut rows = db
            .conn
            .query(
                "SELECT subsystem, source FROM bug_subsystems WHERE bug_id = ? ORDER BY subsystem",
                libsql::params![bug_id],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((row.get(0)?, row.get(1)?));
        }
        Ok(out)
    }

    #[tokio::test]
    async fn test_bug_subsystem_provenance_is_recorded_and_refreshed() -> Result<()> {
        let db = setup_db().await;
        let mut bug: NewBug =
            serde_json::from_value(json!({"bugid": "provenance", "title": "test"}))?;
        bug.subsystems = vec![
            AttributedSubsystem::from_maintainers("NETWORKING [IPv4/IPv6]"),
            AttributedSubsystem::from_path_prefix("net/ipv4"),
            AttributedSubsystem::new("whatever the caller said", SubsystemSource::default()),
        ];
        let id = db.create_bug(&bug).await?;

        assert_eq!(
            stored_subsystem_sources(&db, id).await?,
            vec![
                (
                    "NETWORKING [IPv4/IPv6]".to_string(),
                    "maintainers_section".to_string()
                ),
                ("net/ipv4".to_string(), "path_prefix".to_string()),
                (
                    "whatever the caller said".to_string(),
                    "caller_supplied".to_string()
                ),
            ]
        );

        // A later run that matches the same name out of MAINTAINERS has to
        // upgrade the provenance rather than leave the stale value behind.
        db.update_bug_subsystems(id, &[AttributedSubsystem::from_maintainers("net/ipv4")])
            .await?;
        assert_eq!(
            stored_subsystem_sources(&db, id).await?,
            vec![("net/ipv4".to_string(), "maintainers_section".to_string())]
        );

        let reattributed = db
            .get_bug(id)
            .await?
            .unwrap()
            .enrichments
            .iter()
            .filter(|e| {
                e.data_json
                    .as_ref()
                    .is_some_and(|d| d["action"] == "subsystem_reattributed")
            })
            .count();
        assert_eq!(reattributed, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_bug_visibility_scopes_listings_to_maintained_sections() -> Result<()> {
        let db = setup_db().await;
        let mut btrfs: NewBug = serde_json::from_value(json!({"bugid": "b1", "title": "t"}))?;
        btrfs.subsystems = vec![AttributedSubsystem::from_maintainers("BTRFS FILE SYSTEM")];
        let btrfs = db.create_bug(&btrfs).await?;

        let mut net: NewBug = serde_json::from_value(json!({"bugid": "b2", "title": "t"}))?;
        net.subsystems = vec![AttributedSubsystem::from_maintainers("NETWORKING DRIVERS")];
        let net = db.create_bug(&net).await?;

        // Attributed by path and by the reporter, so it names no maintainer.
        let mut unclaimed: NewBug = serde_json::from_value(json!({"bugid": "b3", "title": "t"}))?;
        unclaimed.subsystems = vec![
            AttributedSubsystem::from_path_prefix("drivers/misc"),
            AttributedSubsystem::new("btrfs file system", SubsystemSource::CallerSupplied),
        ];
        let unclaimed = db.create_bug(&unclaimed).await?;

        assert_eq!(
            db.authorizing_sections_for_bug(btrfs).await?,
            vec!["BTRFS FILE SYSTEM".to_string()]
        );
        assert!(db.authorizing_sections_for_bug(unclaimed).await?.is_empty());

        let batch = db
            .authorizing_sections_for_bugs(&[btrfs, net, unclaimed])
            .await?;
        assert_eq!(batch.len(), 2);
        assert!(!batch.contains_key(&unclaimed));

        // The scope matches the stored title regardless of case, and a name the
        // reporter invented never brings a bug into scope.
        let scope = vec!["btrfs file system".to_string()];
        let (items, total) = db
            .list_bugs(ListBugsParams {
                visibility: BugVisibility::Sections(&scope),
                ..Default::default()
            })
            .await?;
        assert_eq!(total, 1);
        assert_eq!(items[0].id, btrfs);

        // The default is the empty scope, so a caller who says nothing sees
        // nothing.
        let (items, total) = db.list_bugs(ListBugsParams::default()).await?;
        assert_eq!(total, 0);
        assert!(items.is_empty());

        let (_, total) = db
            .list_bugs(ListBugsParams {
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await?;
        assert_eq!(total, 3);

        // Facet counts follow the same scope, so they cannot be used to infer
        // that a bug exists in someone else's subsystem.
        let counts = db
            .get_subsystems_bug_counts(
                Some(BugLifecycleStatus::New),
                BugVisibility::Sections(&scope),
            )
            .await?;
        assert_eq!(counts, vec![("BTRFS FILE SYSTEM".to_string(), 1)]);

        let counts = db
            .get_subsystems_bug_counts(Some(BugLifecycleStatus::New), BugVisibility::default())
            .await?;
        assert!(counts.is_empty());

        let counts = db
            .get_subsystems_bug_counts(Some(BugLifecycleStatus::New), BugVisibility::Unrestricted)
            .await?;
        assert_eq!(counts.len(), 4);
        Ok(())
    }

    #[test]
    fn test_bare_subsystem_name_deserializes_as_caller_supplied() {
        let bug: NewBug = serde_json::from_value(json!({
            "bugid": "b",
            "title": "t",
            "subsystems": ["net", {"name": "fs", "source": "maintainers_section"}, {"name": "mm"}],
        }))
        .unwrap();
        assert_eq!(
            bug.subsystems,
            vec![
                AttributedSubsystem::new("net", SubsystemSource::CallerSupplied),
                AttributedSubsystem::from_maintainers("fs"),
                AttributedSubsystem::new("mm", SubsystemSource::CallerSupplied),
            ]
        );
    }

    #[tokio::test]
    async fn test_bug_model_resolution_from_review() -> Result<()> {
        let db = setup_db().await;
        let thread_id = db.create_thread("t1", "subj", 100).await?;
        let ps_id = db
            .create_patchset(
                thread_id, None, "m1", "subj", "auth", 100, 1, 0, "", "", None, 1, None, false,
                None, None,
            )
            .await?
            .unwrap();
        let rev_id = db
            .create_review(ps_id, None, "gemini", "test-model", None, None)
            .await?;

        let bug = NewBug {
            bugid: "linux-model-test".to_string(),
            title: "Model test bug".to_string(),
            lifecycle_status: BugLifecycleStatus::Open,
            pipeline_state: BugPipelineState::Succeeded,
            assignee: None,
            reporter: "sashiko".to_string(),
            reported_at: 1000,
            discovered_in_patchset_id: Some(ps_id),
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
        };
        let bug_id = db
            .create_bug_with_enrichment(
                &bug,
                Some(&NewBugEnrichment {
                    kind: "candidate".to_string(),
                    tool: "sashiko:linux_patch_review".to_string(),
                    model: None,
                    created_at: 1000,
                    content: Some("test".to_string()),
                    ..Default::default()
                }),
            )
            .await?;
        db.link_review_to_bug(rev_id, bug_id, true).await?;

        // 1. bug_discovery_summaries and bug_evidence resolve model from linked review
        let summaries = db.bug_discovery_summaries(&[bug_id]).await?;
        let summary = &summaries[&bug_id];
        assert_eq!(summary["count"], 1);
        assert_eq!(summary["models"], json!(["test-model"]));
        assert_eq!(summary["tools"], json!(["sashiko:linux_patch_review"]));
        assert_eq!(summary["unknown_models"], 0);

        let evidence = db
            .bug_evidence(&db.bug_family(bug_id, false).await?)
            .await?;
        assert_eq!(evidence["count"], 1);
        assert_eq!(evidence["models"], json!(["test-model"]));
        assert_eq!(evidence["unknown_models"], 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_bug_discoveries_record_patch_and_patchset_and_multiple_occurrences() -> Result<()>
    {
        let db = setup_db().await;
        let thread_id = db.create_thread("t1", "subj", 100).await?;
        db.create_message(
            "m1", thread_id, None, "auth", "subj 1", 100, "", "", "", None, None,
        )
        .await?;
        db.create_message(
            "patch1_m1",
            thread_id,
            Some("m1"),
            "auth",
            "patch 1",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await?;
        db.create_message(
            "patch1_m2",
            thread_id,
            Some("m1"),
            "auth",
            "patch 2",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await?;

        // Patchset 1 with 2 patches
        let ps1_id = db
            .create_patchset(
                thread_id, None, "m1", "subj 1", "auth", 100, 2, 0, "", "", None, 1, None, false,
                None, None,
            )
            .await?
            .unwrap();
        let p1_id = db.create_patch(ps1_id, "patch1_m1", 1, "diff1").await?;
        let p2_id = db.create_patch(ps1_id, "patch1_m2", 2, "diff2").await?;

        db.create_message(
            "m2", thread_id, None, "auth", "subj 2", 200, "", "", "", None, None,
        )
        .await?;
        db.create_message(
            "patch2_m1",
            thread_id,
            Some("m2"),
            "auth",
            "patch 1",
            200,
            "",
            "",
            "",
            None,
            None,
        )
        .await?;

        // Patchset 2 with 1 patch
        let ps2_id = db
            .create_patchset(
                thread_id, None, "m2", "subj 2", "auth", 200, 1, 0, "", "", None, 1, None, false,
                None, None,
            )
            .await?
            .unwrap();
        let p3_id = db.create_patch(ps2_id, "patch2_m1", 1, "diff3").await?;

        // 1. Initial bug discovered while reviewing patch 1 of patchset 1
        let bug1 = NewBug {
            bugid: "linux-bug-multi-1".to_string(),
            title: "Preexisting bug".to_string(),
            lifecycle_status: BugLifecycleStatus::Open,
            pipeline_state: BugPipelineState::Succeeded,
            assignee: None,
            reporter: "sashiko.dev".to_string(),
            reported_at: 1000,
            discovered_in_patchset_id: Some(ps1_id),
            discovered_in_patch_id: Some(p1_id),
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
        };
        let bug1_id = db
            .create_bug_with_enrichment(
                &bug1,
                Some(&NewBugEnrichment {
                    kind: "candidate".to_string(),
                    tool: "sashiko:linux_patch_review".to_string(),
                    model: Some("gemini-1.5-pro".to_string()),
                    author: Some("sashiko.dev".to_string()),
                    created_at: 1000,
                    content: Some("First discovery".to_string()),
                    data_json: Some(json!({
                        "patchset_id": ps1_id,
                        "patch_id": p1_id,
                    })),
                    ..Default::default()
                }),
            )
            .await?;

        // 2. Same bug discovered while reviewing patch 2 of patchset 1 -> duplicate of bug 1
        let bug2 = NewBug {
            bugid: "linux-bug-multi-2".to_string(),
            title: "Preexisting bug copy 2".to_string(),
            lifecycle_status: BugLifecycleStatus::New,
            pipeline_state: BugPipelineState::Pending,
            assignee: None,
            reporter: "sashiko.dev".to_string(),
            reported_at: 2000,
            discovered_in_patchset_id: Some(ps1_id),
            discovered_in_patch_id: Some(p2_id),
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
        };
        let bug2_id = db
            .create_bug_with_enrichment(
                &bug2,
                Some(&NewBugEnrichment {
                    kind: "candidate".to_string(),
                    tool: "sashiko:linux_patch_review".to_string(),
                    model: Some("gemini-1.5-pro".to_string()),
                    author: Some("sashiko.dev".to_string()),
                    created_at: 2000,
                    content: Some("Second discovery".to_string()),
                    data_json: Some(json!({
                        "patchset_id": ps1_id,
                        "patch_id": p2_id,
                    })),
                    ..Default::default()
                }),
            )
            .await?;
        db.mark_bug_as_duplicate(MarkDuplicateBugParams {
            ephemeral_id: bug2_id,
            canonical_id: bug1_id,
            reasoning: "Duplicate of bug 1",
            ..Default::default()
        })
        .await?;

        // 3. Same bug discovered while reviewing patch 1 of patchset 2 -> duplicate of bug 1
        let bug3 = NewBug {
            bugid: "linux-bug-multi-3".to_string(),
            title: "Preexisting bug copy 3".to_string(),
            lifecycle_status: BugLifecycleStatus::New,
            pipeline_state: BugPipelineState::Pending,
            assignee: None,
            reporter: "sashiko.dev".to_string(),
            reported_at: 3000,
            discovered_in_patchset_id: Some(ps2_id),
            discovered_in_patch_id: Some(p3_id),
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
        };
        let bug3_id = db
            .create_bug_with_enrichment(
                &bug3,
                Some(&NewBugEnrichment {
                    kind: "candidate".to_string(),
                    tool: "sashiko:linux_patch_review".to_string(),
                    model: Some("gemini-2.0-flash".to_string()),
                    author: Some("sashiko.dev".to_string()),
                    created_at: 3000,
                    content: Some("Third discovery".to_string()),
                    data_json: Some(json!({
                        "patchset_id": ps2_id,
                        "patch_id": p3_id,
                    })),
                    ..Default::default()
                }),
            )
            .await?;
        db.mark_bug_as_duplicate(MarkDuplicateBugParams {
            ephemeral_id: bug3_id,
            canonical_id: bug1_id,
            reasoning: "Duplicate of bug 1",
            ..Default::default()
        })
        .await?;

        // 4. Same bug discovered with patchset and patch set on bug
        let bug4 = NewBug {
            bugid: "linux-bug-multi-4".to_string(),
            title: "Preexisting bug copy 4".to_string(),
            lifecycle_status: BugLifecycleStatus::New,
            pipeline_state: BugPipelineState::Pending,
            assignee: None,
            reporter: "sashiko.dev".to_string(),
            reported_at: 4000,
            discovered_in_patchset_id: Some(ps1_id),
            discovered_in_patch_id: Some(p2_id),
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
        };
        let bug4_id = db.create_bug(&bug4).await?;
        db.mark_bug_as_duplicate(MarkDuplicateBugParams {
            ephemeral_id: bug4_id,
            canonical_id: bug1_id,
            reasoning: "Duplicate of bug 1",
            ..Default::default()
        })
        .await?;

        // Query bug_evidence for canonical bug 1
        let evidence = db
            .bug_evidence(&db.bug_family(bug1_id, false).await?)
            .await?;
        assert_eq!(evidence["count"], 4);

        let discoveries = evidence["discoveries"].as_array().unwrap();
        assert_eq!(discoveries.len(), 4);

        // First discovery
        assert_eq!(discoveries[0]["bug_id"], bug1_id);
        assert_eq!(discoveries[0]["author"], "sashiko.dev");
        assert_eq!(discoveries[0]["tool"], "sashiko:linux_patch_review");
        assert_eq!(discoveries[0]["patchset_id"], ps1_id);
        assert_eq!(discoveries[0]["patch_id"], p1_id);
        assert_eq!(discoveries[0]["patch_part"], 1);

        // Second discovery
        assert_eq!(discoveries[1]["bug_id"], bug2_id);
        assert_eq!(discoveries[1]["author"], "sashiko.dev");
        assert_eq!(discoveries[1]["tool"], "sashiko:linux_patch_review");
        assert_eq!(discoveries[1]["patchset_id"], ps1_id);
        assert_eq!(discoveries[1]["patch_id"], p2_id);
        assert_eq!(discoveries[1]["patch_part"], 2);

        // Third discovery
        assert_eq!(discoveries[2]["bug_id"], bug3_id);
        assert_eq!(discoveries[2]["author"], "sashiko.dev");
        assert_eq!(discoveries[2]["tool"], "sashiko:linux_patch_review");
        assert_eq!(discoveries[2]["patchset_id"], ps2_id);
        assert_eq!(discoveries[2]["patch_id"], p3_id);
        assert_eq!(discoveries[2]["patch_part"], 1);

        // Fourth discovery (resolved via review link)
        assert_eq!(discoveries[3]["bug_id"], bug4_id);
        assert_eq!(discoveries[3]["patchset_id"], ps1_id);
        assert_eq!(discoveries[3]["patch_id"], p2_id);
        assert_eq!(discoveries[3]["patch_part"], 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_bug_audit_log_triggers() -> Result<()> {
        let db = setup_db().await;

        let bug = crate::db::NewBug {
            bugid: "AUDIT-123".to_string(),
            title: "Initial problem".to_string(),
            lifecycle_status: BugLifecycleStatus::New,
            pipeline_state: BugPipelineState::Pending,
            assignee: None,
            reporter: "test@example.com".to_string(),
            reported_at: chrono::Utc::now().timestamp(),
            source_ref: None,
            duplicate_of_id: None,
            discovered_in_commit: None,
            discovered_in_patch_id: None,
            discovered_in_patchset_id: None,
            vector_json: None,
            subsystems: vec![],
        };
        let bug_id = db.create_bug(&bug).await?;

        // 1. Update the triage status.
        db.set_bug_lifecycle_status(bug_id, BugLifecycleStatus::Open)
            .await?;

        // Fetch enrichments
        let bug = db.get_bug(bug_id).await?.unwrap();
        assert_eq!(bug.lifecycle_status, BugLifecycleStatus::Open);

        let enrichments = bug.enrichments;
        assert_eq!(enrichments.len(), 2); // creation and status

        assert!(enrichments.iter().any(|e| {
            e.kind == "audit"
                && e.content
                    .as_deref()
                    .unwrap_or("")
                    .contains(r#"Status changed from "new" to "open""#)
        }));

        Ok(())
    }
    use super::*;
    use crate::settings::DatabaseSettings;
    use std::sync::Arc;

    async fn setup_db() -> Arc<Database> {
        let settings = DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await.unwrap();
        db.migrate().await.unwrap();
        Arc::new(db)
    }

    #[tokio::test]
    async fn test_create_multiple_patchsets_in_thread() {
        let db = setup_db().await;

        // Create a thread
        let thread_id = db.create_thread("root", "Test Thread", 1000).await.unwrap();

        // 1. Create first patchset from Patch 1 (index 1)
        db.create_message(
            "msg1", thread_id, None, "Author A", "Patch 1", 1000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                thread_id,
                None,
                "msg1",
                "Patch 1",
                "Author A",
                1000,
                2,
                1,
                "to",
                "cc",
                Some(1),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(ps1.is_some());

        // 2. Add Cover Letter (index 0)
        // Should return same ID and update subject to "Cover Letter"
        db.create_message(
            "root",
            thread_id,
            None,
            "Author A",
            "Cover Letter",
            1005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1_update = db
            .create_patchset(
                thread_id,
                Some("root"),
                "root",
                "Cover Letter",
                "Author A",
                1005,
                2,
                1,
                "to",
                "cc",
                Some(1),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(ps1, ps1_update);

        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].subject.as_deref(), Some("Cover Letter"));

        // 3. Add Patch 2 (index 2)
        // Should NOT update subject (index 2 > index 0)
        db.create_message(
            "msg2", thread_id, None, "Author A", "Patch 2", 1006, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patchset(
            thread_id,
            None,
            "msg2",
            "Patch 2",
            "Author A",
            1006,
            2,
            1,
            "to",
            "cc",
            Some(1),
            2,
            None,
            true,
            None,
            None,
        )
        .await
        .unwrap();

        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].subject.as_deref(), Some("Cover Letter"));

        // 4. Create NEW patchset in same thread (Author B, Time 1000 - same time but diff author)
        // With relaxed logic, this SHOULD merge if total_parts match (assuming same series).
        let ps3 = db
            .create_patchset(
                thread_id,
                None,
                "msg_other",
                "Other Author",
                "Author B",
                1000,
                2,
                1,
                "to",
                "cc",
                Some(1),
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(ps3, ps1, "Different author in same series should merge");

        // 5. Create NEW patchset v2 (Author A, Time 1002 - close time, but v2)
        // Under new logic "Implicit matches Explicit", this SHOULD merge with ps1 (Implicit)
        // because time/author/total match.
        let ps_v2 = db
            .create_patchset(
                thread_id,
                None,
                "msg_v2",
                "[PATCH v2] Patchset 1",
                "Author B",
                1002,
                2,
                1,
                "to",
                "cc",
                Some(2),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert_ne!(
            ps1, ps_v2,
            "Implicit v1 should NOT merge with v2 even if time/author match"
        );

        // 7. Test Merging: Create disjoint patchsets then bridge them
        let t_merge = db
            .create_thread("root_merge", "Merge Test", 10000)
            .await
            .unwrap();

        // PS A (Time 10000)
        db.create_message(
            "m1", t_merge, None, "Merger", "P1", 10000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let psa = db
            .create_patchset(
                t_merge,
                None,
                "m1",
                "Series",
                "Merger",
                10000,
                3,
                1,
                "",
                "",
                Some(1),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // PS B (Time 200000) - 190000s diff > 86400s limit -> New PS
        db.create_message(
            "m2", t_merge, None, "Merger", "P3", 200000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let psb = db
            .create_patchset(
                t_merge,
                None,
                "m2",
                "Series",
                "Merger",
                200000,
                3,
                1,
                "",
                "",
                Some(1),
                3,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(psa, psb);

        // PS C (Time 100000) - 90000s diff from A (>86400), 100000s diff from B (>86400)
        // Wait, if C is > 86400 from both, it won't match either!
        // We need C to match BOTH.
        // A=10000. B=200000. Gap=190000.
        // If we want C to bridge, C needs to be within 86400 of A AND within 86400 of B.
        // But 190000 > 86400 * 2 (172800).
        // So it's IMPOSSIBLE to bridge with ONE message if they are that far apart!
        // We need A and B to be < 2 * 86400 apart.
        // Let's set B = 10000 + 100000 = 110000.
        // Diff = 100000. > 86400. So disjoint.
        // C = 10000 + 50000 = 60000.
        // Diff(A, C) = 50000 < 86400. Match A.
        // Diff(B, C) = 110000 - 60000 = 50000 < 86400. Match B.
        // So C bridges A and B.

        db.create_message(
            "m2_fixed", t_merge, None, "Merger", "P3_fixed", 120000, "", "", "", None, None,
        )
        .await
        .unwrap(); // 120000. Diff 110000 > 86400.
        let psb_fixed = db
            .create_patchset(
                t_merge,
                None,
                "m2_fixed",
                "Series",
                "Merger",
                120000,
                3,
                1,
                "",
                "",
                Some(1),
                3,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(psa, psb_fixed);

        // PS C (Time 65000)
        // Diff(A, C) = 55000 < 86400.
        // Diff(B, C) = 120000 - 65000 = 55000 < 86400.
        db.create_message(
            "m3", t_merge, None, "Merger", "P2", 65000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let psc = db
            .create_patchset(
                t_merge,
                None,
                "m3",
                "Series",
                "Merger",
                65000,
                3,
                1,
                "",
                "",
                Some(1),
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(psc, psa);
    }

    #[tokio::test]
    async fn test_five_patch_series_merging() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_5", "Five Patch Series", 20000)
            .await
            .unwrap();
        let author = "Series Author <author@example.com>";

        // Patches arrive in order: 1/5, 0/5, 2/5, 4/5, 3/5
        let indices = [1, 0, 2, 4, 3];
        let mut patchset_ids = Vec::new();

        for (i, &idx) in indices.iter().enumerate() {
            let msg_id = format!("msg_{}", idx);
            let subject = format!("[PATCH {}/5] Feature part {}", idx, idx);
            let time = 20000 + (i as i64 * 10); // 10s apart

            db.create_message(
                &msg_id, thread_id, None, author, &subject, time, "", "", "", None, None,
            )
            .await
            .unwrap();
            let ps_id = db
                .create_patchset(
                    thread_id,
                    if idx == 0 { Some(&msg_id) } else { None },
                    &msg_id,
                    &subject,
                    author,
                    time,
                    5,
                    1,
                    "to",
                    "cc",
                    None,
                    idx as u32,
                    None,
                    true,
                    None,
                    None,
                )
                .await
                .unwrap()
                .unwrap();

            patchset_ids.push(ps_id);
        }

        // All IDs should be the same
        let first_id = patchset_ids[0];
        for id in patchset_ids {
            assert_eq!(
                id, first_id,
                "All parts of the same series should share the same patchset ID"
            );
        }

        // Verify the final subject is the cover letter (index 0)
        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(
            list[0].subject.as_deref(),
            Some("[PATCH 0/5] Feature part 0")
        );
    }

    #[tokio::test]
    async fn test_patchset_status_transition() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_status", "Status Test", 60000)
            .await
            .unwrap();
        let author = "Status Author <status@example.com>";

        // 1. Create patchset with 2 parts. received=0 initially (cover letter doesn't count as received part in DB logic usually, but here we insert it)
        // Wait, create_patchset creates the set. create_patch updates received count.
        // We call create_patchset first.
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_status",
                "Status Test",
                author,
                60000,
                2,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Check initial status
        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Incomplete"));

        // 2. Add Patch 1. received=1. Total=2. Status should be Incomplete.
        db.create_message(
            "msg_1", thread_id, None, author, "Part 1", 60005, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_1", 1, "diff").await.unwrap();
        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Incomplete"));

        // 3. Add Patch 2. received=2. Total=2. Status should transition to Pending.
        db.create_message(
            "msg_2", thread_id, None, author, "Part 2", 60010, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_2", 2, "diff").await.unwrap();
        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Pending"));
    }

    #[tokio::test]
    async fn test_embargoed_patchset_dynamic_recalculation() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_embargo", "Embargo Test", 60000)
            .await
            .unwrap();
        let author = "Embargo Author <embargo@example.com>";

        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_embargo",
                "Embargo Test",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_message(
            "msg_embargo",
            thread_id,
            None,
            author,
            "Embargo Test",
            60000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_embargo", 1, "diff")
            .await
            .unwrap();

        db.conn
            .execute(
                "UPDATE patchsets SET status = 'Reviewed' WHERE id = ?",
                libsql::params![ps_id],
            )
            .await
            .unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        db.set_patchset_embargo_until(ps_id, now + 3600)
            .await
            .unwrap();

        db.create_review(ps_id, None, "gemini", "test-model", None, None)
            .await
            .unwrap();

        let patchsets = db.get_patchsets(10, 0, None, None).await.unwrap();
        assert_eq!(patchsets[0].status.as_deref(), Some("Embargoed"));
        let details = db
            .get_patchset_details(ps_id, None, None)
            .await
            .unwrap()
            .unwrap();
        assert!(
            details
                .get("reviews")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );

        db.set_patchset_embargo_until(ps_id, now - 3600)
            .await
            .unwrap();

        let patchsets = db.get_patchsets(10, 0, None, None).await.unwrap();
        assert_eq!(patchsets[0].status.as_deref(), Some("Reviewed"));
        let details = db
            .get_patchset_details(ps_id, None, None)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !details
                .get("reviews")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_clean_patchset_is_releasable_before_embargo_expiry() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_clean_embargo", "Clean Embargo", 70000)
            .await
            .unwrap();
        db.create_message(
            "msg_clean_embargo",
            thread_id,
            None,
            "Author <author@example.com>",
            "Clean Embargo",
            70000,
            "body",
            "list@example.com",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_clean_embargo",
                "Clean Embargo",
                "Author <author@example.com>",
                70000,
                1,
                1,
                "list@example.com",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch_id = db
            .create_patch(ps_id, "msg_clean_embargo", 1, "diff")
            .await
            .unwrap();
        let review_id = db
            .create_review(ps_id, Some(patch_id), "test", "test", None, None)
            .await
            .unwrap();
        db.complete_review(
            review_id,
            "Reviewed",
            "Review completed successfully.",
            Some("clean"),
            None,
            Some("No issues found."),
            None,
        )
        .await
        .unwrap();
        db.update_patchset_status(ps_id, "Reviewed").await.unwrap();

        let now = chrono::Utc::now().timestamp();
        db.set_patchset_embargo_until(ps_id, now + 3600)
            .await
            .unwrap();

        assert_eq!(
            db.get_patchset_review_outcome(ps_id).await.unwrap(),
            PatchsetReviewOutcome::Clean
        );
        let releasable = db
            .get_releasable_embargoed_patchsets(now, 10)
            .await
            .unwrap();
        assert!(releasable.iter().any(|patchset| patchset.id == ps_id));

        assert!(db.claim_patchset_embargo_release(ps_id, now).await.unwrap());
        assert!(!db.claim_patchset_embargo_release(ps_id, now).await.unwrap());
        assert!(
            db.claim_patchset_embargo_release(ps_id, now + 601)
                .await
                .unwrap()
        );
        db.clear_patchset_embargo_release_claim(ps_id)
            .await
            .unwrap();

        db.update_patchset_status(ps_id, "Pending").await.unwrap();
        assert!(
            !db.claim_patchset_embargo_release(ps_id, now + 601)
                .await
                .unwrap()
        );
        db.update_patchset_status(ps_id, "Reviewed").await.unwrap();

        db.create_finding(Finding {
            review_id,
            severity: Severity::Low,
            severity_explanation: None,
            problem: "Pre-existing issue".to_string(),
            preexisting: Some(true),
            locations: None,
        })
        .await
        .unwrap();

        assert_eq!(
            db.get_patchset_review_outcome(ps_id).await.unwrap(),
            PatchsetReviewOutcome::HasFindings
        );
        let releasable = db
            .get_releasable_embargoed_patchsets(now, 10)
            .await
            .unwrap();
        assert!(!releasable.iter().any(|patchset| patchset.id == ps_id));
    }

    #[tokio::test]
    async fn test_no_ai_review_is_not_clean() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_no_ai_embargo", "No AI Embargo", 71000)
            .await
            .unwrap();
        db.create_message(
            "msg_no_ai_embargo",
            thread_id,
            None,
            "Author <author@example.com>",
            "No AI Embargo",
            71000,
            "body",
            "list@example.com",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_no_ai_embargo",
                "No AI Embargo",
                "Author <author@example.com>",
                71000,
                1,
                1,
                "list@example.com",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch_id = db
            .create_patch(ps_id, "msg_no_ai_embargo", 1, "diff")
            .await
            .unwrap();
        let review_id = db
            .create_review(ps_id, Some(patch_id), "test", "test", None, None)
            .await
            .unwrap();
        db.complete_review(
            review_id,
            "Skipped",
            "Skipped AI review via --no-ai",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.update_patchset_status(ps_id, "Reviewed").await.unwrap();

        assert_eq!(
            db.get_patchset_review_outcome(ps_id).await.unwrap(),
            PatchsetReviewOutcome::Incomplete
        );

        let now = chrono::Utc::now().timestamp();
        db.set_patchset_embargo_until(ps_id, now + 3600)
            .await
            .unwrap();
        let releasable = db.get_releasable_embargoed_patchsets(now, 1).await.unwrap();
        assert!(releasable.iter().all(|patchset| patchset.id != ps_id));
    }

    #[tokio::test]
    async fn test_implicit_version_mismatch_should_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_v6", "Version 6 Series", 30000)
            .await
            .unwrap();
        let author = "Author V6 <v6@example.com>";

        // Case: Cover letter has v6, but patches don't say v6 (implicitly v1).
        // If the user forgot to version patches, they should NOT merge with strict version checking.
        // This prevents merging v1 patches into v6 series if timestamps overlap.

        // 1. Cover letter: [PATCH 00/33 v6] -> v6
        db.create_message(
            "msg_00",
            thread_id,
            None,
            author,
            "[PATCH 00/33 v6] Cover",
            30000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_cover = db
            .create_patchset(
                thread_id,
                Some("msg_00"),
                "msg_00",
                "[PATCH 00/33 v6] Cover",
                author,
                30000,
                33,
                1,
                "",
                "",
                Some(6),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Patch 1: [PATCH 01/33] -> v1 (implicit). Pass None.
        db.create_message(
            "msg_01",
            thread_id,
            None,
            author,
            "[PATCH 01/33] Part 1",
            30005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_p1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_01",
                "[PATCH 01/33] Part 1",
                author,
                30005,
                33,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Relaxed checking: Should merge because same thread
        assert_eq!(
            ps_cover, ps_p1,
            "Should merge explicit v6 cover with implicit v1 patch if in same thread"
        );
    }

    #[tokio::test]
    async fn test_unrelated_singletons_no_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_single", "Singletons", 60000)
            .await
            .unwrap();
        let author = "Author S <s@example.com>";

        // Patch A
        db.create_message(
            "msg_a",
            thread_id,
            None,
            author,
            "[PATCH] Fix A",
            60000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_a = db
            .create_patchset(
                thread_id,
                None,
                "msg_a",
                "[PATCH] Fix A",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Patch B (Close time, same author, implicit version, total=1)
        db.create_message(
            "msg_b",
            thread_id,
            None,
            author,
            "[PATCH] Fix B",
            60005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_b = db
            .create_patchset(
                thread_id,
                None,
                "msg_b",
                "[PATCH] Fix B",
                author,
                60005,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_ne!(
            ps_a, ps_b,
            "Should NOT merge unrelated singletons even if author/time match"
        );
    }

    #[tokio::test]
    async fn test_singleton_cover_patch_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_1of1", "Singleton Series", 60000)
            .await
            .unwrap();
        let author = "Author 1of1 <1@example.com>";

        // Cover: [PATCH 0/1] Subject A
        db.create_message(
            "msg_0",
            thread_id,
            None,
            author,
            "[PATCH 0/1] Subject A",
            60000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_0 = db
            .create_patchset(
                thread_id,
                Some("msg_0"),
                "msg_0",
                "[PATCH 0/1] Subject A",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Patch: [PATCH 1/1] Subject B (Different subject)
        db.create_message(
            "msg_1",
            thread_id,
            None,
            author,
            "[PATCH 1/1] Subject B",
            60005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_1",
                "[PATCH 1/1] Subject B",
                author,
                60005,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps_0, ps_1,
            "Should merge 0/1 and 1/1 even if subjects differ"
        );
    }

    #[tokio::test]
    async fn test_version_mismatch_no_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_diff_ver", "Version Mismatch", 40000)
            .await
            .unwrap();
        let author = "Author Diff <diff@example.com>";

        // v5
        db.create_message(
            "msg_v5",
            thread_id,
            None,
            author,
            "[PATCH v5 1/2] Part 1",
            40000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_v5 = db
            .create_patchset(
                thread_id,
                None,
                "msg_v5",
                "[PATCH v5 1/2] Part 1",
                author,
                40000,
                2,
                1,
                "",
                "",
                Some(5),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Add patch to trigger index collision logic
        db.create_patch(ps_v5, "msg_v5", 1, "diff").await.unwrap();

        // v6 (Close time)
        db.create_message(
            "msg_v6",
            thread_id,
            None,
            author,
            "[PATCH v6 1/2] Part 1",
            40010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_v6 = db
            .create_patchset(
                thread_id,
                None,
                "msg_v6",
                "[PATCH v6 1/2] Part 1",
                author,
                40010,
                2,
                1,
                "",
                "",
                Some(6),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_ne!(
            ps_v5, ps_v6,
            "Should NOT merge different explicit versions (v5 vs v6)"
        );
    }

    #[tokio::test]
    async fn test_v3_series_fragmentation() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_v3", "v3 Series", 50000)
            .await
            .unwrap();
        let author = "Author V3 <v3@example.com>";

        // 1. [PATCH v3 0/2] (Cover)
        db.create_message(
            "v3_0",
            thread_id,
            None,
            author,
            "[PATCH v3 0/2] Cover",
            50000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_0 = db
            .create_patchset(
                thread_id,
                Some("v3_0"),
                "v3_0",
                "[PATCH v3 0/2] Cover",
                author,
                50000,
                2,
                1,
                "",
                "",
                Some(3),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. [PATCH v3 1/2]
        db.create_message(
            "v3_1",
            thread_id,
            None,
            author,
            "[PATCH v3 1/2] Part 1",
            50005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_1 = db
            .create_patchset(
                thread_id,
                None,
                "v3_1",
                "[PATCH v3 1/2] Part 1",
                author,
                50005,
                2,
                1,
                "",
                "",
                Some(3),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 3. [PATCH v3 2/2]
        db.create_message(
            "v3_2",
            thread_id,
            None,
            author,
            "[PATCH v3 2/2] Part 2",
            50010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_2 = db
            .create_patchset(
                thread_id,
                None,
                "v3_2",
                "[PATCH v3 2/2] Part 2",
                author,
                50010,
                2,
                1,
                "",
                "",
                Some(3),
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(ps_0, ps_1, "Patch 1 should merge with Cover");
        assert_eq!(ps_0, ps_2, "Patch 2 should merge with Cover");
    }

    #[tokio::test]
    async fn test_merge_with_confusing_version_in_subject() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_confusing", "Confusing Versions", 80000)
            .await
            .unwrap();
        let author = "Confused Author <confused@example.com>";

        // 1. [PATCH v3 00/17] (v3)
        db.create_message(
            "msg_v3_conf_00",
            thread_id,
            None,
            author,
            "[PATCH v3 00/17] Cover",
            80000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_cover = db
            .create_patchset(
                thread_id,
                Some("msg_v3_conf_00"),
                "msg_v3_conf_00",
                "[PATCH v3 00/17] Cover",
                author,
                80000,
                17,
                1,
                "",
                "",
                Some(3),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. [PATCH 01/17] Support v2 hardware. Treat as implicit version (None), NOT v2.
        db.create_message(
            "msg_conf_01",
            thread_id,
            None,
            author,
            "[PATCH 01/17] Support v2 hardware",
            80005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        // Here we simulate the parser extracting "2" from "v2" if it's aggressive
        // But `create_patchset` takes the *parsed* version.
        // If we want to simulate the BUG, we must pass what `parse_email` WOULD pass.
        // `parse_email` uses `parse_subject_version`.
        // Let's check what `parse_subject_version` does for this string.
        let subject = "[PATCH v3 01/17] Support v2 hardware";
        let parsed_ver = crate::patch::parse_subject_version(subject);

        let ps_part1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_conf_01",
                subject,
                author,
                80005,
                17,
                1,
                "",
                "",
                parsed_ver, // Pass the result of the potentially buggy parser
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps_cover, ps_part1,
            "Should merge because subject implies v3 (and ignores v2 in text)"
        );
    }

    #[tokio::test]
    async fn test_merge_patchsets_with_dependencies() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_deps", "Dependencies Test", 90000)
            .await
            .unwrap();
        let author = "Deps Author <deps@example.com>";

        // 1. Create first patchset part [PATCH 1/2]
        db.create_message(
            "msg_deps_1",
            thread_id,
            None,
            author,
            "[PATCH 1/2] Part 1",
            90000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_deps_1",
                "[PATCH 1/2] Part 1",
                author,
                90000,
                2,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Add dependencies to ps1 (Review, Tag, Subsystem)
        let review_id = db
            .create_review(ps1, None, "gemini", "test-model", None, None)
            .await
            .unwrap();

        let sub_id = db
            .ensure_subsystem("test_sub", "test@example.com")
            .await
            .unwrap();
        db.add_subsystem_to_patchset(ps1, sub_id).await.unwrap();

        // 3. Create second patchset part [PATCH 2/2] -> Should merge into ps1 (or ps1 into ps2, but we keep oldest ID so ps2 into ps1)
        // ps1 ID should be preserved because it was created first.
        db.create_message(
            "msg_deps_2",
            thread_id,
            None,
            author,
            "[PATCH 2/2] Part 2",
            90005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps2 = db
            .create_patchset(
                thread_id,
                None,
                "msg_deps_2",
                "[PATCH 2/2] Part 2",
                author,
                90005, // Close enough
                2,
                1,
                "",
                "",
                None,
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(ps1, ps2, "Patchsets should have merged");

        // 4. Verify dependencies moved
        // Check review
        let mut rows = db
            .conn
            .query(
                "SELECT patchset_id FROM reviews WHERE id = ?",
                libsql::params![review_id],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let review_ps_id: i64 = row.get(0).unwrap();
        assert_eq!(review_ps_id, ps1);

        // Check subsystem
        let mut rows = db
            .conn
            .query(
                "SELECT count(*) FROM patchsets_subsystems WHERE patchset_id = ?",
                libsql::params![ps1],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let count: i64 = row.get(0).unwrap();
        assert_eq!(count, 1);

        let summary = db
            .get_patchset_summary(ps1, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary["mailing_lists"], serde_json::json!(["test_sub"]));
        assert_eq!(summary["subsystems"], serde_json::json!(["test_sub"]));

        let details = db
            .get_patchset_details(ps1, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details["mailing_lists"], serde_json::json!(["test_sub"]));
        assert_eq!(details["subsystems"], serde_json::json!(["test_sub"]));

        let list_items = db.get_patchsets(50, 0, None, None).await.unwrap();
        let found = list_items.iter().find(|p| p.id == ps1).unwrap();
        assert_eq!(found.mailing_lists, vec!["test_sub".to_string()]);
        assert_eq!(found.subsystems, vec!["test_sub".to_string()]);
    }

    #[tokio::test]
    async fn test_create_ai_interaction_with_cached_tokens() {
        let db = setup_db().await;

        // Create interaction
        let params = AiInteractionParams {
            id: "test_id",
            parent_id: None,
            workflow_id: None,
            provider: "test_provider",
            model: "test_model",
            input: "input",
            output: "output",
            tokens_in: 100,
            tokens_out: 50,
            tokens_cached: 25,
        };

        db.create_ai_interaction(params).await.unwrap();

        // Verify via raw query since there is no direct get_ai_interaction method exposed
        // (get_review_details joins it, but requires a review)

        let mut rows = db
            .conn
            .query(
                "SELECT tokens_cached FROM ai_interactions WHERE id = 'test_id'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let cached: u32 = row.get(0).unwrap();

        assert_eq!(cached, 25);
    }

    #[tokio::test]
    async fn test_has_failed_review_logic() {
        let db = setup_db().await;

        // Setup patchset
        let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();
        db.create_message(
            "msg1", thread_id, None, "Author", "Subject", 100, "", "", "", None, None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                Some("msg1"),
                "msg1",
                "Subject",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch_id = db.create_patch(ps_id, "msg1", 1, "diff").await.unwrap();

        // 1. Initial State: No reviews
        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 2. Failed Review (No interaction) -> Should be detected
        let review_id = db
            .create_review(ps_id, Some(patch_id), "gemini", "test-model", None, None)
            .await
            .unwrap();
        db.update_review_status(review_id, "FailedToApply", None)
            .await
            .unwrap();

        assert!(db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 3. Status "Failed" (No interaction) -> Should be detected
        db.update_review_status(review_id, "Failed", None)
            .await
            .unwrap();
        assert!(db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 4. Status "Reviewed" (Success) -> Should NOT be detected
        db.update_review_status(review_id, "Reviewed", None)
            .await
            .unwrap();
        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 5. Status "Failed" WITH interaction_id -> Should NOT be detected (reached AI)
        // Revert to Failed first
        db.update_review_status(review_id, "Failed", None)
            .await
            .unwrap();

        // Create interaction first to satisfy FK
        db.create_ai_interaction(AiInteractionParams {
            id: "int_id",
            parent_id: None,
            workflow_id: None,
            provider: "p",
            model: "m",
            input: "",
            output: "",
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        })
        .await
        .unwrap();

        // Set interaction_id
        db.complete_review(
            review_id,
            "Failed",
            "desc",
            None,
            Some("int_id"),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());
    }

    #[tokio::test]
    async fn test_get_review_details() {
        let db = setup_db().await;

        let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();
        db.create_message(
            "msg1", thread_id, None, "Author", "Subject", 100, "", "", "", None, None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                Some("msg1"),
                "msg1",
                "Subject",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch_id = db.create_patch(ps_id, "msg1", 1, "diff").await.unwrap();

        let baseline_id = db
            .create_baseline(
                Some("https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git"),
                Some("master"),
                Some("commit_hash"),
            )
            .await
            .unwrap();

        db.create_ai_interaction(AiInteractionParams {
            id: "interaction_1",
            parent_id: None,
            workflow_id: None,
            provider: "gemini",
            model: "gemini-2.5",
            input: "prompt input",
            output: "prompt output",
            tokens_in: 120,
            tokens_out: 45,
            tokens_cached: 10,
        })
        .await
        .unwrap();

        let review_id = db
            .create_review(
                ps_id,
                Some(patch_id),
                "gemini",
                "gemini-2.5",
                Some(baseline_id),
                Some("hash123"),
            )
            .await
            .unwrap();

        db.complete_review(
            review_id,
            "Reviewed",
            "LGTM",
            Some("Summary of patch"),
            Some("interaction_1"),
            Some("Inline comment"),
            Some("{\"step\": \"done\"}"),
        )
        .await
        .unwrap();

        let details = db
            .get_review_details(review_id)
            .await
            .unwrap()
            .expect("review details should exist");
        assert_eq!(details["id"], review_id);
        assert_eq!(details["model"], "gemini-2.5");
        assert_eq!(details["provider"], "gemini");
        assert_eq!(details["prompts_hash"], "hash123");
        assert_eq!(details["summary"], "Summary of patch");
        assert_eq!(details["result"], "LGTM");
        assert_eq!(details["status"], "Reviewed");
        assert_eq!(details["inline_review"], "Inline comment");
        assert_eq!(details["logs"], "{\"step\": \"done\"}");
        assert_eq!(details["tokens_in"], 120);
        assert_eq!(details["tokens_out"], 45);
        assert_eq!(details["tokens_cached"], 10);
        assert_eq!(details["baseline"]["branch"], "master");
        assert_eq!(details["baseline"]["commit"], "commit_hash");

        // Non-existent review
        let none_details = db.get_review_details(99999).await.unwrap();
        assert!(none_details.is_none());
    }

    #[tokio::test]
    async fn test_rerun_patchset_logic() {
        let db = setup_db().await;

        // Setup patchset
        let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();

        // Create messages to satisfy FK constraints
        db.create_message(
            "msg_cl1", thread_id, None, "Author", "Cover 1", 100, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_message(
            "msg_p1",
            thread_id,
            Some("msg_cl1"),
            "Author",
            "Patch 1",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            "msg_cl2", thread_id, None, "Author", "Cover 2", 100, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_message(
            "msg_p2",
            thread_id,
            Some("msg_cl2"),
            "Author",
            "Patch 2",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        // Create a patchset that is "Reviewed"
        let ps_reviewed = db
            .create_patchset(
                thread_id,
                Some("msg_cl1"),
                "msg_cl1",
                "Subject Reviewed",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        db.update_patchset_status(ps_reviewed, "Reviewed")
            .await
            .unwrap();

        // Create a patchset that is "Failed"
        let ps_failed = db
            .create_patchset(
                thread_id,
                Some("msg_cl2"),
                "msg_cl2",
                "Subject Failed",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        db.update_patchset_status(ps_failed, "Failed")
            .await
            .unwrap();

        // Add a patch to ps_failed
        let patch_id = db
            .create_patch(ps_failed, "msg_p2", 1, "diff")
            .await
            .unwrap();

        // Add a failed review without interaction (infra failure) to ps_failed
        let review_infra = db
            .create_review(ps_failed, Some(patch_id), "p", "m", None, None)
            .await
            .unwrap();
        db.update_review_status(review_infra, "FailedToApply", None)
            .await
            .unwrap();

        // Add a failed review WITH interaction (AI failure) to ps_failed
        let review_ai = db
            .create_review(ps_failed, Some(patch_id), "p", "m", None, None)
            .await
            .unwrap();
        db.create_ai_interaction(AiInteractionParams {
            id: "int_id2",
            parent_id: None,
            workflow_id: None,
            provider: "p",
            model: "m",
            input: "",
            output: "",
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        })
        .await
        .unwrap();
        db.complete_review(
            review_ai,
            "Failed",
            "desc",
            None,
            Some("int_id2"),
            None,
            None,
        )
        .await
        .unwrap();

        // Verify initial target counts (should be 1)
        let mut rows = db
            .conn
            .query(
                "SELECT target_review_count FROM patchsets WHERE id = ?",
                libsql::params![ps_reviewed],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let target: i64 = row.get(0).unwrap();
        assert_eq!(target, 1);

        let mut rows = db
            .conn
            .query(
                "SELECT target_review_count FROM patchsets WHERE id = ?",
                libsql::params![ps_failed],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let target: i64 = row.get(0).unwrap();
        assert_eq!(target, 1);

        // Verify blocking review is present
        assert!(
            db.has_failed_review(ps_failed, patch_id, None)
                .await
                .unwrap()
        );

        // RERUN Reviewed patchset -> Should increment target count
        db.rerun_patchset(ps_reviewed).await.unwrap();
        let mut rows = db
            .conn
            .query(
                "SELECT target_review_count FROM patchsets WHERE id = ?",
                libsql::params![ps_reviewed],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let target: i64 = row.get(0).unwrap();
        assert_eq!(target, 2);

        // RERUN Failed patchset -> Should NOT increment target count
        db.rerun_patchset(ps_failed).await.unwrap();
        let mut rows = db
            .conn
            .query(
                "SELECT target_review_count FROM patchsets WHERE id = ?",
                libsql::params![ps_failed],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let target: i64 = row.get(0).unwrap();
        assert_eq!(target, 1);

        // Verify blocking review was deleted
        let mut rows = db
            .conn
            .query(
                "SELECT 1 FROM reviews WHERE id = ?",
                libsql::params![review_infra],
            )
            .await
            .unwrap();
        assert!(rows.next().await.unwrap().is_none());

        // Verify blocking review is NO LONGER blocking
        assert!(
            !db.has_failed_review(ps_failed, patch_id, None)
                .await
                .unwrap()
        );

        // Verify AI failure review is NOT cancelled (remains Failed)
        let mut rows = db
            .conn
            .query(
                "SELECT status FROM reviews WHERE id = ?",
                libsql::params![review_ai],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let status: String = row.get(0).unwrap();
        assert_eq!(status, "Failed");
    }

    #[tokio::test]
    async fn test_cross_thread_no_merge() {
        let db = setup_db().await;

        // 1. Create Thread A and Patchset A (1/2)
        let t1 = db
            .create_thread("root1", "Subject 1/2", 1000)
            .await
            .unwrap();
        db.create_message(
            "msg1",
            t1,
            None,
            "Author",
            "[PATCH 1/2] Series",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                t1,
                None,
                "msg1",
                "[PATCH 1/2] Series",
                "Author",
                1000,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Create Thread B and Patchset B (2/2) - Same Author, Close Time, Different Thread
        let t2 = db
            .create_thread("root2", "Subject 2/2", 1005)
            .await
            .unwrap(); // 5 seconds later
        db.create_message(
            "msg2",
            t2,
            None,
            "Author",
            "[PATCH 2/2] Series",
            1005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps2 = db
            .create_patchset(
                t2,
                None,
                "msg2",
                "[PATCH 2/2] Series",
                "Author",
                1005,
                2,
                0,
                "",
                "",
                None,
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 3. Assert they DID NOT merge (ps2 should NOT equal ps1)
        assert_ne!(
            ps1, ps2,
            "Patchsets from different threads should NOT merge even if author/time match"
        );

        // 4. Verify total patches count or received parts
        db.create_patch(ps1, "msg1", 1, "").await.unwrap();
        db.create_patch(ps2, "msg2", 2, "").await.unwrap();

        let details1 = db
            .get_patchset_details(ps1, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details1["received_parts"], 1);
        let details2 = db
            .get_patchset_details(ps2, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details2["received_parts"], 1);
    }

    #[tokio::test]
    async fn test_duplicate_ingestion_on_full_patchset() {
        let db = setup_db().await;

        // 1. Create Patchset (1/1)
        let t1 = db.create_thread("root1", "Subject", 1000).await.unwrap();
        let msg_id = "msg1";

        db.create_message(
            msg_id,
            t1,
            None,
            "Author",
            "[PATCH 1/1] Subject",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps1 = db
            .create_patchset(
                t1,
                None,
                msg_id,
                "[PATCH 1/1] Subject",
                "Author",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Add patch so it becomes full
        db.create_patch(ps1, msg_id, 1, "diff").await.unwrap();

        let details = db
            .get_patchset_details(ps1, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details["received_parts"], 1);
        assert_eq!(details["total_parts"], 1);

        // 3. Try to ingest the SAME patch again
        // It matches the existing patchset (Author/Time/Thread).
        // It IS full (1/1).
        // But it IS a duplicate (msg_id matches).
        // So it SHOULD merge.
        let ps2 = db
            .create_patchset(
                t1,
                None,
                msg_id,
                "[PATCH 1/1] Subject",
                "Author",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps1, ps2,
            "Should merge duplicate into existing patchset even if full"
        );

        // 4. Try to ingest a NEW patch (different ID) that looks like it belongs
        // This simulates a collision or a separate series with same metadata.
        // It should NOT merge because the set is full and it's NOT a duplicate.
        let msg_id_new = "msg_new";
        db.create_message(
            msg_id_new,
            t1,
            None,
            "Author",
            "[PATCH 1/1] Subject",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps3 = db
            .create_patchset(
                t1,
                None,
                msg_id_new,
                "[PATCH 1/1] Subject",
                "Author",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_ne!(
            ps1, ps3,
            "Should create NEW patchset for non-duplicate when full"
        );
    }

    #[tokio::test]
    async fn test_mailing_list_filtering() {
        let db = setup_db().await;

        // 1. Setup lists
        db.ensure_mailing_list("List A", "list-a").await.unwrap();
        db.ensure_mailing_list("List B", "list-b").await.unwrap();
        let id_a = db
            .get_mailing_list_id_by_name("list-a")
            .await
            .unwrap()
            .unwrap();
        let id_b = db
            .get_mailing_list_id_by_name("list-b")
            .await
            .unwrap()
            .unwrap();

        // 2. Create threads
        let t_a = db.create_thread("root_a", "Subject A", 100).await.unwrap();
        let t_b = db.create_thread("root_b", "Subject B", 100).await.unwrap();

        // 3. Create Message A (in List A)
        db.create_message(
            "msg_a",
            t_a,
            None,
            "Author",
            "Subject A",
            100,
            "",
            "",
            "",
            None,
            Some("list-a"),
        )
        .await
        .unwrap();
        let msg_a_id = db.get_message_id_by_msg_id("msg_a").await.unwrap().unwrap();
        db.add_message_to_mailing_list(msg_a_id, id_a)
            .await
            .unwrap();

        // 4. Create Message B (in List B)
        db.create_message(
            "msg_b",
            t_b,
            None,
            "Author",
            "Subject B",
            100,
            "",
            "",
            "",
            None,
            Some("list-b"),
        )
        .await
        .unwrap();
        let msg_b_id = db.get_message_id_by_msg_id("msg_b").await.unwrap().unwrap();
        db.add_message_to_mailing_list(msg_b_id, id_b)
            .await
            .unwrap();

        // 5. Create Patchsets
        // Patchset A linked to msg_a (as cover letter)
        let ps_a = db
            .create_patchset(
                t_a,
                Some("msg_a"),
                "msg_a",
                "Subject A",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Patchset B linked to msg_b (as cover letter)
        let ps_b = db
            .create_patchset(
                t_b,
                Some("msg_b"),
                "msg_b",
                "Subject B",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 6. Test filtering messages
        let msgs_a = db
            .get_messages(10, 0, None, Some("list-a".to_string()))
            .await
            .unwrap();
        assert_eq!(msgs_a.len(), 1);
        assert_eq!(msgs_a[0].message_id, "msg_a");

        let msgs_b = db
            .get_messages(10, 0, None, Some("list-b".to_string()))
            .await
            .unwrap();
        assert_eq!(msgs_b.len(), 1);
        assert_eq!(msgs_b[0].message_id, "msg_b");

        // 7. Add patch to ps_a to make it pass the CURRENT logic (patches only)
        // db.create_message(
        //     "patch_a_1", t_a, None, "Author", "Patch A 1", 101, "", "", "", None, Some("list-a")
        // ).await.unwrap();
        // let p_a_1_id = db.get_message_id_by_msg_id("patch_a_1").await.unwrap().unwrap();
        // db.add_message_to_mailing_list(p_a_1_id, id_a).await.unwrap();
        // db.create_patch(ps_a, "patch_a_1", 1, "").await.unwrap();

        // Now ps_a has a patch in list-a.
        // UPDATE: We commented out the patch creation above.
        // ps_a only has a cover letter in list-a.
        // The UNION query should find it.
        let psets_a = db
            .get_patchsets(10, 0, None, Some("list-a".to_string()))
            .await
            .unwrap();
        assert_eq!(psets_a.len(), 1);
        assert_eq!(psets_a[0].id, ps_a);

        let psets_b = db
            .get_patchsets(10, 0, None, Some("list-a".to_string()))
            .await
            .unwrap();
        let found_b = psets_b.iter().any(|p| p.id == ps_b);
        assert!(!found_b);
    }

    #[tokio::test]
    async fn test_tool_usages_telemetry() {
        let db = setup_db().await;

        let thread_id = db.create_thread("root", "Test Thread", 1000).await.unwrap();
        db.create_message(
            "msg1", thread_id, None, "Author", "Subject", 1000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id, None, "msg1", "Subject", "Author", 1000, 1, 1, "", "", None, 1, None,
                true, None, None,
            )
            .await
            .unwrap()
            .unwrap();

        let review_id = db
            .create_review(ps_id, None, "gemini", "test-model", None, None)
            .await
            .unwrap();

        db.create_tool_usage(ToolUsage {
            review_id,
            provider: "test_prov".to_string(),
            model: "test_model".to_string(),
            tool_name: "git_grep".to_string(),
            arguments: Some("{\"pattern\":\"gup_fast\"}".to_string()),
            output_length: 0,
        })
        .await
        .unwrap();

        db.update_tool_usage_length(review_id, "git_grep", "{\"pattern\":\"gup_fast\"}", 456)
            .await
            .unwrap();

        let stmt = db
            .conn
            .prepare("SELECT output_length FROM tool_usages WHERE review_id = ?")
            .await
            .unwrap();
        let mut rows = stmt.query(libsql::params![review_id]).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let length: i64 = row.get(0).unwrap();
        assert_eq!(length, 456);
    }

    #[tokio::test]
    async fn test_message_references_header_storage() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root", "References Thread", 1000)
            .await
            .unwrap();

        db.create_message_with_references(
            "msg1",
            thread_id,
            None,
            "Author",
            "Subject 1",
            1000,
            "",
            "",
            "",
            None,
            None,
            None,
        )
        .await
        .unwrap();

        db.create_message_with_references(
            "msg2",
            thread_id,
            Some("msg1"),
            "Author",
            "Subject 2",
            1001,
            "",
            "",
            "",
            None,
            None,
            Some("msg1"),
        )
        .await
        .unwrap();

        db.create_message_with_references(
            "msg3",
            thread_id,
            Some("msg2"),
            "Author",
            "Subject 3",
            1002,
            "",
            "",
            "",
            None,
            None,
            Some("msg1 msg2"),
        )
        .await
        .unwrap();

        let msg3 = db
            .get_message_details_by_msgid("msg3")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg3.references_hdr.as_deref(), Some("msg1 msg2"));

        let msg2 = db
            .get_message_details_by_msgid("msg2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg2.references_hdr.as_deref(), Some("msg1"));

        let msg1 = db
            .get_message_details_by_msgid("msg1")
            .await
            .unwrap()
            .unwrap();
        assert!(msg1.references_hdr.is_none());
    }

    #[tokio::test]
    async fn test_merge_b4_relay_alias_with_real_author() {
        let db = setup_db().await;

        // 1. Create Thread
        let thread_id = db
            .create_thread("root_b4_merge", "Subject", 1000)
            .await
            .unwrap();

        // 2. Create Patchset Part 1 (devnull alias)
        let ps1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_b4_1",
                "[PATCH 1/2] B4 Merge Series",
                "devnull+author.example.com@kernel.org",
                1000,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 3. Create Patchset Part 2 (real email address)
        let ps2 = db
            .create_patchset(
                thread_id,
                None,
                "msg_b4_2",
                "[PATCH 2/2] B4 Merge Series",
                "Real Author <author@example.com>",
                1010,
                2,
                0,
                "",
                "",
                None,
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 4. Assert they merged (ps1 == ps2)
        assert_eq!(
            ps1, ps2,
            "Patchset from B4 Relay devnull alias and real author email MUST merge"
        );
    }

    /// Add one part of a series to the patchset identified by cover
    /// letter "cover".
    async fn add_part(db: &Database, thread_id: i64, part: u32, baseline: Option<i64>) -> i64 {
        let msg = format!("msg_{}", part);
        db.create_message(
            &msg,
            thread_id,
            Some("cover"),
            "author@example.com",
            "Patch",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        db.create_patchset(
            thread_id,
            Some("cover"),
            &msg,
            &format!("[PATCH {}/3] Subject", part),
            "author@example.com",
            100,
            3,
            1,
            "",
            "",
            None,
            part,
            baseline,
            true,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap()
    }

    async fn patchset_baseline(db: &Database, id: i64) -> Option<i64> {
        let mut rows = db
            .conn
            .query(
                "SELECT baseline_id FROM patchsets WHERE id = ?",
                libsql::params![id],
            )
            .await
            .unwrap();
        rows.next().await.unwrap().unwrap().get(0).ok()
    }

    #[tokio::test]
    async fn test_baseline_comes_from_lowest_part() {
        // A later part must not overwrite the first patch's parent,
        // whatever order the parts arrive in.
        for order in [[1u32, 2, 3], [3, 2, 1], [2, 3, 1]] {
            let db = setup_db().await;
            let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();
            db.create_message(
                "cover",
                thread_id,
                None,
                "author@example.com",
                "Cover",
                100,
                "",
                "",
                "",
                None,
                None,
            )
            .await
            .unwrap();

            let base = db
                .create_baseline(None, None, Some("series_base"))
                .await
                .unwrap();
            let after1 = db
                .create_baseline(None, None, Some("patch1"))
                .await
                .unwrap();
            let after2 = db
                .create_baseline(None, None, Some("patch2"))
                .await
                .unwrap();

            let mut ps_id = 0;
            for part in order {
                // Part N's parent is patch N-1; part 1's parent is the base.
                let baseline = match part {
                    1 => base,
                    2 => after1,
                    _ => after2,
                };
                ps_id = add_part(&db, thread_id, part, Some(baseline)).await;
            }

            assert_eq!(
                patchset_baseline(&db, ps_id).await,
                Some(base),
                "arrival order {:?} must leave the first patch's parent as the baseline",
                order
            );
        }
    }

    #[tokio::test]
    async fn test_baseline_recorded_when_lowest_part_has_none() {
        // A cover letter with no base-commit trailer wins the lowest part
        // index while supplying no baseline.
        let db = setup_db().await;
        let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();
        db.create_message(
            "cover",
            thread_id,
            None,
            "author@example.com",
            "Cover",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let base = db
            .create_baseline(None, None, Some("series_base"))
            .await
            .unwrap();

        let ps_id = add_part(&db, thread_id, 0, None).await;
        assert_eq!(patchset_baseline(&db, ps_id).await, None);

        let ps_id = add_part(&db, thread_id, 1, Some(base)).await;
        assert_eq!(patchset_baseline(&db, ps_id).await, Some(base));
    }

    #[tokio::test]
    async fn test_baseline_from_lowest_part_behind_a_cover_letter() {
        let db = setup_db().await;
        let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();
        db.create_message(
            "cover",
            thread_id,
            None,
            "author@example.com",
            "Cover",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let base = db
            .create_baseline(None, None, Some("series_base"))
            .await
            .unwrap();
        let after1 = db
            .create_baseline(None, None, Some("patch1"))
            .await
            .unwrap();

        add_part(&db, thread_id, 0, None).await;
        let ps_id = add_part(&db, thread_id, 2, Some(after1)).await;
        assert_eq!(patchset_baseline(&db, ps_id).await, Some(after1));

        let ps_id = add_part(&db, thread_id, 1, Some(base)).await;
        assert_eq!(
            patchset_baseline(&db, ps_id).await,
            Some(base),
            "patch 1 must replace the baseline a later part filled in"
        );
    }

    #[tokio::test]
    async fn test_baseline_corrected_by_resubmitted_lowest_part() {
        // Resubmitting a series is how a patchset that recorded the
        // wrong baseline gets repaired.
        let db = setup_db().await;
        let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();
        db.create_message(
            "cover",
            thread_id,
            None,
            "author@example.com",
            "Cover",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let wrong = db.create_baseline(None, None, Some("wrong")).await.unwrap();
        let base = db
            .create_baseline(None, None, Some("series_base"))
            .await
            .unwrap();

        let ps_id = add_part(&db, thread_id, 1, Some(wrong)).await;
        assert_eq!(patchset_baseline(&db, ps_id).await, Some(wrong));

        let ps_id = add_part(&db, thread_id, 1, Some(base)).await;
        assert_eq!(patchset_baseline(&db, ps_id).await, Some(base));
    }

    /// Add one part of a series as its own patchset, reached through the
    /// author and time matching path rather than the cover letter lookup.
    /// The parts share a git send-email message-id prefix, which is what
    /// lets a later part match across threads and merge the two rows.
    async fn add_unthreaded_part(
        db: &Database,
        thread_id: i64,
        part: u32,
        date: i64,
        baseline: Option<i64>,
    ) -> i64 {
        let author = "Merge Author <merge@example.com>";
        let msg = format!("20260731120000.4242-{}-merge@example.com", part);
        let subject = format!("[PATCH {}/3] Subject", part);
        db.create_message(
            &msg, thread_id, None, author, &subject, date, "", "", "", None, None,
        )
        .await
        .unwrap();

        let id = db
            .create_patchset(
                thread_id, None, &msg, &subject, author, date, 3, 1, "", "", None, part, baseline,
                true, None, None,
            )
            .await
            .unwrap()
            .unwrap();
        db.create_patch(id, &msg, part, "").await.unwrap();
        id
    }

    #[tokio::test]
    async fn test_baseline_survives_patchset_merge() {
        // Two patchsets form because the parts land in separate threads
        // more than a day apart. A part between them matches both, and the
        // merge keeps the row created first -- the one holding the higher
        // part's baseline.
        let db = setup_db().await;
        let thread_hi = db
            .create_thread("root_hi", "Subject", 100_000)
            .await
            .unwrap();
        let thread_lo = db
            .create_thread("root_lo", "Subject", 250_000)
            .await
            .unwrap();
        let thread_mid = db
            .create_thread("root_mid", "Subject", 175_000)
            .await
            .unwrap();

        let base = db
            .create_baseline(None, None, Some("series_base"))
            .await
            .unwrap();
        let after1 = db
            .create_baseline(None, None, Some("patch1"))
            .await
            .unwrap();
        let after2 = db
            .create_baseline(None, None, Some("patch2"))
            .await
            .unwrap();

        let ps_hi = add_unthreaded_part(&db, thread_hi, 2, 100_000, Some(after1)).await;
        let ps_lo = add_unthreaded_part(&db, thread_lo, 1, 250_000, Some(base)).await;
        assert_ne!(ps_hi, ps_lo, "the two parts must form separate patchsets");

        let ps_id = add_unthreaded_part(&db, thread_mid, 3, 175_000, Some(after2)).await;
        assert_eq!(ps_id, ps_hi, "the merge keeps the patchset created first");
        assert_eq!(
            patchset_baseline(&db, ps_id).await,
            Some(base),
            "the merged-away patchset's baseline must survive the merge"
        );
    }

    /// Verify that a commit SHA submitted as a singleton does NOT steal
    /// a patch from a range patchset in a different thread.
    ///
    /// Regression test for the clid_candidates fallback that matched
    /// across unrelated patchsets via the @sashiko.local suffix.
    #[tokio::test]
    async fn test_range_patch_not_stolen_by_singleton() {
        let db = setup_db().await;
        let author = "Akhil R <akhilrajeev@nvidia.com>";

        // Thread 1: single commit submission (sha_D).
        // For singletons, cover_letter_message_id = message_id.
        let t1 = db
            .create_thread("sha_D", "Single commit", 90000)
            .await
            .unwrap();
        db.create_message(
            "sha_D",
            t1,
            None,
            author,
            "i2c: tegra: Update timing",
            90000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps_single = db
            .create_patchset(
                t1,
                Some("sha_D"),
                "sha_D",
                "i2c: tegra: Update timing",
                author,
                90000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_single, "sha_D", 1, "diff-single")
            .await
            .unwrap();

        // Verify single patchset is full (1/1)
        let det = db
            .get_patchset_details(ps_single, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(det["received_parts"], 1);
        assert_eq!(det["total_parts"], 1);

        // Thread 2: range submission (A..D) with 4 commits.
        // The root message_id is the range itself.
        let t2 = db
            .create_thread("range_root", "Range submission", 90010)
            .await
            .unwrap();

        // Create the root/cover message for the range
        db.create_message(
            "range_root",
            t2,
            None,
            author,
            "Range A..D",
            90010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        // First patch establishes the range patchset
        db.create_message(
            "sha_A",
            t2,
            Some("range_root"),
            author,
            "[PATCH 1/4] Patch sha_A",
            90011,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps_range = db
            .create_patchset(
                t2,
                Some("range_root"),
                "sha_A",
                "[PATCH 1/4] Patch sha_A",
                author,
                90011,
                4,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_range, "sha_A", 1, "diff-sha_A")
            .await
            .unwrap();

        // Patches 2-3 merge into the range patchset
        for (i, sha) in ["sha_B", "sha_C"].iter().enumerate() {
            let idx = (i + 2) as u32;
            db.create_message(
                sha,
                t2,
                Some("range_root"),
                author,
                &format!("[PATCH {}/4] Patch {}", idx, sha),
                90010 + idx as i64,
                "",
                "",
                "",
                None,
                None,
            )
            .await
            .unwrap();

            let ps = db
                .create_patchset(
                    t2,
                    Some("range_root"),
                    sha,
                    &format!("[PATCH {}/4] Patch {}", idx, sha),
                    author,
                    90010 + idx as i64,
                    4,
                    0,
                    "",
                    "",
                    None,
                    idx,
                    None,
                    false,
                    None,
                    None,
                )
                .await
                .unwrap()
                .unwrap();

            assert_eq!(ps, ps_range, "Patch {}/4 should merge into range", idx);
            db.create_patch(ps, sha, idx, &format!("diff-{}", sha))
                .await
                .unwrap();
        }

        // Now patch 4/4: its message_id "sha_D_range" differs from
        // the singleton's "sha_D", but the @sashiko.local fallback
        // used to construct "sha_D_range@sashiko.local" which is
        // different enough. The real issue was when message_id was
        // literally the same SHA. Simulate that: message_id = "sha_D"
        // but in thread t2.
        db.create_message(
            "sha_D_in_range",
            t2,
            Some("range_root"),
            author,
            "[PATCH 4/4] i2c: tegra: Update timing",
            90014,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps_4 = db
            .create_patchset(
                t2,
                Some("range_root"),
                "sha_D_in_range",
                "[PATCH 4/4] i2c: tegra: Update timing",
                author,
                90014,
                4,
                0,
                "",
                "",
                None,
                4,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .expect("Patch 4/4 should merge into range, not be dropped");

        assert_eq!(
            ps_4, ps_range,
            "Patch 4/4 must merge into the range patchset, not the singleton"
        );

        db.create_patch(ps_4, "sha_D_in_range", 4, "diff-sha_D")
            .await
            .unwrap();

        // Range patchset should have all 4 patches
        let range_det = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            range_det["received_parts"], 4,
            "Range patchset should have all 4 patches"
        );

        // Singleton should still be untouched (1/1)
        let single_det = db
            .get_patchset_details(ps_single, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            single_det["received_parts"], 1,
            "Singleton patchset should still have exactly 1 patch"
        );
    }

    /// Count the actual patch rows owned by a patchset.
    async fn count_patches(db: &Database, patchset_id: i64) -> i64 {
        let mut rows = db
            .conn
            .query(
                "SELECT COUNT(*) FROM patches WHERE patchset_id = ?",
                libsql::params![patchset_id],
            )
            .await
            .unwrap();
        if let Ok(Some(row)) = rows.next().await {
            row.get(0).unwrap()
        } else {
            0
        }
    }

    /// Verify that create_patch does not steal a patch from one patchset
    /// to give it to another when both share the same message_id (SHA).
    ///
    /// Reproduces the bug where submitting a single commit then a range
    /// containing the same commit causes the singleton to drop to 0/1.
    #[tokio::test]
    async fn test_create_patch_no_cross_patchset_steal() {
        let db = setup_db().await;
        let author = "Test <test@example.com>";
        let shared_sha = "abcdef1234567890abcdef1234567890abcdef12";

        // Thread 1: singleton submission
        let t1 = db
            .create_thread("single_root", "Single", 80000)
            .await
            .unwrap();
        db.create_message(
            shared_sha,
            t1,
            None,
            author,
            "Fix something",
            80000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_single = db
            .create_patchset(
                t1,
                Some(shared_sha),
                shared_sha,
                "Fix something",
                author,
                80000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_single, shared_sha, 1, "diff-singleton")
            .await
            .unwrap();

        let det = db
            .get_patchset_details(ps_single, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(det["received_parts"], 1, "Singleton should have 1 patch");

        // Thread 2: range submission that includes the same SHA
        let t2 = db
            .create_thread("range_root", "Range", 80010)
            .await
            .unwrap();
        db.create_message(
            "range_root",
            t2,
            None,
            author,
            "Range cover",
            80010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            "sha_other",
            t2,
            Some("range_root"),
            author,
            "[PATCH 1/2] Other fix",
            80011,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_range = db
            .create_patchset(
                t2,
                Some("range_root"),
                "sha_other",
                "[PATCH 1/2] Other fix",
                author,
                80011,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_range, "sha_other", 1, "diff-other")
            .await
            .unwrap();

        // Patch 2/2 uses the SAME message_id as the singleton.
        db.create_message(
            shared_sha,
            t2,
            Some("range_root"),
            author,
            "[PATCH 2/2] Fix something",
            80012,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        // This should NOT steal the patch from ps_single.
        db.create_patch(ps_range, shared_sha, 2, "diff-range-copy")
            .await
            .unwrap();

        // Singleton must keep its patch
        let single_det = db
            .get_patchset_details(ps_single, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            single_det["received_parts"], 1,
            "Singleton must keep its patch (was stolen by range)"
        );
        // Verify the actual patch row still belongs to the singleton
        assert_eq!(
            count_patches(&db, ps_single).await,
            1,
            "Singleton must physically own its patch row"
        );

        // Range should have both patches
        let range_det = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            range_det["received_parts"], 2,
            "Range should have both patches"
        );
        assert_eq!(
            count_patches(&db, ps_range).await,
            2,
            "Range must physically own both patch rows"
        );

        // Resubmit the same range patch again (idempotent guard)
        db.create_patch(ps_range, shared_sha, 2, "diff-range-copy")
            .await
            .unwrap();

        // received_parts must not exceed total_parts
        let range_det2 = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            range_det2["received_parts"], 2,
            "Resubmission must not over-count received_parts"
        );
    }

    /// Same singleton submitted twice: create_patch should be idempotent
    /// via ON CONFLICT within the same patchset.
    #[tokio::test]
    async fn test_create_patch_same_singleton_twice() {
        let db = setup_db().await;
        let author = "Test <test@example.com>";

        let t1 = db.create_thread("root_dup", "Dup", 70000).await.unwrap();
        db.create_message(
            "sha_dup",
            t1,
            None,
            author,
            "Fix duplicate",
            70000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps = db
            .create_patchset(
                t1,
                Some("sha_dup"),
                "sha_dup",
                "Fix duplicate",
                author,
                70000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // First insert
        db.create_patch(ps, "sha_dup", 1, "diff-v1").await.unwrap();
        let det = db
            .get_patchset_details(ps, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(det["received_parts"], 1);

        // Second insert (same patchset, same message_id) -- idempotent
        db.create_patch(ps, "sha_dup", 1, "diff-v2").await.unwrap();
        let det = db
            .get_patchset_details(ps, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            det["received_parts"], 1,
            "Duplicate insert in same patchset must stay at 1"
        );
    }

    /// Same range submitted twice: each patch in the range should be
    /// idempotent via ON CONFLICT within the same patchset.
    #[tokio::test]
    async fn test_create_patch_same_range_twice() {
        let db = setup_db().await;
        let author = "Test <test@example.com>";

        let t1 = db
            .create_thread("range_dup_root", "Range dup", 71000)
            .await
            .unwrap();
        db.create_message(
            "range_dup_root",
            t1,
            None,
            author,
            "Range cover",
            71000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        for sha in ["sha_r1", "sha_r2"] {
            db.create_message(
                sha,
                t1,
                Some("range_dup_root"),
                author,
                &format!("[PATCH] {}", sha),
                71001,
                "",
                "",
                "",
                None,
                None,
            )
            .await
            .unwrap();
        }

        let ps = db
            .create_patchset(
                t1,
                Some("range_dup_root"),
                "sha_r1",
                "[PATCH 1/2] sha_r1",
                author,
                71001,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // First round
        db.create_patch(ps, "sha_r1", 1, "diff-r1").await.unwrap();
        db.create_patch(ps, "sha_r2", 2, "diff-r2").await.unwrap();
        let det = db
            .get_patchset_details(ps, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(det["received_parts"], 2);

        // Second round (duplicate range submission)
        db.create_patch(ps, "sha_r1", 1, "diff-r1").await.unwrap();
        db.create_patch(ps, "sha_r2", 2, "diff-r2").await.unwrap();
        let det = db
            .get_patchset_details(ps, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            det["received_parts"], 2,
            "Duplicate range in same patchset must stay at 2"
        );
    }

    /// Range submitted first, then singleton with shared SHA: neither
    /// should lose its patch.
    #[tokio::test]
    async fn test_create_patch_range_then_singleton() {
        let db = setup_db().await;
        let author = "Test <test@example.com>";
        let shared_sha = "shared_range_then_single";

        // Thread 1: range first
        let t1 = db
            .create_thread("rts_range_root", "Range first", 72000)
            .await
            .unwrap();
        db.create_message(
            "rts_range_root",
            t1,
            None,
            author,
            "Range cover",
            72000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            "rts_other",
            t1,
            Some("rts_range_root"),
            author,
            "[PATCH 1/2] Other",
            72001,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            shared_sha,
            t1,
            Some("rts_range_root"),
            author,
            "[PATCH 2/2] Shared",
            72002,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps_range = db
            .create_patchset(
                t1,
                Some("rts_range_root"),
                "rts_other",
                "[PATCH 1/2] Other",
                author,
                72001,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_range, "rts_other", 1, "diff-other")
            .await
            .unwrap();
        db.create_patch(ps_range, shared_sha, 2, "diff-shared")
            .await
            .unwrap();

        let range_det = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            range_det["received_parts"], 2,
            "Range should have 2 patches"
        );

        // Thread 2: singleton with the shared SHA
        let t2 = db
            .create_thread("rts_single", "Single after", 72010)
            .await
            .unwrap();
        db.create_message(
            shared_sha,
            t2,
            None,
            author,
            "Shared commit alone",
            72010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_single = db
            .create_patchset(
                t2,
                Some(shared_sha),
                shared_sha,
                "Shared commit alone",
                author,
                72010,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // This should NOT steal from the range
        db.create_patch(ps_single, shared_sha, 1, "diff-single")
            .await
            .unwrap();

        // Range must still have 2 patches
        let range_det = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            range_det["received_parts"], 2,
            "Range must keep both patches after singleton submission"
        );
        // Verify the range physically owns both patch rows
        assert_eq!(
            count_patches(&db, ps_range).await,
            2,
            "Range must physically own both patch rows"
        );

        // Singleton should have 1
        let single_det = db
            .get_patchset_details(ps_single, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            single_det["received_parts"], 1,
            "Singleton should have 1 patch"
        );
        assert_eq!(
            count_patches(&db, ps_single).await,
            1,
            "Singleton must physically own its patch row"
        );
    }

    /// 4-patch range where a shared commit is in the middle (not last):
    /// ensures that all 4 patches are inserted physically and received_parts is 4.
    #[tokio::test]
    async fn test_create_patch_range_shared_mid_sequence() {
        let db = setup_db().await;
        let author = "Test <test@example.com>";
        let shared_sha = "sha_mid_shared";

        // Thread 1: singleton with the shared SHA
        let t1 = db
            .create_thread("mid_single_root", "Single", 90000)
            .await
            .unwrap();
        db.create_message(
            shared_sha,
            t1,
            None,
            author,
            "Shared commit",
            90000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_single = db
            .create_patchset(
                t1,
                Some(shared_sha),
                shared_sha,
                "Shared commit",
                author,
                90000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_single, shared_sha, 1, "diff-single")
            .await
            .unwrap();

        let det = db
            .get_patchset_details(ps_single, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(det["received_parts"], 1, "Singleton should have 1 patch");

        // Thread 2: 4-patch range where shared_sha is patch 2 of 4
        let t2 = db
            .create_thread("mid_range_root", "Range", 90010)
            .await
            .unwrap();
        db.create_message(
            "mid_range_root",
            t2,
            None,
            author,
            "Range cover",
            90010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        for sha in ["sha_p1", shared_sha, "sha_p3", "sha_p4"] {
            db.create_message(
                sha,
                t2,
                Some("mid_range_root"),
                author,
                &format!("[PATCH] {}", sha),
                90011,
                "",
                "",
                "",
                None,
                None,
            )
            .await
            .ok(); // shared_sha message already exists, ignore dup
        }

        let ps_range = db
            .create_patchset(
                t2,
                Some("mid_range_root"),
                "sha_p1",
                "[PATCH 1/4] sha_p1",
                author,
                90011,
                4,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Insert patches in order: p1, shared (cross-patchset), p3, p4
        db.create_patch(ps_range, "sha_p1", 1, "diff-p1")
            .await
            .unwrap();
        let det = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            det["received_parts"], 1,
            "After p1: received_parts should be 1"
        );

        // Patch 2: shared SHA — cross-patchset, bumps received_parts
        db.create_patch(ps_range, shared_sha, 2, "diff-shared")
            .await
            .unwrap();
        let det = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            det["received_parts"], 2,
            "After shared: received_parts should be 2"
        );

        // Patch 3: normal insert — must NOT overwrite the bump
        db.create_patch(ps_range, "sha_p3", 3, "diff-p3")
            .await
            .unwrap();
        let det = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            det["received_parts"], 3,
            "After p3: received_parts should be 3 (COUNT bug would give 2)"
        );

        // Patch 4: normal insert — completes the range
        db.create_patch(ps_range, "sha_p4", 4, "diff-p4")
            .await
            .unwrap();
        let det = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            det["received_parts"], 4,
            "After p4: range must be complete with 4/4"
        );

        // Verify physical patch ownership: range has all 4 physical patches
        assert_eq!(
            count_patches(&db, ps_range).await,
            4,
            "Range should physically own all 4 patches"
        );

        // Singleton must still have its patch
        let single_det = db
            .get_patchset_details(ps_single, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            single_det["received_parts"], 1,
            "Singleton must keep its patch"
        );
        assert_eq!(
            count_patches(&db, ps_single).await,
            1,
            "Singleton must physically own its patch row"
        );

        // Status check: range should be Pending (complete)
        assert!(
            det["status"] == "Pending" || det["status"] == "In Review",
            "Range status should transition to Pending/In Review, got: {}",
            det["status"]
        );
    }

    /// Verify that when a commit SHA is shared across multiple patchsets
    /// (e.g. submitted as a singleton and then in a range resubmission),
    /// both patchsets physically own their patch rows in the database,
    /// so that get_patch_diffs and get_patchset_details return all patches
    /// for both patchsets.
    #[tokio::test]
    async fn test_cross_patchset_shared_commit_physical_rows_and_diffs() {
        let db = setup_db().await;
        let author = "Author <author@example.com>";
        let shared_sha = "shared_commit_sha_1234567890";
        let other_sha = "other_commit_sha_0987654321";

        // 1. Thread 1: Singleton patchset with shared_sha
        let t1 = db
            .create_thread("t1_singleton", "Singleton Subject", 1000)
            .await
            .unwrap();
        db.create_message(
            shared_sha,
            t1,
            None,
            author,
            "Singleton commit",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_single = db
            .create_patchset(
                t1,
                Some(shared_sha),
                shared_sha,
                "Singleton commit",
                author,
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_single, shared_sha, 1, "diff-shared-v1")
            .await
            .unwrap();

        // 2. Thread 2: 2-patch range containing other_sha (part 1) and shared_sha (part 2)
        let t2 = db
            .create_thread("t2_range_cover", "Range Subject", 2000)
            .await
            .unwrap();
        db.create_message(
            "t2_range_cover",
            t2,
            None,
            author,
            "Range Cover Letter",
            2000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            other_sha,
            t2,
            Some("t2_range_cover"),
            author,
            "[PATCH 1/2] Other commit",
            2001,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            shared_sha,
            t2,
            Some("t2_range_cover"),
            author,
            "[PATCH 2/2] Shared commit",
            2002,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps_range = db
            .create_patchset(
                t2,
                Some("t2_range_cover"),
                other_sha,
                "[PATCH 1/2] Other commit",
                author,
                2001,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_range, other_sha, 1, "diff-other")
            .await
            .unwrap();
        db.create_patch(ps_range, shared_sha, 2, "diff-shared-v2")
            .await
            .unwrap();

        // Check singleton: must have 1 physical patch row and 1 diff
        assert_eq!(
            count_patches(&db, ps_single).await,
            1,
            "Singleton must physically own 1 patch row"
        );
        let single_diffs = db.get_patch_diffs(ps_single).await.unwrap();
        assert_eq!(
            single_diffs.len(),
            1,
            "Singleton must have 1 patch diff returned by get_patch_diffs"
        );
        assert_eq!(single_diffs[0].6, shared_sha);

        let single_det = db
            .get_patchset_details(ps_single, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            single_det["patches"].as_array().unwrap().len(),
            1,
            "Singleton details must contain 1 patch"
        );

        // Check range: must have 2 physical patch rows and 2 diffs
        assert_eq!(
            count_patches(&db, ps_range).await,
            2,
            "Range must physically own 2 patch rows"
        );
        let range_diffs = db.get_patch_diffs(ps_range).await.unwrap();
        assert_eq!(
            range_diffs.len(),
            2,
            "Range must have 2 patch diffs returned by get_patch_diffs (including shared SHA)"
        );
        assert_eq!(range_diffs[0].6, other_sha);
        assert_eq!(range_diffs[1].6, shared_sha);

        let range_det = db
            .get_patchset_details(ps_range, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            range_det["patches"].as_array().unwrap().len(),
            2,
            "Range details must contain 2 patches"
        );
    }

    /// Verify the reverse ingestion order: range first, then singleton with
    /// the shared commit. The singleton must physically own its patch row
    /// and return its diff.
    #[tokio::test]
    async fn test_cross_patchset_shared_commit_range_then_singleton() {
        let db = setup_db().await;
        let author = "Author <author@example.com>";
        let shared_sha = "shared_commit_rev_1234567890";
        let other_sha = "other_commit_rev_0987654321";

        // 1. Thread 1: 2-patch range containing other_sha (part 1) and shared_sha (part 2)
        let t1 = db
            .create_thread("t1_range_cover", "Range Subject", 1000)
            .await
            .unwrap();
        db.create_message(
            "t1_range_cover",
            t1,
            None,
            author,
            "Range Cover Letter",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            other_sha,
            t1,
            Some("t1_range_cover"),
            author,
            "[PATCH 1/2] Other commit",
            1001,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            shared_sha,
            t1,
            Some("t1_range_cover"),
            author,
            "[PATCH 2/2] Shared commit",
            1002,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps_range = db
            .create_patchset(
                t1,
                Some("t1_range_cover"),
                other_sha,
                "[PATCH 1/2] Other commit",
                author,
                1001,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_range, other_sha, 1, "diff-other")
            .await
            .unwrap();
        db.create_patch(ps_range, shared_sha, 2, "diff-shared-v1")
            .await
            .unwrap();

        // 2. Thread 2: Singleton patchset with shared_sha
        let t2 = db
            .create_thread("t2_singleton", "Singleton Subject", 2000)
            .await
            .unwrap();
        db.create_message(
            shared_sha,
            t2,
            None,
            author,
            "Singleton commit",
            2000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_single = db
            .create_patchset(
                t2,
                Some(shared_sha),
                shared_sha,
                "Singleton commit",
                author,
                2000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_patch(ps_single, shared_sha, 1, "diff-shared-v2")
            .await
            .unwrap();

        // Check range: must have 2 physical patch rows and 2 diffs
        assert_eq!(
            count_patches(&db, ps_range).await,
            2,
            "Range must physically own 2 patch rows"
        );
        let range_diffs = db.get_patch_diffs(ps_range).await.unwrap();
        assert_eq!(
            range_diffs.len(),
            2,
            "Range must have 2 patch diffs returned by get_patch_diffs"
        );

        // Check singleton: must have 1 physical patch row and 1 diff
        assert_eq!(
            count_patches(&db, ps_single).await,
            1,
            "Singleton must physically own 1 patch row"
        );
        let single_diffs = db.get_patch_diffs(ps_single).await.unwrap();
        assert_eq!(
            single_diffs.len(),
            1,
            "Singleton must have 1 patch diff returned by get_patch_diffs"
        );
        assert_eq!(single_diffs[0].6, shared_sha);

        let single_det = db
            .get_patchset_details(ps_single, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            single_det["patches"].as_array().unwrap().len(),
            1,
            "Singleton details must contain 1 patch"
        );
    }

    /// Verify that get_patchset_details_by_msgid and get_patchset_summary_by_msgid
    /// resolve synthetic IDs when queried by bare SHA.
    #[tokio::test]
    async fn test_get_patchset_details_by_synthetic_msgid() {
        let db = setup_db().await;
        let sha = "a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4";
        let synthetic_id = format!("{}@sashiko.local", sha);

        // Create a fetching patchset with the synthetic cover_letter_message_id
        let ps_id = db
            .create_fetching_patchset(
                &synthetic_id,
                "Fetching patchset subject",
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        // 1. Querying with the bare SHA must find the patchset details
        let details = db
            .get_patchset_details_by_msgid(sha, None, None)
            .await
            .unwrap();
        assert!(
            details.is_some(),
            "get_patchset_details_by_msgid with bare SHA should resolve patchset with @sashiko.local cover letter"
        );
        assert_eq!(details.unwrap()["id"], ps_id);

        // 2. Querying with bracketed SHA must also find the patchset details
        let details_bracketed = db
            .get_patchset_details_by_msgid(&format!("<{}>", sha), None, None)
            .await
            .unwrap();
        assert_eq!(details_bracketed.unwrap()["id"], ps_id);

        // 3. Querying with full synthetic ID must also find the patchset details
        let details_full = db
            .get_patchset_details_by_msgid(&synthetic_id, None, None)
            .await
            .unwrap();
        assert_eq!(details_full.unwrap()["id"], ps_id);

        // 4. Querying with the bare SHA must also find the patchset summary
        let summary = db
            .get_patchset_summary_by_msgid(sha, None, None)
            .await
            .unwrap();
        assert!(
            summary.is_some(),
            "get_patchset_summary_by_msgid with bare SHA should resolve patchset with @sashiko.local cover letter"
        );
        assert_eq!(summary.unwrap()["id"], ps_id);

        // 5. update_patchset_error with bare SHA updates the synthetic patchset
        db.update_patchset_error(sha, "Fetch failed: timeout")
            .await
            .unwrap();
        let updated = db
            .get_patchset_details_by_msgid(sha, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated["status"], "Failed");
        assert_eq!(updated["failed_reason"], "Fetch failed: timeout");
    }

    /// Verify that has_patchset_by_msgid detects patchsets by bare SHA
    /// for synthetic @sashiko.local cover letters and for patches in the patches table.
    #[tokio::test]
    async fn test_has_patchset_by_msgid_synthetic_and_patch_sha() {
        let db = setup_db().await;
        let sha_fetching = "deadbeef1234567890abcdef1234567890abcdef";
        let synthetic_id = format!("{}@sashiko.local", sha_fetching);

        // 1. Placeholder patchset in Fetching state with @sashiko.local cover letter
        db.create_fetching_patchset(
            &synthetic_id,
            "Fetching placeholder",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // Must return true when queried by bare SHA
        assert!(
            db.has_patchset_by_msgid(sha_fetching).await.unwrap(),
            "has_patchset_by_msgid should return true for bare SHA matching @sashiko.local cover letter"
        );

        // 2. Patchset with physical patch row
        let sha_patch = "c0ffee1234567890abcdef1234567890abcdef";
        let t = db
            .create_thread("t_thread", "Test Thread", 1000)
            .await
            .unwrap();
        db.create_message(
            "cover_letter@example.com",
            t,
            None,
            "Author <a@example.com>",
            "Cover Subject",
            999,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            sha_patch,
            t,
            Some("cover_letter@example.com"),
            "Author <a@example.com>",
            "Patch Subject",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps = db
            .create_patchset(
                t,
                Some("cover_letter@example.com"),
                sha_patch,
                "Patch Subject",
                "Author <a@example.com>",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        db.create_patch(ps, sha_patch, 1, "diff-patch")
            .await
            .unwrap();

        // Must return true when queried by the patch's SHA even if cover letter is different
        assert!(
            db.has_patchset_by_msgid(sha_patch).await.unwrap(),
            "has_patchset_by_msgid should return true for SHA existing in patches table"
        );
    }

    /// Verify that has_patchset_by_msgid returns false for Failed and Cancelled
    /// patchsets so that retry submissions can be re-fetched.
    #[tokio::test]
    async fn test_has_patchset_by_msgid_excludes_failed_and_cancelled() {
        let db = setup_db().await;
        let sha_failed = "f00f00f001234567890abcdef1234567890abcdef";
        let synthetic_failed = format!("{}@sashiko.local", sha_failed);

        // 1. Create a patchset and mark it Failed
        let _ps_failed = db
            .create_fetching_patchset(
                &synthetic_failed,
                "Fetching failed placeholder",
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        db.update_patchset_error(sha_failed, "Remote host unreachable")
            .await
            .unwrap();

        // Must return false for Failed patchset to allow retry
        assert!(
            !db.has_patchset_by_msgid(sha_failed).await.unwrap(),
            "has_patchset_by_msgid should return false for Failed patchset to allow retry"
        );

        // 2. Create a patchset and mark it Cancelled
        let sha_cancelled = "c00c00c001234567890abcdef1234567890abcdef";
        let synthetic_cancelled = format!("{}@sashiko.local", sha_cancelled);
        let ps_cancelled = db
            .create_fetching_patchset(
                &synthetic_cancelled,
                "Fetching cancelled placeholder",
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        db.update_patchset_status(ps_cancelled, "Cancelled")
            .await
            .unwrap();

        // Must return false for Cancelled patchset to allow retry
        assert!(
            !db.has_patchset_by_msgid(sha_cancelled).await.unwrap(),
            "has_patchset_by_msgid should return false for Cancelled patchset to allow retry"
        );

        // 3. Create a patchset with a physical patch row, then mark patchset Failed
        let sha_failed_patch = "a11a11a111234567890abcdef1234567890abcdef";
        let t_failed = db
            .create_thread("t_f", "Failed thread", 2000)
            .await
            .unwrap();
        db.create_message(
            "cover_failed@example.com",
            t_failed,
            None,
            "Author <a@example.com>",
            "Failed Subject",
            2000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            sha_failed_patch,
            t_failed,
            Some("cover_failed@example.com"),
            "Author <a@example.com>",
            "Failed Patch Subject",
            2001,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_f = db
            .create_patchset(
                t_failed,
                Some("cover_failed@example.com"),
                sha_failed_patch,
                "Failed Patch Subject",
                "Author <a@example.com>",
                2001,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        db.create_patch(ps_f, sha_failed_patch, 1, "diff-fail")
            .await
            .unwrap();
        db.update_patchset_status(ps_f, "Failed").await.unwrap();

        // Must return false when queried by the patch's SHA because its patchset is Failed
        assert!(
            !db.has_patchset_by_msgid(sha_failed_patch).await.unwrap(),
            "has_patchset_by_msgid should return false for patch SHA belonging to Failed patchset"
        );

        // 4. Create a patchset with a physical patch row, then mark patchset Cancelled
        let sha_cancelled_patch = "b22b22b221234567890abcdef1234567890abcdef";
        let t_cancelled = db
            .create_thread("t_c", "Cancelled thread", 3000)
            .await
            .unwrap();
        db.create_message(
            "cover_cancelled@example.com",
            t_cancelled,
            None,
            "Author <a@example.com>",
            "Cancelled Subject",
            3000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            sha_cancelled_patch,
            t_cancelled,
            Some("cover_cancelled@example.com"),
            "Author <a@example.com>",
            "Cancelled Patch Subject",
            3001,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_c = db
            .create_patchset(
                t_cancelled,
                Some("cover_cancelled@example.com"),
                sha_cancelled_patch,
                "Cancelled Patch Subject",
                "Author <a@example.com>",
                3001,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        db.create_patch(ps_c, sha_cancelled_patch, 1, "diff-cancel")
            .await
            .unwrap();
        db.update_patchset_status(ps_c, "Cancelled").await.unwrap();

        // Must return false when queried by the patch's SHA because its patchset is Cancelled
        assert!(
            !db.has_patchset_by_msgid(sha_cancelled_patch).await.unwrap(),
            "has_patchset_by_msgid should return false for patch SHA belonging to Cancelled patchset"
        );

        // 5. Create a patchset with a physical patch row, then mark patchset Failed To Apply
        let sha_fta_patch = "c33c33c331234567890abcdef1234567890abcdef";
        let t_fta = db
            .create_thread("t_fta", "Failed to apply thread", 4000)
            .await
            .unwrap();
        db.create_message(
            "cover_fta@example.com",
            t_fta,
            None,
            "Author <a@example.com>",
            "FTA Subject",
            4000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            sha_fta_patch,
            t_fta,
            Some("cover_fta@example.com"),
            "Author <a@example.com>",
            "FTA Patch Subject",
            4001,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_fta = db
            .create_patchset(
                t_fta,
                Some("cover_fta@example.com"),
                sha_fta_patch,
                "FTA Patch Subject",
                "Author <a@example.com>",
                4001,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        db.create_patch(ps_fta, sha_fta_patch, 1, "diff-fta")
            .await
            .unwrap();
        db.update_patchset_status(ps_fta, "Failed To Apply")
            .await
            .unwrap();

        // Must return false when queried by cover letter and by patch SHA
        assert!(
            !db.has_patchset_by_msgid("cover_fta@example.com")
                .await
                .unwrap(),
            "has_patchset_by_msgid should return false for cover letter of Failed To Apply patchset"
        );
        assert!(
            !db.has_patchset_by_msgid(sha_fta_patch).await.unwrap(),
            "has_patchset_by_msgid should return false for patch SHA belonging to Failed To Apply patchset"
        );
    }

    /// Verify that create_fetching_patchset resets patchsets in 'Failed To Apply'
    /// status to 'Fetching' so that re-submitted patches can transition to 'Pending'.
    #[tokio::test]
    async fn test_failed_to_apply_patchset_resettable_and_reingestible() {
        let db = setup_db().await;
        let sha = "d44d44d441234567890abcdef1234567890abcdef";
        let synthetic_cover = format!("{}@sashiko.local", sha);

        // 1. Create a placeholder and ingest the patch
        let ps_id = db
            .create_fetching_patchset(
                &synthetic_cover,
                "Fetching initial",
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let t = db
            .create_thread("t_reingest", "Thread", 5000)
            .await
            .unwrap();
        db.create_message(
            sha,
            t,
            Some(&synthetic_cover),
            "Author <a@example.com>",
            "Patch Subject",
            5000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps_created = db
            .create_patchset(
                t,
                Some(&synthetic_cover),
                sha,
                "Patch Subject",
                "Author <a@example.com>",
                5000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ps_id, ps_created);

        db.create_patch(ps_id, sha, 1, "diff-v1").await.unwrap();

        // 2. Mark patchset Failed To Apply during review
        db.update_patchset_status(ps_id, "Failed To Apply")
            .await
            .unwrap();

        let det = db
            .get_patchset_details(ps_id, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(det["status"], "Failed To Apply");

        // 3. User re-submits: create_fetching_patchset must reset status to Fetching
        let ps_re_id = db
            .create_fetching_patchset(
                &synthetic_cover,
                "Fetching retry",
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(ps_re_id, ps_id);

        let det_after_fetch = db
            .get_patchset_details(ps_id, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(det_after_fetch["status"], "Fetching");

        // 4. Ingestion re-runs create_patchset and create_patch
        let ps_reingested = db
            .create_patchset(
                t,
                Some(&synthetic_cover),
                sha,
                "Patch Subject Retry",
                "Author <a@example.com>",
                5010,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ps_reingested, ps_id);

        db.create_patch(ps_id, sha, 1, "diff-v2").await.unwrap();

        // Patchset must now successfully be in Pending status (ready for review)
        let det_final = db
            .get_patchset_details(ps_id, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(det_final["status"], "Pending");
    }

    /// Verify that an existing database at user_version = 1 with the legacy
    /// UNIQUE(message_id) constraint is automatically migrated by db.migrate()
    /// without requiring a user_version bump.
    #[tokio::test]
    async fn test_bug_crud_and_links() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let bug = NewBug {
            bugid: "linux-test-1234".to_string(),
            title: "Memory leak in e1000_probe()".to_string(),
            lifecycle_status: BugLifecycleStatus::Open,
            pipeline_state: BugPipelineState::Succeeded,
            assignee: None,
            reporter: "sashiko".to_string(),
            reported_at: 123456789,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: Some("abc1234".to_string()),
            source_ref: None,
            vector_json: Some("[0.1, 0.2, 0.3]".to_string()),
            duplicate_of_id: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net/core")],
        };

        let bug_id = db.create_bug(&bug).await.unwrap();
        assert!(bug_id > 0);

        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "verification".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 123456790,
                content: None,
                data_json: Some(json!({
                    "is_valid": true,
                    "locations": [{"file": "drivers/net/e1000.c", "line": 42}],
                    "source_files": ["drivers/net/e1000.c"],
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "origin_discovery".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 123456791,
                content: Some("11223344 (net: e1000: add probe)".to_string()),
                data_json: Some(json!({
                    "introducing_commit_sha": "11223344 (net: e1000: add probe)",
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "severity_calibration".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 123456792,
                content: Some("Buffer is allocated but not freed on error path".to_string()),
                data_json: Some(json!({
                    "severity": "High",
                    "severity_int": 3,
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "report".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 123456793,
                content: Some("> problematic_code();\nMemory is leaked here.".to_string()),
                data_json: Some(json!({
                    "format": "lkml_markdown",
                })),
                tokens_in: Some(100),
                tokens_out: Some(50),
                tokens_cached: Some(25),
                logs: Some("[{\"role\":\"system\",\"content\":\"test system\"}]".to_string()),
            },
        )
        .await
        .unwrap();

        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "fix_candidate".to_string(),
                tool: "human".to_string(),
                model: None,
                author: Some("Developer".to_string()),
                created_at: 123456794,
                content: None,
                data_json: Some(json!({
                    "status": "merged",
                    "commit_sha": "55667788 (net: e1000: fix leak in probe)",
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Fetch by id
        let fetched = db.get_bug(bug_id).await.unwrap().expect("Bug should exist");
        assert_eq!(fetched.bugid, "linux-test-1234");
        assert_eq!(fetched.slug(), "linux-test-1234");
        assert_eq!(fetched.problem(), "Memory leak in e1000_probe()");
        assert_eq!(fetched.severity(), Severity::High);
        assert_eq!(fetched.subsystems, vec!["net/core".to_string()]);
        assert_eq!(
            fetched.introduced_in_commit().as_deref(),
            Some("11223344 (net: e1000: add probe)")
        );
        assert!(fetched.is_fixed());
        assert_eq!(
            fetched.fixed_in_commit().as_deref(),
            Some("55667788 (net: e1000: fix leak in probe)")
        );
        assert_eq!(
            fetched.inline_review(),
            "> problematic_code();\nMemory is leaked here."
        );
        assert_eq!(fetched.tokens_in(), 100);
        assert_eq!(fetched.tokens_out(), 50);
        assert_eq!(fetched.tokens_cached(), 25);
        assert_eq!(
            fetched
                .enrichments
                .iter()
                .filter(|e| e.kind != "audit")
                .count(),
            5
        );

        // Fetch by bugid and slug
        let fetched_bugid = db
            .get_bug_by_bugid("linux-test-1234")
            .await
            .unwrap()
            .expect("Bug should exist");
        assert_eq!(fetched_bugid.id, bug_id);
        let fetched_slug = db
            .get_bug_by_slug("linux-test-1234")
            .await
            .unwrap()
            .expect("Bug should exist");
        assert_eq!(fetched_slug.id, bug_id);
        assert_eq!(
            fetched_slug.introduced_in_commit().as_deref(),
            Some("11223344 (net: e1000: add probe)")
        );
        assert!(fetched_slug.is_fixed());
        assert_eq!(
            fetched_slug.fixed_in_commit().as_deref(),
            Some("55667788 (net: e1000: fix leak in probe)")
        );

        // List bugs with search and filter (logs omitted)
        let (list, total) = db
            .list_bugs(ListBugsParams {
                page: Some(1),
                limit: Some(10),
                min_severity: Some(Severity::High),
                subsystem: Some("net"),
                lifecycle_status: Some(BugLifecycleStatus::Open),
                search: Some("e1000"),
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, bug_id);

        // Test hierarchical subsystem matching: "net" should match "net/core"
        let (list_hier, total_hier) = db
            .list_bugs(ListBugsParams {
                page: Some(1),
                limit: Some(10),
                subsystem: Some("net"),
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total_hier, 1);
        assert_eq!(list_hier.len(), 1);

        // Substring that is not a prefix should NOT match (e.g. "cor" shouldn't match "net/core")
        let (_, total_no_match) = db
            .list_bugs(ListBugsParams {
                page: Some(1),
                limit: Some(10),
                subsystem: Some("cor"),
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total_no_match, 0);

        // Test status mismatch filter
        let (list_closed, total_closed) = db
            .list_bugs(ListBugsParams {
                page: Some(1),
                limit: Some(10),
                lifecycle_status: Some(BugLifecycleStatus::Dismissed),
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total_closed, 0);
        assert_eq!(list_closed.len(), 0);

        // Test min_severity filter (Critical is higher than High)
        let (list_crit, total_crit) = db
            .list_bugs(ListBugsParams {
                page: Some(1),
                limit: Some(10),
                min_severity: Some(Severity::Critical),
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total_crit, 0);
        assert_eq!(list_crit.len(), 0);

        // Test sorting
        let (list_sorted, _) = db
            .list_bugs(ListBugsParams {
                page: Some(1),
                limit: Some(10),
                sort_by: Some("severity"),
                sort_order: Some("asc"),
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(list_sorted.len(), 1);

        let (list_disc, _) = db
            .list_bugs(ListBugsParams {
                page: Some(1),
                limit: Some(10),
                sort_by: Some("discoveries"),
                sort_order: Some("desc"),
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(list_disc.len(), 1);

        // Test duplicate linking
        let dup_bug = NewBug {
            bugid: "linux-dup-1".to_string(),
            title: "Duplicate of e1000 leak".to_string(),
            lifecycle_status: BugLifecycleStatus::New,
            pipeline_state: BugPipelineState::Pending,
            assignee: None,
            reporter: "sashiko".to_string(),
            reported_at: 100005,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net/core")],
        };
        let dup_id = db.create_bug(&dup_bug).await.unwrap();
        let dup_params = MarkDuplicateBugParams {
            preserve_triage: false,
            ephemeral_id: dup_id,
            canonical_id: bug_id,
            reasoning: "Duplicate issue",
            logs: None,
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
        };
        db.mark_bug_as_duplicate(dup_params).await.unwrap();

        let duplicates = db.list_duplicates_for_bug(bug_id).await.unwrap();
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].id, dup_id);
        assert_eq!(duplicates[0].duplicate_of_id, Some(bug_id));

        // Dedicated log retrieval
        let logs_str = db.get_bug_logs(bug_id).await.unwrap().unwrap();
        assert!(logs_str.contains("test system"));
        let logs_parsed: serde_json::Value = serde_json::from_str(&logs_str).unwrap();
        assert_eq!(logs_parsed[0]["role"], "system");
        assert_eq!(logs_parsed[0]["content"], "test system");

        assert_eq!(
            db.get_bug_logs_by_bugid("linux-test-1234").await.unwrap(),
            Some(logs_str.clone())
        );
        assert_eq!(
            db.get_bug_logs_by_slug("linux-test-1234").await.unwrap(),
            Some(logs_str)
        );

        // Link to review
        let thread_id = db.create_thread("t1", "subj", 100).await.unwrap();
        let ps_id = db
            .create_patchset(
                thread_id, None, "m1", "subj", "auth", 100, 1, 0, "", "", None, 1, None, false,
                None, None,
            )
            .await
            .unwrap()
            .unwrap();
        let review_id = db
            .create_review(ps_id, None, "gemini", "model", None, None)
            .await
            .unwrap();

        db.link_review_to_bug(review_id, bug_id, true)
            .await
            .unwrap();

        let review_bugs = db.list_bugs_for_review(review_id).await.unwrap();
        assert_eq!(review_bugs.len(), 1);
        assert_eq!(review_bugs[0].0.id, bug_id);
        assert!(review_bugs[0].1); // is_newly_discovered == true

        let ps_bugs = db.list_bugs_for_patchset(ps_id).await.unwrap();
        assert_eq!(ps_bugs.len(), 1);
        assert_eq!(ps_bugs[0].0.id, bug_id);

        let all_bugs = db.list_all_bugs_for_vector_search().await.unwrap();
        assert_eq!(all_bugs.len(), 1);
        assert_eq!(all_bugs[0].id, bug_id);
    }

    /// Builds a bug that is ready to be claimed for analysis.
    async fn create_pending_bug(db: &Database, bugid: &str) -> i64 {
        db.create_bug(&NewBug {
            bugid: bugid.to_string(),
            title: "Memory leak on crash".to_string(),
            lifecycle_status: BugLifecycleStatus::New,
            pipeline_state: BugPipelineState::Pending,
            assignee: None,
            reporter: "sashiko".to_string(),
            reported_at: 100000,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("kernel")],
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_migration_retires_folded_bugs_left_in_the_pipeline() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let canonical = create_pending_bug(&db, "linux-canonical").await;
        let folded = create_pending_bug(&db, "linux-folded").await;

        // Reproduce what folding used to leave behind: the triage state says
        // the bug is a duplicate while the pipeline still says it is running.
        db.conn
            .execute(
                "UPDATE bugs
                    SET lifecycle_status = 'duplicate',
                        duplicate_of_id = ?1,
                        pipeline_state = 'running',
                        locked_by = 'dead-worker:1',
                        lease_expires_at = NULL
                  WHERE id = ?2",
                libsql::params![canonical, folded],
            )
            .await
            .unwrap();
        db.conn
            .execute("PRAGMA user_version = 3", ())
            .await
            .unwrap();

        db.migrate().await.unwrap();

        let mut rows = db
            .conn
            .query(
                "SELECT pipeline_state, locked_by FROM bugs WHERE id = ?",
                libsql::params![folded],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "succeeded");
        assert!(row.get::<Option<String>>(1).unwrap().is_none());

        // A healed row is out of reach of the claim query, which is the whole
        // point: it must not be analysed again to rediscover a finding the
        // canonical bug already carries.
        let claimed = db.claim_pending_bug("worker:1", 300, 3).await.unwrap();
        assert_eq!(claimed.map(|b| b.id), Some(canonical));
    }

    #[tokio::test]
    async fn test_recover_stale_running_bugs() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_pending_bug(&db, "linux-crash-recovery").await;

        // Claiming a bug moves it onto the running pipeline state and records
        // who holds the lease.
        let locked = db
            .claim_pending_bug("worker-a", 3600, 10)
            .await
            .unwrap()
            .expect("should claim bug");
        assert_eq!(locked.id, bug_id);
        assert_eq!(locked.pipeline_state, BugPipelineState::Running);

        // A second worker must not get the same bug while the lease is live.
        assert!(
            db.claim_pending_bug("worker-b", 3600, 10)
                .await
                .unwrap()
                .is_none()
        );

        // An unexpired lease is not disturbed by recovery.
        assert_eq!(db.recover_stale_running_bugs().await.unwrap(), 0);

        // Expire the lease, as if the worker holding it had died.
        db.conn
            .execute(
                "UPDATE bugs SET lease_expires_at = 1 WHERE id = ?",
                libsql::params![bug_id],
            )
            .await
            .unwrap();

        assert_eq!(db.recover_stale_running_bugs().await.unwrap(), 1);

        // Recovery only rewinds the pipeline axis, leaving triage untouched.
        let rewound = db.get_bug(bug_id).await.unwrap().unwrap();
        assert_eq!(rewound.pipeline_state, BugPipelineState::Pending);
        assert_eq!(rewound.lifecycle_status, BugLifecycleStatus::New);

        let claimed_again = db
            .claim_pending_bug("worker-b", 3600, 10)
            .await
            .unwrap()
            .expect("should re-claim recovered bug");
        assert_eq!(claimed_again.id, bug_id);
        assert_eq!(claimed_again.pipeline_state, BugPipelineState::Running);
    }

    /// The dedup stage folds a bug while it is still claimed, and the caller
    /// then only drops the lease. Unless the fold itself retires the pipeline,
    /// the row keeps a 'running' state with no lease, which no recovery query
    /// can match, and the bug is stranded for good.
    #[tokio::test]
    async fn test_mark_duplicate_retires_running_pipeline() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let canonical_id = create_pending_bug(&db, "linux-canonical").await;
        let dup_id = create_pending_bug(&db, "linux-folded").await;

        // Reproduce the worker's sequence: claim the bug, fold it, release.
        // Claim ordering between two bugs created in the same second is not
        // guaranteed, so drain the queue rather than assuming which comes back.
        let mut claimed_dup = false;
        while let Some(bug) = db.claim_pending_bug("worker-a", 3600, 10).await.unwrap() {
            if bug.id == dup_id {
                assert_eq!(bug.pipeline_state, BugPipelineState::Running);
                claimed_dup = true;
            }
        }
        assert!(claimed_dup, "the bug being folded must have been claimed");

        db.mark_bug_as_duplicate(MarkDuplicateBugParams {
            ephemeral_id: dup_id,
            canonical_id,
            reasoning: "Same root cause",
            ..Default::default()
        })
        .await
        .unwrap();
        db.release_bug_lease(dup_id).await.unwrap();

        let folded = db.get_bug(dup_id).await.unwrap().unwrap();
        assert_eq!(folded.lifecycle_status, BugLifecycleStatus::Duplicate);
        assert_eq!(
            folded.pipeline_state,
            BugPipelineState::Succeeded,
            "a folded bug owes no further analysis"
        );

        // The decisive check: nothing is left for recovery to find, and the row
        // is not silently waiting on a lease that will never expire.
        assert_eq!(db.recover_stale_running_bugs().await.unwrap(), 0);
        let running_without_lease: i64 = db
            .conn
            .query(
                "SELECT count(*) FROM bugs WHERE pipeline_state = 'running' AND lease_expires_at IS NULL",
                (),
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(running_without_lease, 0);
    }

    /// A lease that is absent rather than expired must still be reclaimable.
    /// NULL never satisfies `lease_expires_at < now`, so a predicate written
    /// only against expiry leaves such a row running for good.
    #[tokio::test]
    async fn test_recovery_reclaims_running_bug_with_no_lease() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_pending_bug(&db, "linux-leaseless").await;
        db.claim_pending_bug("worker-a", 3600, 10)
            .await
            .unwrap()
            .expect("should claim bug");

        // Drop the lease but leave the pipeline running, which is what a
        // release that forgets the state leaves behind.
        db.release_bug_lease(bug_id).await.unwrap();
        let stranded = db.get_bug(bug_id).await.unwrap().unwrap();
        assert_eq!(stranded.pipeline_state, BugPipelineState::Running);
        // The lease is not projected onto the read model, so read the column.
        let lease: Option<i64> = db
            .conn
            .query(
                "SELECT lease_expires_at FROM bugs WHERE id = ?",
                libsql::params![bug_id],
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap()
            .get(0)
            .unwrap();
        assert!(lease.is_none());

        assert_eq!(db.recover_stale_running_bugs().await.unwrap(), 1);
        let recovered = db.get_bug(bug_id).await.unwrap().unwrap();
        assert_eq!(recovered.pipeline_state, BugPipelineState::Pending);

        // Claiming must reach the same row even without the sweep above.
        db.release_bug_lease(bug_id).await.unwrap();
        db.conn
            .execute(
                "UPDATE bugs SET pipeline_state = 'running' WHERE id = ?",
                libsql::params![bug_id],
            )
            .await
            .unwrap();
        let reclaimed = db
            .claim_pending_bug("worker-b", 3600, 10)
            .await
            .unwrap()
            .expect("a running bug with no lease must be claimable");
        assert_eq!(reclaimed.id, bug_id);
    }

    /// A folded bug is a tombstone. Re-analysing it would spend the budget to
    /// rediscover a finding that already lives on the canonical bug.
    #[tokio::test]
    async fn test_claim_skips_folded_bugs() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let canonical_id = create_pending_bug(&db, "linux-tombstone-canonical").await;
        let dup_id = create_pending_bug(&db, "linux-tombstone-dup").await;
        db.mark_bug_as_duplicate(MarkDuplicateBugParams {
            ephemeral_id: dup_id,
            canonical_id,
            reasoning: "Same root cause",
            ..Default::default()
        })
        .await
        .unwrap();

        // Drain everything the worker would ever be offered.
        let mut claimed = Vec::new();
        while let Some(bug) = db.claim_pending_bug("worker-a", 3600, 10).await.unwrap() {
            claimed.push(bug.id);
        }
        assert!(
            claimed.contains(&canonical_id),
            "the canonical bug still needs analysis"
        );
        assert!(
            !claimed.contains(&dup_id),
            "a folded bug must never be handed to a worker"
        );
    }

    #[tokio::test]
    async fn failed_outcome_write_rolls_back_every_result_and_remains_recoverable() {
        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".into(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let id = create_pending_bug(&db, "linux-atomic-result").await;
        db.claim_pending_bug("worker", 300, 3).await.unwrap();
        let before = serde_json::to_value(db.get_bug(id).await.unwrap().unwrap()).unwrap();
        // Fail late, after title, subsystem, vector, verification and severity
        // writes would already have happened in the former implementation.
        db.conn
            .execute_batch(
                "CREATE TRIGGER fail_report BEFORE INSERT ON bug_enrichments
            WHEN NEW.kind = 'report' BEGIN SELECT RAISE(ABORT, 'report write failed'); END;",
            )
            .await
            .unwrap();
        let subsystems = vec![AttributedSubsystem::from_maintainers("NEW SECTION")];
        let params = || UpdateBugOutcomeParams {
            lifecycle_status: BugLifecycleStatus::Open,
            problem: Some("new title"),
            subsystems: Some(&subsystems),
            vector_json: Some("{}"),
            severity: Severity::High,
            verified_on_sha: Some("abc123"),
            introduced_in_commit: Some("def456"),
            inline_review: "complete report",
            ..Default::default()
        };
        assert!(db.update_bug_outcome(id, params()).await.is_err());
        let after = serde_json::to_value(db.get_bug(id).await.unwrap().unwrap()).unwrap();
        assert_eq!(
            after, before,
            "a failed result left partial writes or audit records"
        );
        let mut vectors = db
            .conn
            .query("SELECT count(*) FROM bug_vectors", ())
            .await
            .unwrap();
        assert_eq!(
            vectors
                .next()
                .await
                .unwrap()
                .unwrap()
                .get::<i64>(0)
                .unwrap(),
            0
        );
        drop(vectors);
        // A process dying before its failure handler runs still leaves a
        // recoverable running row, rather than an incomplete successful result.
        db.conn
            .execute("UPDATE bugs SET lease_expires_at = 1", ())
            .await
            .unwrap();
        assert_eq!(db.recover_stale_running_bugs().await.unwrap(), 1);
        assert!(
            db.claim_pending_bug("replacement", 300, 3)
                .await
                .unwrap()
                .is_some()
        );
        db.conn
            .execute_batch("DROP TRIGGER fail_report;")
            .await
            .unwrap();
        db.update_bug_outcome(id, params()).await.unwrap();
        let saved = db.get_bug(id).await.unwrap().unwrap();
        assert_eq!(saved.pipeline_state, BugPipelineState::Succeeded);
        assert_eq!(saved.lifecycle_status, BugLifecycleStatus::Open);
        assert_eq!(saved.title, "new title");
        assert_eq!(saved.inline_review(), "complete report");
        assert_eq!(saved.severity(), Severity::High);
        assert_eq!(saved.subsystems, vec!["NEW SECTION"]);
    }

    #[tokio::test]
    async fn analysis_verdicts_preserve_existing_triage() {
        for status in [
            BugLifecycleStatus::New,
            BugLifecycleStatus::Open,
            BugLifecycleStatus::Closed,
            BugLifecycleStatus::Dismissed,
            BugLifecycleStatus::Fixed,
            BugLifecycleStatus::Duplicate,
        ] {
            for verdict in [BugLifecycleStatus::Open, BugLifecycleStatus::Dismissed] {
                let db = Database::new(&crate::settings::DatabaseSettings {
                    url: ":memory:".into(),
                    token: String::new(),
                })
                .await
                .unwrap();
                db.migrate().await.unwrap();
                let id = create_pending_bug(&db, "linux-triaged").await;
                let canonical = create_pending_bug(&db, "linux-canonical").await;
                let other = create_pending_bug(&db, "linux-other").await;
                db.claim_pending_bug("worker", 300, 3).await.unwrap();
                if status == BugLifecycleStatus::Duplicate {
                    db.mark_bug_as_duplicate(MarkDuplicateBugParams {
                        ephemeral_id: id,
                        canonical_id: canonical,
                        reasoning: "human triage",
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                } else {
                    db.change_bug_status_with_reason(id, status, Some("human triage"))
                        .await
                        .unwrap();
                }
                if status != BugLifecycleStatus::New {
                    assert!(
                        !db.mark_bug_as_duplicate(MarkDuplicateBugParams {
                            preserve_triage: true,
                            ephemeral_id: id,
                            canonical_id: other,
                            reasoning: "automatic deduplication",
                            ..Default::default()
                        })
                        .await
                        .unwrap()
                    );
                }
                db.update_bug_outcome(
                    id,
                    UpdateBugOutcomeParams {
                        lifecycle_status: verdict,
                        inline_review: "analysis report",
                        verified_on_sha: Some("abc123"),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                let bug = db.get_bug(id).await.unwrap().unwrap();
                assert_eq!(
                    bug.lifecycle_status,
                    if status == BugLifecycleStatus::New {
                        verdict
                    } else {
                        status
                    }
                );
                assert_eq!(bug.pipeline_state, BugPipelineState::Succeeded);
                assert_eq!(
                    bug.duplicate_of_id,
                    (status == BugLifecycleStatus::Duplicate).then_some(canonical)
                );
                assert_eq!(bug.inline_review(), "analysis report");
                assert!(
                    bug.enrichments
                        .iter()
                        .any(|e| e.content.as_deref() == Some("human triage"))
                );
            }
        }
    }

    #[tokio::test]
    async fn stale_analysis_cannot_write_or_clear_a_replacement_lease() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let settings = crate::settings::DatabaseSettings {
            url: directory
                .path()
                .join("lease.db")
                .to_string_lossy()
                .into_owned(),
            token: String::new(),
        };
        let db = Database::new(&settings).await?;
        db.migrate().await?;
        let replacement_db = Database::new(&settings).await?;
        let id = create_pending_bug(&db, "linux-lease-source").await;
        let canonical = create_pending_bug(&db, "linux-lease-target").await;
        db.set_bug_pipeline_state(canonical, BugPipelineState::Succeeded)
            .await?;
        db.conn
            .execute_batch(
                "INSERT INTO patchsets (id) VALUES (1);
            INSERT INTO reviews (id, patchset_id) VALUES (1, 1);",
            )
            .await?;
        // Two different attempts in one process must not share an owner value.
        let first = "host:123:first-attempt";
        let second = "host:123:second-attempt";
        assert_eq!(db.claim_pending_bug(first, 300, 3).await?.unwrap().id, id);
        let stale = db
            .with_bug_claim(id, first)
            .with_bug_actor("reporter", "analysis", None);
        stale
            .add_bug_enrichment(
                id,
                &NewBugEnrichment {
                    kind: "analysis".into(),
                    content: Some("first stage".into()),
                    ..Default::default()
                },
            )
            .await?;
        assert!(
            stale
                .add_bug_enrichment(canonical, &NewBugEnrichment::default())
                .await
                .is_err()
        );
        db.conn
            .execute("UPDATE bugs SET lease_expires_at = 1 WHERE id = ?", [id])
            .await?;
        // Expiration alone is sufficient to revoke writes and renewal.
        assert!(
            stale
                .update_bug_outcome(id, UpdateBugOutcomeParams::default())
                .await
                .is_err()
        );
        assert!(!db.renew_bug_lease(id, first, 300).await?);
        assert_eq!(
            replacement_db
                .claim_pending_bug(second, 300, 3)
                .await?
                .unwrap()
                .id,
            id
        );
        let before = serde_json::to_value(replacement_db.get_bug(id).await?.unwrap())?;
        for handle in [
            stale.with_bug_actor("new actor", "new tool", Some("model".into())),
            stale,
        ] {
            assert!(
                handle
                    .add_bug_enrichment(
                        id,
                        &NewBugEnrichment {
                            kind: "report".into(),
                            content: Some("stale report".into()),
                            ..Default::default()
                        }
                    )
                    .await
                    .is_err()
            );
            assert!(
                handle
                    .update_bug_outcome(
                        id,
                        UpdateBugOutcomeParams {
                            lifecycle_status: BugLifecycleStatus::Open,
                            problem: Some("stale title"),
                            inline_review: "stale report",
                            ..Default::default()
                        }
                    )
                    .await
                    .is_err()
            );
            assert!(
                handle
                    .mark_bug_as_duplicate(MarkDuplicateBugParams {
                        ephemeral_id: id,
                        canonical_id: canonical,
                        ..Default::default()
                    })
                    .await
                    .is_err()
            );
            assert!(
                handle
                    .link_review_to_bug(1, canonical, false)
                    .await
                    .is_err()
            );
            assert!(handle.fail_bug_analysis(id, "stale failure").await.is_err());
            assert!(handle.release_bug_lease(id).await.is_err());
        }
        assert_eq!(
            serde_json::to_value(replacement_db.get_bug(id).await?.unwrap())?,
            before
        );
        assert!(replacement_db.list_bugs_for_review(1).await?.is_empty());
        assert!(replacement_db.renew_bug_lease(id, second, 300).await?);

        let owner = replacement_db.with_bug_claim(id, second);
        owner
            .add_bug_enrichment(
                id,
                &NewBugEnrichment {
                    kind: "analysis".into(),
                    content: Some("replacement stage".into()),
                    ..Default::default()
                },
            )
            .await?;
        owner
            .update_bug_outcome(
                id,
                UpdateBugOutcomeParams {
                    lifecycle_status: BugLifecycleStatus::Open,
                    inline_review: "replacement report",
                    ..Default::default()
                },
            )
            .await?;
        owner.link_review_to_bug(1, id, true).await?;
        assert!(replacement_db.renew_bug_lease(id, second, 300).await?);
        assert!(
            owner
                .fail_bug_analysis(id, "error after commit")
                .await
                .is_err()
        );
        assert_eq!(
            replacement_db.get_bug(id).await?.unwrap().pipeline_state,
            BugPipelineState::Succeeded
        );
        owner.release_bug_lease(id).await?;
        assert!(!replacement_db.renew_bug_lease(id, second, 300).await?);
        Ok(())
    }

    #[tokio::test]
    async fn automatic_duplicate_keeps_its_claim_until_linking_finishes() -> Result<()> {
        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".into(),
            token: String::new(),
        })
        .await?;
        db.migrate().await?;
        let id = create_pending_bug(&db, "linux-owned-duplicate").await;
        let canonical = create_pending_bug(&db, "linux-owned-canonical").await;
        db.set_bug_pipeline_state(canonical, BugPipelineState::Succeeded)
            .await?;
        db.conn
            .execute_batch(
                "INSERT INTO patchsets (id) VALUES (1);
            INSERT INTO reviews (id, patchset_id) VALUES (1, 1);",
            )
            .await?;
        db.claim_pending_bug("attempt", 300, 3).await?;
        let owner = db.with_bug_claim(id, "attempt");
        assert!(
            owner
                .mark_bug_as_duplicate(MarkDuplicateBugParams {
                    preserve_triage: true,
                    ephemeral_id: id,
                    canonical_id: canonical,
                    ..Default::default()
                })
                .await?
        );
        assert!(db.renew_bug_lease(id, "attempt", 300).await?);
        owner.link_review_to_bug(1, canonical, false).await?;
        owner.release_bug_lease(id).await?;
        assert_eq!(db.list_bugs_for_review(1).await?[0].0.id, canonical);
        assert!(!db.renew_bug_lease(id, "attempt", 300).await?);

        let human_fold = create_pending_bug(&db, "linux-human-duplicate").await;
        db.claim_pending_bug("another-attempt", 300, 3).await?;
        let cancelled = db.with_bug_claim(human_fold, "another-attempt");
        db.mark_bug_as_duplicate(MarkDuplicateBugParams {
            ephemeral_id: human_fold,
            canonical_id: canonical,
            ..Default::default()
        })
        .await?;
        assert!(
            !db.renew_bug_lease(human_fold, "another-attempt", 300)
                .await?
        );
        assert!(
            cancelled
                .fail_bug_analysis(human_fold, "cancelled")
                .await
                .is_err()
        );
        assert_eq!(
            db.get_bug(human_fold).await?.unwrap().pipeline_state,
            BugPipelineState::Succeeded
        );
        Ok(())
    }

    /// The lease is short and the worker renews it while it works. Renewal has
    /// to hold off other workers for as long as the analysis genuinely runs,
    /// and has to refuse once the claim belongs to somebody else.
    #[tokio::test]
    async fn test_renew_bug_lease_holds_and_detects_takeover() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_pending_bug(&db, "linux-long-running").await;
        db.claim_pending_bug("worker-a", 300, 10)
            .await
            .unwrap()
            .expect("should claim bug");

        // Renew a live lease before it expires.
        db.conn
            .execute(
                "UPDATE bugs SET lease_expires_at = unixepoch() + 10 WHERE id = ?",
                libsql::params![bug_id],
            )
            .await
            .unwrap();
        assert!(
            db.renew_bug_lease(bug_id, "worker-a", 300).await.unwrap(),
            "the holder must be able to push its own lease forward"
        );

        // Having renewed, the bug is protected from both recovery paths.
        assert_eq!(db.recover_stale_running_bugs().await.unwrap(), 0);
        assert!(
            db.claim_pending_bug("worker-b", 300, 10)
                .await
                .unwrap()
                .is_none(),
            "a renewed lease must keep other workers out"
        );

        // Once the lease really lapses another worker takes over, and the
        // original holder must not be able to take it back.
        db.conn
            .execute(
                "UPDATE bugs SET lease_expires_at = 1 WHERE id = ?",
                libsql::params![bug_id],
            )
            .await
            .unwrap();
        let stolen = db
            .claim_pending_bug("worker-b", 300, 10)
            .await
            .unwrap()
            .expect("an expired lease is reclaimable");
        assert_eq!(stolen.id, bug_id);
        assert!(
            !db.renew_bug_lease(bug_id, "worker-a", 300).await.unwrap(),
            "a worker that lost the claim must not renew it"
        );
    }

    /// One review can raise two candidates that later turn out to be the same
    /// defect. Folding one into the other links the survivor back to that same
    /// review as a rediscovery, which must not erase the fact that the review
    /// discovered it in the first place.
    #[tokio::test]
    async fn test_link_review_to_bug_never_downgrades_discovery() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_pending_bug(&db, "linux-two-symptoms").await;
        // reviews.patchset_id is a foreign key, so the parent must exist.
        db.conn
            .execute(
                "INSERT INTO patchsets (id, subject, status) VALUES (1, 'test series', 'Reviewed')",
                (),
            )
            .await
            .unwrap();
        let review_id = db
            .conn
            .query(
                "INSERT INTO reviews (patchset_id, status) VALUES (1, 'Reviewed') RETURNING id",
                (),
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap();

        // The review discovers the bug, then rediscovers it by folding a
        // sibling candidate into it.
        db.link_review_to_bug(review_id, bug_id, true)
            .await
            .unwrap();
        db.link_review_to_bug(review_id, bug_id, false)
            .await
            .unwrap();

        let flag: i64 = db
            .conn
            .query(
                "SELECT is_newly_discovered FROM bug_reviews WHERE review_id = ? AND bug_id = ?",
                libsql::params![review_id, bug_id],
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(flag, 1, "a rediscovery must not erase the discovery");

        // The reverse order must still end up crediting the discovery.
        let other_id = create_pending_bug(&db, "linux-two-symptoms-b").await;
        db.link_review_to_bug(review_id, other_id, false)
            .await
            .unwrap();
        db.link_review_to_bug(review_id, other_id, true)
            .await
            .unwrap();
        let flag: i64 = db
            .conn
            .query(
                "SELECT is_newly_discovered FROM bug_reviews WHERE review_id = ? AND bug_id = ?",
                libsql::params![review_id, other_id],
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap()
            .get(0)
            .unwrap();
        assert_eq!(flag, 1);
    }

    /// Assignment writes both columns together, because the schema requires
    /// assigned_at to be present exactly when there is an assignee.
    #[tokio::test]
    async fn test_assign_and_unassign_bug() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_pending_bug(&db, "linux-assignable").await;
        let fresh = db.get_bug(bug_id).await.unwrap().unwrap();
        assert!(fresh.assignee.is_none());
        assert!(fresh.assigned_at.is_none());

        let scoped = db.with_bug_actor("triager@example.org", "web", None);
        assert!(
            scoped
                .assign_bug(bug_id, Some("  dev@example.org  "), Some("Owns this area"))
                .await
                .unwrap()
        );
        let assigned = db.get_bug(bug_id).await.unwrap().unwrap();
        // The address is stored trimmed so that filtering by exact match works.
        assert_eq!(assigned.assignee.as_deref(), Some("dev@example.org"));
        assert!(assigned.assigned_at.is_some());
        assert!(
            assigned.enrichments.iter().any(|e| e.kind == "audit"
                && e.content.as_deref() == Some("Assigned to dev@example.org"))
        );
        assert!(
            assigned
                .enrichments
                .iter()
                .any(|e| e.kind == "comment" && e.content.as_deref() == Some("Owns this area"))
        );

        // Reassignment is recorded as a handover rather than as two events.
        scoped
            .assign_bug(bug_id, Some("other@example.org"), None)
            .await
            .unwrap();
        let reassigned = db.get_bug(bug_id).await.unwrap().unwrap();
        assert_eq!(reassigned.assignee.as_deref(), Some("other@example.org"));
        assert!(reassigned.enrichments.iter().any(|e| {
            e.content.as_deref() == Some("Reassigned from dev@example.org to other@example.org")
        }));

        // Clearing the assignee must clear the timestamp with it.
        scoped.assign_bug(bug_id, None, None).await.unwrap();
        let cleared = db.get_bug(bug_id).await.unwrap().unwrap();
        assert!(cleared.assignee.is_none());
        assert!(cleared.assigned_at.is_none());

        // An unknown bug reports failure instead of silently doing nothing.
        assert!(!db.assign_bug(999_999, Some("x@y.org"), None).await.unwrap());
    }

    /// Filtering by assignee must be able to express both "mine" and
    /// "nobody has picked this up yet".
    #[tokio::test]
    async fn test_list_bugs_by_assignee() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let mine = create_pending_bug(&db, "linux-mine").await;
        let theirs = create_pending_bug(&db, "linux-theirs").await;
        let nobody = create_pending_bug(&db, "linux-nobody").await;
        db.assign_bug(mine, Some("me@example.org"), None)
            .await
            .unwrap();
        db.assign_bug(theirs, Some("them@example.org"), None)
            .await
            .unwrap();

        let (items, total) = db
            .list_bugs(ListBugsParams {
                assignee: Some(AssigneeFilter::Is("me@example.org")),
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].id, mine);

        let (items, total) = db
            .list_bugs(ListBugsParams {
                assignee: Some(AssigneeFilter::Unassigned),
                visibility: BugVisibility::Unrestricted,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].id, nobody);
    }

    /// A bug that keeps failing must not be retried forever, and once it stops
    /// being retried it must say so rather than sitting in the queue looking
    /// like work that is about to happen.
    #[tokio::test]
    async fn test_bug_analysis_retry_cap_and_dead_letter() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_pending_bug(&db, "linux-always-fails").await;
        let max_attempts = 2;

        for attempt in 1..=max_attempts {
            let claimed = db
                .claim_pending_bug("worker-a", 3600, max_attempts)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("attempt {} should be claimable", attempt));
            assert_eq!(claimed.id, bug_id);
            db.fail_bug_analysis(bug_id, "boom").await.unwrap();
            // A failed attempt leaves triage alone: a crashed run says nothing
            // about whether the defect is real.
            let failed = db.get_bug(bug_id).await.unwrap().unwrap();
            assert_eq!(failed.pipeline_state, BugPipelineState::Failed);
            assert_eq!(failed.lifecycle_status, BugLifecycleStatus::New);
        }

        // The cap is now reached, so the bug is no longer claimable.
        assert!(
            db.claim_pending_bug("worker-a", 3600, max_attempts)
                .await
                .unwrap()
                .is_none()
        );

        assert_eq!(db.abandon_exhausted_bugs(max_attempts).await.unwrap(), 1);
        let abandoned = db.get_bug(bug_id).await.unwrap().unwrap();
        assert_eq!(abandoned.pipeline_state, BugPipelineState::Abandoned);
        assert_eq!(abandoned.lifecycle_status, BugLifecycleStatus::New);

        // Abandoning is idempotent and never resurrects the bug.
        assert_eq!(db.abandon_exhausted_bugs(max_attempts).await.unwrap(), 0);
        assert!(
            db.claim_pending_bug("worker-a", 3600, max_attempts)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_bug_enrichments_multi_tool_timeline() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        // 1. Initial report from syzbot
        let new_bug = NewBug {
            bugid: "linux-syzbot-12345".to_string(),
            title: "KASAN: use-after-free Read in sock_close".to_string(),
            lifecycle_status: BugLifecycleStatus::New,
            pipeline_state: BugPipelineState::Pending,
            assignee: None,
            reporter: "syzbot".to_string(),
            reported_at: 1700000000,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: Some("c0ffee112233".to_string()),
            source_ref: Some("https://syzkaller.appspot.com/bug?id=12345".to_string()),
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
        };
        let bug_id = db.create_bug(&new_bug).await.unwrap();

        // Raw crash report enrichment from syzbot
        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "report".to_string(),
                tool: "syzbot".to_string(),
                model: None,
                author: None,
                created_at: 1700000000,
                content: Some("BUG: KASAN: use-after-free in sock_close+0x42/0x100".to_string()),
                data_json: Some(json!({
                    "crash_type": "use-after-free",
                    "has_reproducer": true,
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Reproducer enrichment from syzbot
        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "reproducer".to_string(),
                tool: "syzbot".to_string(),
                model: None,
                author: None,
                created_at: 1700000010,
                content: Some("#define _GNU_SOURCE\nint main() { ... }".to_string()),
                data_json: Some(json!({
                    "language": "C",
                    "syz_repro": "syz_open_pts() ...",
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // 2. Sashiko automated verification with LLM
        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "verification".to_string(),
                tool: "sashiko".to_string(),
                model: Some("gemini-1.5-pro".to_string()),
                author: None,
                created_at: 1700000050,
                content: Some("Confirmed UAF in net/socket.c sock_close during concurrent disconnect".to_string()),
                data_json: Some(json!({
                    "is_valid": true,
                    "locations": [{"file": "net/socket.c", "line": 642, "function_or_symbol": "sock_close"}],
                    "source_files": ["net/socket.c"],
                })),
                tokens_in: Some(4000),
                tokens_out: Some(500),
                tokens_cached: Some(3000),
                logs: Some("[{\"step\": 1, \"status\": \"verified\"}]".to_string()),
            },
        )
        .await
        .unwrap();

        // 3. Sashiko severity calibration
        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "severity_calibration".to_string(),
                tool: "sashiko".to_string(),
                model: Some("gemini-1.5-pro".to_string()),
                author: None,
                created_at: 1700000060,
                content: Some(
                    "Privilege escalation potential through dangling socket file descriptor"
                        .to_string(),
                ),
                data_json: Some(json!({
                    "severity": "Critical",
                    "severity_int": 4,
                })),
                tokens_in: Some(1500),
                tokens_out: Some(200),
                tokens_cached: Some(1000),
                logs: None,
            },
        )
        .await
        .unwrap();

        // 4. Sashiko origin discovery
        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "origin_discovery".to_string(),
                tool: "sashiko".to_string(),
                model: Some("gemini-1.5-pro".to_string()),
                author: None,
                created_at: 1700000070,
                content: Some("9876543210ab (net: socket: optimize close locking)".to_string()),
                data_json: Some(json!({
                    "introducing_commit_sha": "9876543210ab (net: socket: optimize close locking)",
                })),
                tokens_in: Some(2000),
                tokens_out: Some(150),
                tokens_cached: Some(1500),
                logs: None,
            },
        )
        .await
        .unwrap();

        // 5. Human comment
        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "comment".to_string(),
                tool: "human".to_string(),
                model: None,
                author: Some("torvalds@linux-foundation.org".to_string()),
                created_at: 1700000100,
                content: Some(
                    "I agree with the origin tracing, we should revert commit 9876543210ab."
                        .to_string(),
                ),
                data_json: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // 6. Fix candidate
        db.add_bug_enrichment(
            bug_id,
            &NewBugEnrichment {
                kind: "fix_candidate".to_string(),
                tool: "human".to_string(),
                model: None,
                author: Some("davem@davemloft.net".to_string()),
                created_at: 1700000200,
                content: Some("Revert 9876543210ab and add proper socket refcounting".to_string()),
                data_json: Some(json!({
                    "status": "merged",
                    "commit_sha": "fedcba098765 (net: socket: fix race in sock_close)",
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Query bug and verify all projections and enrichments
        let bug = db.get_bug(bug_id).await.unwrap().expect("Bug must exist");
        assert_eq!(bug.id, bug_id);
        assert_eq!(bug.bugid, "linux-syzbot-12345");
        assert_eq!(bug.reporter, "syzbot");
        assert_eq!(bug.reported_at, 1700000000);
        assert_eq!(
            bug.source_ref.as_deref(),
            Some("https://syzkaller.appspot.com/bug?id=12345")
        );
        assert_eq!(bug.problem(), "KASAN: use-after-free Read in sock_close");
        assert_eq!(bug.severity(), Severity::Critical);
        assert_eq!(
            bug.severity_explanation().as_deref(),
            Some("Privilege escalation potential through dangling socket file descriptor")
        );
        assert_eq!(bug.source_files(), Some(vec!["net/socket.c".to_string()]));
        assert_eq!(
            bug.introduced_in_commit().as_deref(),
            Some("9876543210ab (net: socket: optimize close locking)")
        );
        assert!(bug.is_fixed());
        assert_eq!(
            bug.fixed_in_commit().as_deref(),
            Some("fedcba098765 (net: socket: fix race in sock_close)")
        );

        // Verify aggregated token counters
        assert_eq!(bug.tokens_in(), 4000 + 1500 + 2000);
        assert_eq!(bug.tokens_out(), 500 + 200 + 150);
        assert_eq!(bug.tokens_cached(), 3000 + 1000 + 1500);

        // Verify enrichments timeline length and chronological order
        assert_eq!(bug.enrichments.len(), 9);
        let enrichments: Vec<_> = bug
            .enrichments
            .iter()
            .filter(|e| e.kind != "audit")
            .collect();
        let kinds: Vec<&str> = enrichments.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "report",
                "reproducer",
                "verification",
                "severity_calibration",
                "origin_discovery",
                "comment",
                "fix_candidate",
            ]
        );

        let tools: Vec<&str> = enrichments.iter().map(|e| e.tool.as_str()).collect();
        assert_eq!(
            tools,
            vec![
                "syzbot", "syzbot", "sashiko", "sashiko", "sashiko", "human", "human"
            ]
        );

        let models: Vec<Option<&str>> = enrichments.iter().map(|e| e.model.as_deref()).collect();
        assert_eq!(
            models,
            vec![
                None,
                None,
                Some("gemini-1.5-pro"),
                Some("gemini-1.5-pro"),
                Some("gemini-1.5-pro"),
                None,
                None,
            ]
        );

        // Check dedicated enrichment query method
        let enrichments = db.get_bug_enrichments(bug_id).await.unwrap();
        assert_eq!(enrichments.len(), 9);
    }
}
