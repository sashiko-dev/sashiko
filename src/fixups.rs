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

use crate::settings::ReviewSettings;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::str::FromStr;
use tokio::io::AsyncWriteExt;

/// A generated candidate patch that may address a finding or suggestion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateFixup {
    pub title: String,
    pub category: FixupCategory,
    pub rationale: String,
    pub confidence: FixupConfidence,
    pub applies_to_finding_id: Option<i64>,
    pub applies_to_suggestion_id: Option<i64>,
    pub patch: String,
    pub files_touched: Vec<String>,
    pub risk: FixupRisk,
    pub requires_human_testing: bool,
}

/// A candidate fixup record persisted for a specific review run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateFixupRecord {
    pub id: i64,
    pub review_id: i64,
    pub patchset_id: i64,
    pub patch_id: Option<i64>,
    pub finding_id: Option<i64>,
    pub suggestion_id: Option<i64>,
    pub title: String,
    pub category: FixupCategory,
    pub rationale: String,
    pub confidence: FixupConfidence,
    pub risk: FixupRisk,
    pub patch: String,
    pub files_touched: Vec<String>,
    pub validation_status: FixupValidationStatus,
    pub created_at: i64,
}

/// Structured LLM output for generated fixups.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateFixupOutput {
    #[serde(default)]
    pub candidate_fixups: Vec<CandidateFixup>,
}

/// A candidate fixup after policy and applyability validation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreparedCandidateFixup {
    pub fixup: CandidateFixup,
    pub validation_status: FixupValidationStatus,
    pub files_touched: Vec<String>,
}

/// Generation mode selected by configuration or CLI flags.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FixupMode {
    Off,
    Trivial,
    Local,
    ReviewInformed,
    All,
}

impl FixupMode {
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl fmt::Display for FixupMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Trivial => "trivial",
            Self::Local => "local",
            Self::ReviewInformed => "review-informed",
            Self::All => "all",
        })
    }
}

impl FromStr for FixupMode {
    type Err = FixupParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "trivial" => Ok(Self::Trivial),
            "local" => Ok(Self::Local),
            "review-informed" => Ok(Self::ReviewInformed),
            "all" => Ok(Self::All),
            _ => Err(FixupParseError::UnknownMode(value.to_string())),
        }
    }
}

/// Common fixup-generation settings shared by local and daemon review paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixupGenerationConfig {
    pub enabled: bool,
    pub mode: FixupMode,
    pub max_fixups_per_patchset: usize,
    pub validation_policy: FixupValidationPolicy,
}

impl FixupGenerationConfig {
    pub fn from_review_settings(settings: &ReviewSettings) -> Result<Self, FixupParseError> {
        let mode = FixupMode::from_str(&settings.fixup_mode)?;
        Ok(Self {
            enabled: settings.generate_fixups && mode.is_enabled(),
            mode,
            max_fixups_per_patchset: settings.max_fixups_per_patchset,
            validation_policy: FixupValidationPolicy {
                max_lines: settings.max_fixup_lines,
                min_confidence: FixupConfidence::from_str(&settings.min_fixup_confidence)?,
                max_risk: FixupRisk::from_str(&settings.max_fixup_risk)?,
                allowed_path_prefixes: Vec::new(),
            },
        })
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            mode: FixupMode::Off,
            max_fixups_per_patchset: 0,
            validation_policy: FixupValidationPolicy::trivial(0),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FixupCategory {
    Spelling,
    Documentation,
    Kerneldoc,
    Comment,
    Test,
    Cleanup,
    ErrorHandling,
    Locking,
    Lifetime,
    HelperExtraction,
    PatchOrganization,
    DesignExample,
}

impl fmt::Display for FixupCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Spelling => "spelling",
            Self::Documentation => "documentation",
            Self::Kerneldoc => "kerneldoc",
            Self::Comment => "comment",
            Self::Test => "test",
            Self::Cleanup => "cleanup",
            Self::ErrorHandling => "error-handling",
            Self::Locking => "locking",
            Self::Lifetime => "lifetime",
            Self::HelperExtraction => "helper-extraction",
            Self::PatchOrganization => "patch-organization",
            Self::DesignExample => "design-example",
        })
    }
}

impl FromStr for FixupCategory {
    type Err = FixupParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "spelling" => Ok(Self::Spelling),
            "documentation" => Ok(Self::Documentation),
            "kerneldoc" => Ok(Self::Kerneldoc),
            "comment" => Ok(Self::Comment),
            "test" => Ok(Self::Test),
            "cleanup" => Ok(Self::Cleanup),
            "error-handling" => Ok(Self::ErrorHandling),
            "locking" => Ok(Self::Locking),
            "lifetime" => Ok(Self::Lifetime),
            "helper-extraction" => Ok(Self::HelperExtraction),
            "patch-organization" => Ok(Self::PatchOrganization),
            "design-example" => Ok(Self::DesignExample),
            _ => Err(FixupParseError::UnknownCategory(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum FixupConfidence {
    Low,
    Medium,
    High,
}

impl fmt::Display for FixupConfidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        })
    }
}

impl FromStr for FixupConfidence {
    type Err = FixupParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(FixupParseError::UnknownConfidence(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum FixupRisk {
    Trivial,
    Low,
    Medium,
    High,
}

impl fmt::Display for FixupRisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trivial => "trivial",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        })
    }
}

impl FromStr for FixupRisk {
    type Err = FixupParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "trivial" => Ok(Self::Trivial),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(FixupParseError::UnknownRisk(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FixupValidationStatus {
    Pending,
    Valid,
    InvalidPatch,
    DoesNotApply,
    DisallowedPath,
    TooLarge,
    PolicyFiltered,
}

impl fmt::Display for FixupValidationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pending => "pending",
            Self::Valid => "valid",
            Self::InvalidPatch => "invalid-patch",
            Self::DoesNotApply => "does-not-apply",
            Self::DisallowedPath => "disallowed-path",
            Self::TooLarge => "too-large",
            Self::PolicyFiltered => "policy-filtered",
        })
    }
}

impl FromStr for FixupValidationStatus {
    type Err = FixupParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "valid" => Ok(Self::Valid),
            "invalid-patch" => Ok(Self::InvalidPatch),
            "does-not-apply" => Ok(Self::DoesNotApply),
            "disallowed-path" => Ok(Self::DisallowedPath),
            "too-large" => Ok(Self::TooLarge),
            "policy-filtered" => Ok(Self::PolicyFiltered),
            _ => Err(FixupParseError::UnknownValidationStatus(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FixupParseError {
    #[error("unknown fixup mode: {0}")]
    UnknownMode(String),
    #[error("unknown fixup category: {0}")]
    UnknownCategory(String),
    #[error("unknown fixup confidence: {0}")]
    UnknownConfidence(String),
    #[error("unknown fixup risk: {0}")]
    UnknownRisk(String),
    #[error("unknown fixup validation status: {0}")]
    UnknownValidationStatus(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FixupValidationError {
    #[error("generated patch is empty")]
    EmptyPatch,
    #[error("generated patch does not contain a diff header")]
    MissingDiffHeader,
    #[error("generated patch exceeds the configured line limit")]
    TooManyLines,
    #[error("generated patch does not touch any files")]
    NoFilesTouched,
    #[error("generated patch file list does not match diff headers")]
    FilesTouchedMismatch,
    #[error("generated patch touches a path outside policy: {0}")]
    DisallowedPath(String),
    #[error("generated fixup does not meet confidence or risk policy")]
    PolicyFiltered,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FixupOutputError {
    #[error("candidate fixup output is not valid JSON: {0}")]
    InvalidJson(String),
}

#[derive(Debug, thiserror::Error)]
pub enum FixupApplyError {
    #[error("failed to spawn git apply: {0}")]
    Spawn(std::io::Error),
    #[error("failed to pipe generated patch to git apply: {0}")]
    Stdin(std::io::Error),
    #[error("failed to wait for git apply: {0}")]
    Wait(std::io::Error),
    #[error("generated patch does not apply: {0}")]
    DoesNotApply(String),
}

/// Return the JSON schema used to request structured candidate-fixup output.
pub fn candidate_fixup_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "candidate_fixups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "title": { "type": "string" },
                        "category": {
                            "type": "string",
                            "enum": [
                                "spelling",
                                "documentation",
                                "kerneldoc",
                                "comment",
                                "test",
                                "cleanup",
                                "error-handling",
                                "locking",
                                "lifetime",
                                "helper-extraction",
                                "patch-organization",
                                "design-example"
                            ]
                        },
                        "rationale": { "type": "string" },
                        "confidence": {
                            "type": "string",
                            "enum": ["low", "medium", "high"]
                        },
                        "applies_to_finding_id": { "type": ["integer", "null"] },
                        "applies_to_suggestion_id": { "type": ["integer", "null"] },
                        "patch": { "type": "string" },
                        "files_touched": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "risk": {
                            "type": "string",
                            "enum": ["trivial", "low", "medium", "high"]
                        },
                        "requires_human_testing": { "type": "boolean" }
                    },
                    "required": [
                        "title",
                        "category",
                        "rationale",
                        "confidence",
                        "applies_to_finding_id",
                        "applies_to_suggestion_id",
                        "patch",
                        "files_touched",
                        "risk",
                        "requires_human_testing"
                    ]
                }
            }
        },
        "required": ["candidate_fixups"]
    })
}

/// Parse structured candidate-fixup output returned by an AI provider.
pub fn parse_candidate_fixup_output(raw: &str) -> Result<CandidateFixupOutput, FixupOutputError> {
    serde_json::from_str(raw).map_err(|err| FixupOutputError::InvalidJson(err.to_string()))
}

/// Map deterministic validation errors to persisted validation statuses.
pub fn validation_status_for_error(error: &FixupValidationError) -> FixupValidationStatus {
    match error {
        FixupValidationError::TooManyLines => FixupValidationStatus::TooLarge,
        FixupValidationError::DisallowedPath(_) => FixupValidationStatus::DisallowedPath,
        FixupValidationError::PolicyFiltered => FixupValidationStatus::PolicyFiltered,
        FixupValidationError::EmptyPatch
        | FixupValidationError::MissingDiffHeader
        | FixupValidationError::NoFilesTouched
        | FixupValidationError::FilesTouchedMismatch => FixupValidationStatus::InvalidPatch,
    }
}

/// Verify that a generated patch applies cleanly to a review worktree.
pub async fn validate_patch_applies(
    worktree: impl AsRef<Path>,
    patch: &str,
) -> Result<(), FixupApplyError> {
    let mut child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree.as_ref())
        .arg("apply")
        .arg("--check")
        .arg("-")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(FixupApplyError::Spawn)?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(patch.as_bytes())
            .await
            .map_err(FixupApplyError::Stdin)?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(FixupApplyError::Wait)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(FixupApplyError::DoesNotApply(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

/// Policy used before displaying or storing generated fixups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixupValidationPolicy {
    pub max_lines: usize,
    pub min_confidence: FixupConfidence,
    pub max_risk: FixupRisk,
    pub allowed_path_prefixes: Vec<String>,
}

impl FixupValidationPolicy {
    pub fn trivial(max_lines: usize) -> Self {
        Self {
            max_lines,
            min_confidence: FixupConfidence::High,
            max_risk: FixupRisk::Low,
            allowed_path_prefixes: Vec::new(),
        }
    }
}

/// Validate and classify generated fixups using common local/daemon policy.
///
/// The function never fails the caller's review. Every generated candidate is
/// converted into a prepared fixup with a validation status, and callers decide
/// which statuses are visible for their product surface.
pub async fn prepare_candidate_fixups(
    candidates: impl IntoIterator<Item = CandidateFixup>,
    policy: &FixupValidationPolicy,
    worktree: Option<&Path>,
    max_fixups: usize,
) -> Vec<PreparedCandidateFixup> {
    let mut prepared = Vec::new();
    for fixup in candidates.into_iter().take(max_fixups) {
        prepared.push(prepare_candidate_fixup(fixup, policy, worktree).await);
    }
    prepared
}

async fn prepare_candidate_fixup(
    mut fixup: CandidateFixup,
    policy: &FixupValidationPolicy,
    worktree: Option<&Path>,
) -> PreparedCandidateFixup {
    match validate_candidate_fixup(&fixup, policy) {
        Ok(files_touched) => {
            fixup.files_touched = files_touched.clone();
            let validation_status = validate_fixup_applyability(worktree, &fixup.patch).await;
            PreparedCandidateFixup {
                fixup,
                validation_status,
                files_touched,
            }
        }
        Err(error) => PreparedCandidateFixup {
            fixup,
            validation_status: validation_status_for_error(&error),
            files_touched: Vec::new(),
        },
    }
}

async fn validate_fixup_applyability(
    worktree: Option<&Path>,
    patch: &str,
) -> FixupValidationStatus {
    match worktree {
        Some(path) => match validate_patch_applies(path, patch).await {
            Ok(()) => FixupValidationStatus::Valid,
            Err(_) => FixupValidationStatus::DoesNotApply,
        },
        None => FixupValidationStatus::Pending,
    }
}

/// Validate the structure and policy-relevant metadata for a generated fixup.
///
/// Applyability validation is intentionally separate because it requires a
/// checked-out review worktree. This function performs deterministic checks
/// that are safe to run before any filesystem mutation.
pub fn validate_candidate_fixup(
    fixup: &CandidateFixup,
    policy: &FixupValidationPolicy,
) -> Result<Vec<String>, FixupValidationError> {
    if fixup.patch.trim().is_empty() {
        return Err(FixupValidationError::EmptyPatch);
    }

    if !fixup
        .patch
        .lines()
        .any(|line| line.starts_with("diff --git "))
    {
        return Err(FixupValidationError::MissingDiffHeader);
    }

    if policy.max_lines > 0 && fixup.patch.lines().count() > policy.max_lines {
        return Err(FixupValidationError::TooManyLines);
    }

    if fixup.confidence < policy.min_confidence || fixup.risk > policy.max_risk {
        return Err(FixupValidationError::PolicyFiltered);
    }

    let diff_files = files_touched_by_diff(&fixup.patch);
    if diff_files.is_empty() {
        return Err(FixupValidationError::NoFilesTouched);
    }

    let declared_files = normalize_declared_files(&fixup.files_touched);
    if declared_files != diff_files {
        return Err(FixupValidationError::FilesTouchedMismatch);
    }

    for path in &diff_files {
        if is_disallowed_path(path, &policy.allowed_path_prefixes) {
            return Err(FixupValidationError::DisallowedPath(path.clone()));
        }
    }

    Ok(diff_files.into_iter().collect())
}

fn normalize_declared_files(files: &[String]) -> BTreeSet<String> {
    files
        .iter()
        .map(|path| normalize_diff_path(path))
        .filter(|path| !path.is_empty())
        .collect()
}

fn files_touched_by_diff(patch: &str) -> BTreeSet<String> {
    patch
        .lines()
        .filter_map(|line| line.strip_prefix("diff --git a/"))
        .filter_map(|line| line.split_once(" b/"))
        .filter_map(|(_, new_path)| normalize_active_path(new_path))
        .collect()
}

fn normalize_active_path(path: &str) -> Option<String> {
    let path = normalize_diff_path(path);
    if path == "/dev/null" || path.is_empty() {
        None
    } else {
        Some(path)
    }
}

fn normalize_diff_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("a/")
        .trim_start_matches("b/")
        .to_string()
}

fn is_disallowed_path(path: &str, allowed_prefixes: &[String]) -> bool {
    !allowed_prefixes.is_empty()
        && !allowed_prefixes.iter().any(|prefix| {
            path == prefix.as_str()
                || path
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|remainder| remainder.starts_with('/'))
        })
}

impl crate::db::Database {
    /// Store a generated candidate fixup after generation and validation.
    pub async fn create_candidate_fixup(
        &self,
        review_id: i64,
        patchset_id: i64,
        patch_id: Option<i64>,
        fixup: &CandidateFixup,
        validation_status: FixupValidationStatus,
    ) -> anyhow::Result<i64> {
        let files_touched = serde_json::to_string(&fixup.files_touched)?;
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;

        let mut rows = self
            .conn
            .query(
                "INSERT INTO candidate_fixups \
                 (review_id, patchset_id, patch_id, finding_id, suggestion_id, title, \
                  category, rationale, confidence, risk, patch, files_touched, \
                  validation_status, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
                libsql::params![
                    review_id,
                    patchset_id,
                    patch_id,
                    fixup.applies_to_finding_id,
                    fixup.applies_to_suggestion_id,
                    fixup.title.as_str(),
                    fixup.category.to_string(),
                    fixup.rationale.as_str(),
                    fixup.confidence.to_string(),
                    fixup.risk.to_string(),
                    fixup.patch.as_str(),
                    files_touched,
                    validation_status.to_string(),
                    created_at
                ],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("failed to create candidate fixup"))
        }
    }

    /// Return candidate fixups associated with a review run.
    pub async fn get_candidate_fixups_by_review(
        &self,
        review_id: i64,
    ) -> anyhow::Result<Vec<CandidateFixupRecord>> {
        self.query_candidate_fixups(
            "SELECT id, review_id, patchset_id, patch_id, finding_id, suggestion_id, \
             title, category, rationale, confidence, risk, patch, files_touched, \
             validation_status, created_at \
             FROM candidate_fixups WHERE review_id = ? ORDER BY id ASC",
            review_id,
        )
        .await
    }

    /// Return candidate fixups associated with a patchset.
    pub async fn get_candidate_fixups_by_patchset(
        &self,
        patchset_id: i64,
    ) -> anyhow::Result<Vec<CandidateFixupRecord>> {
        self.query_candidate_fixups(
            "SELECT id, review_id, patchset_id, patch_id, finding_id, suggestion_id, \
             title, category, rationale, confidence, risk, patch, files_touched, \
             validation_status, created_at \
             FROM candidate_fixups WHERE patchset_id = ? ORDER BY id ASC",
            patchset_id,
        )
        .await
    }

    async fn query_candidate_fixups(
        &self,
        sql: &str,
        id: i64,
    ) -> anyhow::Result<Vec<CandidateFixupRecord>> {
        let mut rows = self.conn.query(sql, libsql::params![id]).await?;
        let mut fixups = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            fixups.push(candidate_fixup_from_row(&row)?);
        }
        Ok(fixups)
    }
}

fn candidate_fixup_from_row(row: &libsql::Row) -> anyhow::Result<CandidateFixupRecord> {
    let category: String = row.get(7)?;
    let confidence: String = row.get(9)?;
    let risk: String = row.get(10)?;
    let files_touched: String = row.get(12)?;
    let validation_status: String = row.get(13)?;

    Ok(CandidateFixupRecord {
        id: row.get(0)?,
        review_id: row.get(1)?,
        patchset_id: row.get(2)?,
        patch_id: row.get::<Option<i64>>(3).ok().flatten(),
        finding_id: row.get::<Option<i64>>(4).ok().flatten(),
        suggestion_id: row.get::<Option<i64>>(5).ok().flatten(),
        title: row.get(6)?,
        category: FixupCategory::from_str(&category)?,
        rationale: row.get(8)?,
        confidence: FixupConfidence::from_str(&confidence)?,
        risk: FixupRisk::from_str(&risk)?,
        patch: row.get(11)?,
        files_touched: serde_json::from_str(&files_touched)?,
        validation_status: FixupValidationStatus::from_str(&validation_status)?,
        created_at: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spelling_fixup() -> CandidateFixup {
        CandidateFixup {
            title: "Fix spelling in comment".to_string(),
            category: FixupCategory::Spelling,
            rationale: "The comment says recieve instead of receive.".to_string(),
            confidence: FixupConfidence::High,
            applies_to_finding_id: None,
            applies_to_suggestion_id: None,
            patch: "diff --git a/drivers/foo/bar.c b/drivers/foo/bar.c\n--- a/drivers/foo/bar.c\n+++ b/drivers/foo/bar.c\n@@ -1 +1 @@\n-/* recieve */\n+/* receive */\n".to_string(),
            files_touched: vec!["drivers/foo/bar.c".to_string()],
            risk: FixupRisk::Trivial,
            requires_human_testing: false,
        }
    }

    #[test]
    fn validates_matching_trivial_fixup() {
        let fixup = spelling_fixup();
        let files = validate_candidate_fixup(&fixup, &FixupValidationPolicy::trivial(50))
            .expect("fixup should validate");
        assert_eq!(files, vec!["drivers/foo/bar.c".to_string()]);
    }

    #[test]
    fn rejects_mismatched_file_list() {
        let mut fixup = spelling_fixup();
        fixup.files_touched = vec!["drivers/foo/other.c".to_string()];
        let err = validate_candidate_fixup(&fixup, &FixupValidationPolicy::trivial(50))
            .expect_err("mismatched files should fail");
        assert_eq!(err, FixupValidationError::FilesTouchedMismatch);
    }

    #[test]
    fn rejects_fixup_outside_allowed_paths() {
        let fixup = spelling_fixup();
        let mut policy = FixupValidationPolicy::trivial(50);
        policy.allowed_path_prefixes = vec!["Documentation".to_string()];
        let err =
            validate_candidate_fixup(&fixup, &policy).expect_err("disallowed path should fail");
        assert_eq!(
            err,
            FixupValidationError::DisallowedPath("drivers/foo/bar.c".to_string())
        );
    }

    #[test]
    fn rejects_risk_above_policy() {
        let mut fixup = spelling_fixup();
        fixup.risk = FixupRisk::Medium;
        let err = validate_candidate_fixup(&fixup, &FixupValidationPolicy::trivial(50))
            .expect_err("medium-risk fixup should fail trivial policy");
        assert_eq!(err, FixupValidationError::PolicyFiltered);
    }

    #[test]
    fn parses_fixup_mode_from_config_strings() {
        assert_eq!(FixupMode::from_str("off"), Ok(FixupMode::Off));
        assert_eq!(FixupMode::from_str("trivial"), Ok(FixupMode::Trivial));
        assert_eq!(FixupMode::from_str("local"), Ok(FixupMode::Local));
        assert_eq!(
            FixupMode::from_str("review-informed"),
            Ok(FixupMode::ReviewInformed)
        );
        assert_eq!(FixupMode::from_str("all"), Ok(FixupMode::All));
        assert_eq!(
            FixupMode::from_str("surprise"),
            Err(FixupParseError::UnknownMode("surprise".to_string()))
        );
    }

    #[test]
    fn parses_candidate_fixup_output() {
        let raw = serde_json::to_string(&CandidateFixupOutput {
            candidate_fixups: vec![spelling_fixup()],
        })
        .expect("test output should serialize");
        let parsed = parse_candidate_fixup_output(&raw).expect("output should parse");
        assert_eq!(parsed.candidate_fixups.len(), 1);
        assert_eq!(parsed.candidate_fixups[0].category, FixupCategory::Spelling);
    }

    #[tokio::test]
    async fn prepares_invalid_fixup_without_failing_review() {
        let mut fixup = spelling_fixup();
        fixup.patch.clear();
        let prepared =
            prepare_candidate_fixups(vec![fixup], &FixupValidationPolicy::trivial(50), None, 3)
                .await;
        assert_eq!(prepared.len(), 1);
        assert_eq!(
            prepared[0].validation_status,
            FixupValidationStatus::InvalidPatch
        );
    }

    #[tokio::test]
    async fn preparation_honors_fixup_limit() {
        let prepared = prepare_candidate_fixups(
            vec![spelling_fixup(), spelling_fixup()],
            &FixupValidationPolicy::trivial(50),
            None,
            1,
        )
        .await;
        assert_eq!(prepared.len(), 1);
    }

    #[tokio::test]
    async fn zero_fixup_limit_suppresses_all_candidates() {
        let prepared = prepare_candidate_fixups(
            vec![spelling_fixup()],
            &FixupValidationPolicy::trivial(50),
            None,
            0,
        )
        .await;
        assert!(prepared.is_empty());
    }
}
