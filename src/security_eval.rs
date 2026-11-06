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

//! Threat-model-gated security classification and deterministic FIRST CVSS v4.0
//! vector construction and scoring for canonical Sashiko bugs.

use cvss::v4::Vector;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// High-level security classification under the target project's threat model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityClass {
    /// Crosses a trust boundary guaranteed by the project's threat model with
    /// concrete confidentiality, integrity, or availability impact.
    Vulnerability,
    /// Weakens a defense-in-depth mitigation or hardening mechanism without a
    /// standalone boundary-crossing exploit path.
    Hardening,
    /// Regular defect outside any guaranteed security trust boundary.
    NotSecurity,
}

impl SecurityClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vulnerability => "vulnerability",
            Self::Hardening => "hardening",
            Self::NotSecurity => "not_security",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Vulnerability => "Vulnerability",
            Self::Hardening => "Hardening",
            Self::NotSecurity => "Not Security",
        }
    }
}

impl fmt::Display for SecurityClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SecurityClass {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "vulnerability" => Ok(Self::Vulnerability),
            "hardening" => Ok(Self::Hardening),
            "not_security" | "not-security" | "non_security" | "non-security" => {
                Ok(Self::NotSecurity)
            }
            other => Err(format!(
                "invalid security_class {other:?}; expected one of \"vulnerability\", \"hardening\", or \"not_security\""
            )),
        }
    }
}

/// Qualitative FIRST CVSS v4.0 severity rating derived from `score_tenths`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SecurityRating {
    None,
    Low,
    Medium,
    High,
    Critical,
}

impl SecurityRating {
    pub const fn from_score_tenths(score_tenths: u8) -> Self {
        match score_tenths {
            0 => Self::None,
            1..=39 => Self::Low,
            40..=69 => Self::Medium,
            70..=89 => Self::High,
            _ => Self::Critical,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Critical => "Critical",
        }
    }
}

impl fmt::Display for SecurityRating {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// CVSS v4.0 Attack Vector (`AV`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttackVector {
    /// Network (`N`)
    N,
    /// Adjacent (`A`)
    A,
    /// Local (`L`)
    L,
    /// Physical (`P`)
    P,
}

impl AttackVector {
    pub const fn code(self) -> &'static str {
        match self {
            Self::N => "N",
            Self::A => "A",
            Self::L => "L",
            Self::P => "P",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "N" | "NETWORK" => Ok(Self::N),
            "A" | "ADJACENT" => Ok(Self::A),
            "L" | "LOCAL" => Ok(Self::L),
            "P" | "PHYSICAL" => Ok(Self::P),
            other => Err(format!(
                "invalid cvss_v4.av {other:?}; expected \"N\" (Network), \"A\" (Adjacent), \"L\" (Local), or \"P\" (Physical)"
            )),
        }
    }
}

/// CVSS v4.0 Attack Complexity (`AC`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttackComplexity {
    /// Low (`L`)
    L,
    /// High (`H`)
    H,
}

impl AttackComplexity {
    pub const fn code(self) -> &'static str {
        match self {
            Self::L => "L",
            Self::H => "H",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "L" | "LOW" => Ok(Self::L),
            "H" | "HIGH" => Ok(Self::H),
            other => Err(format!(
                "invalid cvss_v4.ac {other:?}; expected \"L\" (Low) or \"H\" (High)"
            )),
        }
    }
}

/// CVSS v4.0 Attack Requirements (`AT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttackRequirements {
    /// None (`N`)
    N,
    /// Present (`P`)
    P,
}

impl AttackRequirements {
    pub const fn code(self) -> &'static str {
        match self {
            Self::N => "N",
            Self::P => "P",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "N" | "NONE" => Ok(Self::N),
            "P" | "PRESENT" => Ok(Self::P),
            other => Err(format!(
                "invalid cvss_v4.at {other:?}; expected \"N\" (None) or \"P\" (Present)"
            )),
        }
    }
}

/// CVSS v4.0 Privileges Required (`PR`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivilegesRequired {
    /// None (`N`)
    N,
    /// Low (`L`)
    L,
    /// High (`H`)
    H,
}

impl PrivilegesRequired {
    pub const fn code(self) -> &'static str {
        match self {
            Self::N => "N",
            Self::L => "L",
            Self::H => "H",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "N" | "NONE" => Ok(Self::N),
            "L" | "LOW" => Ok(Self::L),
            "H" | "HIGH" => Ok(Self::H),
            other => Err(format!(
                "invalid cvss_v4.pr {other:?}; expected \"N\" (None), \"L\" (Low), or \"H\" (High)"
            )),
        }
    }
}

/// CVSS v4.0 User Interaction (`UI`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserInteraction {
    /// None (`N`)
    N,
    /// Passive (`P`)
    P,
    /// Active (`A`)
    A,
}

impl UserInteraction {
    pub const fn code(self) -> &'static str {
        match self {
            Self::N => "N",
            Self::P => "P",
            Self::A => "A",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "N" | "NONE" => Ok(Self::N),
            "P" | "PASSIVE" => Ok(Self::P),
            "A" | "ACTIVE" => Ok(Self::A),
            other => Err(format!(
                "invalid cvss_v4.ui {other:?}; expected \"N\" (None), \"P\" (Passive), or \"A\" (Active)"
            )),
        }
    }
}

/// CVSS v4.0 Confidentiality / Integrity / Availability impact level (`VC`, `VI`,
/// `VA`, `SC`, `SI`, `SA`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImpactLevel {
    /// None (`N`)
    N,
    /// Low (`L`)
    L,
    /// High (`H`)
    H,
}

impl ImpactLevel {
    pub const fn code(self) -> &'static str {
        match self {
            Self::N => "N",
            Self::L => "L",
            Self::H => "H",
        }
    }

    pub const fn is_non_zero(self) -> bool {
        !matches!(self, Self::N)
    }

    fn parse(field: &str, raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "N" | "NONE" => Ok(Self::N),
            "L" | "LOW" => Ok(Self::L),
            "H" | "HIGH" => Ok(Self::H),
            other => Err(format!(
                "invalid cvss_v4.{field} {other:?}; expected \"N\" (None), \"L\" (Low), or \"H\" (High)"
            )),
        }
    }
}

/// Optional CVSS v4.0 Supplemental Automatable metric (`AU`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Automatable {
    /// No (`N`)
    N,
    /// Yes (`Y`)
    Y,
}

impl Automatable {
    pub const fn code(self) -> &'static str {
        match self {
            Self::N => "N",
            Self::Y => "Y",
        }
    }

    fn parse_optional(raw: Option<&str>) -> Result<Option<Self>, String> {
        let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        match s.to_ascii_uppercase().as_str() {
            "X" | "NOT_DEFINED" => Ok(None),
            "N" | "NO" => Ok(Some(Self::N)),
            "Y" | "YES" => Ok(Some(Self::Y)),
            other => Err(format!(
                "invalid cvss_v4.au {other:?}; expected \"N\", \"Y\", or null"
            )),
        }
    }
}

/// Optional CVSS v4.0 Supplemental Recovery metric (`R`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recovery {
    /// Automatic (`A`)
    A,
    /// User (`U`)
    U,
    /// Irrecoverable (`I`)
    I,
}

impl Recovery {
    pub const fn code(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::U => "U",
            Self::I => "I",
        }
    }

    fn parse_optional(raw: Option<&str>) -> Result<Option<Self>, String> {
        let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        match s.to_ascii_uppercase().as_str() {
            "X" | "NOT_DEFINED" => Ok(None),
            "A" | "AUTOMATIC" => Ok(Some(Self::A)),
            "U" | "USER" => Ok(Some(Self::U)),
            "I" | "IRRECOVERABLE" => Ok(Some(Self::I)),
            other => Err(format!(
                "invalid cvss_v4.r {other:?}; expected \"A\" (Automatic), \"U\" (User), \"I\" (Irrecoverable), or null"
            )),
        }
    }
}

/// Optional CVSS v4.0 Supplemental Vulnerability Response Effort metric (`RE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseEffort {
    /// Low (`L`)
    L,
    /// Moderate (`M`)
    M,
    /// High (`H`)
    H,
}

impl ResponseEffort {
    pub const fn code(self) -> &'static str {
        match self {
            Self::L => "L",
            Self::M => "M",
            Self::H => "H",
        }
    }

    fn parse_optional(raw: Option<&str>) -> Result<Option<Self>, String> {
        let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        match s.to_ascii_uppercase().as_str() {
            "X" | "NOT_DEFINED" => Ok(None),
            "L" | "LOW" => Ok(Some(Self::L)),
            "M" | "MODERATE" => Ok(Some(Self::M)),
            "H" | "HIGH" => Ok(Some(Self::H)),
            other => Err(format!(
                "invalid cvss_v4.re {other:?}; expected \"L\" (Low), \"M\" (Moderate), \"H\" (High), or null"
            )),
        }
    }
}

/// Validated CVSS v4.0 Base and optional Supplemental metrics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CvssV4Metrics {
    pub av: AttackVector,
    pub ac: AttackComplexity,
    pub at: AttackRequirements,
    pub pr: PrivilegesRequired,
    pub ui: UserInteraction,
    pub vc: ImpactLevel,
    pub vi: ImpactLevel,
    pub va: ImpactLevel,
    pub sc: ImpactLevel,
    pub si: ImpactLevel,
    pub sa: ImpactLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub au: Option<Automatable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r: Option<Recovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re: Option<ResponseEffort>,
}

/// Deterministically computed CVSS v4.0 vector and score.
#[derive(Clone, Debug, PartialEq)]
pub struct ComputedCvssV4 {
    pub vector_string: String,
    pub score_tenths: u8,
    pub score: f64,
    pub rating: SecurityRating,
}

impl CvssV4Metrics {
    pub const fn has_any_impact(&self) -> bool {
        self.vc.is_non_zero()
            || self.vi.is_non_zero()
            || self.va.is_non_zero()
            || self.sc.is_non_zero()
            || self.si.is_non_zero()
            || self.sa.is_non_zero()
    }

    /// Builds and validates a canonical FIRST CVSS v4.0 [`Vector`] and computes
    /// its deterministic MacroVector score.
    pub fn compute_score(&self) -> Result<ComputedCvssV4, String> {
        let mut raw = format!(
            "CVSS:4.0/AV:{}/AC:{}/AT:{}/PR:{}/UI:{}/VC:{}/VI:{}/VA:{}/SC:{}/SI:{}/SA:{}",
            self.av.code(),
            self.ac.code(),
            self.at.code(),
            self.pr.code(),
            self.ui.code(),
            self.vc.code(),
            self.vi.code(),
            self.va.code(),
            self.sc.code(),
            self.si.code(),
            self.sa.code(),
        );
        if let Some(au) = self.au {
            raw.push_str("/AU:");
            raw.push_str(au.code());
        }
        if let Some(r) = self.r {
            raw.push_str("/R:");
            raw.push_str(r.code());
        }
        if let Some(re) = self.re {
            raw.push_str("/RE:");
            raw.push_str(re.code());
        }

        let vector = Vector::from_str(&raw)
            .map_err(|e| format!("failed to construct valid CVSS v4.0 vector from {raw}: {e}"))?;
        let canonical = vector.to_string();
        let val = vector.score().value().clamp(0.0, 10.0);
        let score_tenths = (val * 10.0).round() as u8;
        let clamped_tenths = score_tenths.min(100);
        Ok(ComputedCvssV4 {
            vector_string: canonical,
            score_tenths: clamped_tenths,
            score: f64::from(clamped_tenths) / 10.0,
            rating: SecurityRating::from_score_tenths(clamped_tenths),
        })
    }
}

/// Code-grounded justifications for CVSS v4.0 metric groups.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricJustifications {
    pub exploitability: String,
    pub vulnerable_system_impact: String,
    pub subsequent_system_impact: String,
}

/// Validated, canonical security evaluation stored on `bug_enrichments` and
/// projected onto `bugs`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SecurityEvaluation {
    pub security_class: SecurityClass,
    pub score: f64,
    pub score_tenths: u8,
    pub qualitative_rating: SecurityRating,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cvss_vector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_boundary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attacker_model: Option<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cvss_v4: Option<CvssV4Metrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric_justifications: Option<MetricJustifications>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluated_on_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threat_model_sha: Option<String>,
}

/// Raw CVSS v4.0 metric block produced by the LLM before validation.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RawCvssV4Metrics {
    #[serde(default)]
    pub av: String,
    #[serde(default)]
    pub ac: String,
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub pr: String,
    #[serde(default)]
    pub ui: String,
    #[serde(default)]
    pub vc: String,
    #[serde(default)]
    pub vi: String,
    #[serde(default)]
    pub va: String,
    #[serde(default)]
    pub sc: String,
    #[serde(default)]
    pub si: String,
    #[serde(default)]
    pub sa: String,
    #[serde(default)]
    pub au: Option<String>,
    #[serde(default)]
    pub r: Option<String>,
    #[serde(default)]
    pub re: Option<String>,
}

impl RawCvssV4Metrics {
    pub fn into_validated(self) -> Result<CvssV4Metrics, String> {
        Ok(CvssV4Metrics {
            av: AttackVector::parse(&self.av)?,
            ac: AttackComplexity::parse(&self.ac)?,
            at: AttackRequirements::parse(&self.at)?,
            pr: PrivilegesRequired::parse(&self.pr)?,
            ui: UserInteraction::parse(&self.ui)?,
            vc: ImpactLevel::parse("vc", &self.vc)?,
            vi: ImpactLevel::parse("vi", &self.vi)?,
            va: ImpactLevel::parse("va", &self.va)?,
            sc: ImpactLevel::parse("sc", &self.sc)?,
            si: ImpactLevel::parse("si", &self.si)?,
            sa: ImpactLevel::parse("sa", &self.sa)?,
            au: Automatable::parse_optional(self.au.as_deref())?,
            r: Recovery::parse_optional(self.r.as_deref())?,
            re: ResponseEffort::parse_optional(self.re.as_deref())?,
        })
    }
}

/// Raw metric justifications block produced by the LLM before validation.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RawMetricJustifications {
    #[serde(default)]
    pub exploitability: String,
    #[serde(default)]
    pub vulnerable_system_impact: String,
    #[serde(default)]
    pub subsequent_system_impact: String,
}

/// Raw JSON output emitted by `SecurityEvalSession` prior to validation and
/// deterministic CVSS v4.0 score calculation.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RawSecurityEvaluationOutput {
    #[serde(default)]
    pub security_class: String,
    #[serde(default)]
    pub trust_boundary: Option<String>,
    #[serde(default)]
    pub attacker_model: Option<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub cvss_v4: Option<RawCvssV4Metrics>,
    #[serde(default)]
    pub metric_justifications: Option<RawMetricJustifications>,
}

fn clean_optional_field(val: Option<String>) -> Option<String> {
    let trimmed = val?.trim().to_string();
    if trimmed.is_empty()
        || matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "none" | "n/a" | "null" | "not applicable"
        )
    {
        None
    } else {
        Some(trimmed)
    }
}

impl SecurityEvaluation {
    /// Validates raw LLM output and deterministically computes the canonical
    /// CVSS v4.0 vector string, integer score tenths (`0..=100`), and rating.
    pub fn from_raw_output(
        raw: RawSecurityEvaluationOutput,
        evaluated_on_sha: Option<String>,
        threat_model_sha: Option<String>,
    ) -> Result<Self, String> {
        let security_class = SecurityClass::from_str(&raw.security_class)?;
        let summary = raw.summary.trim().to_string();
        if summary.len() < 15 {
            return Err(
                "summary must be a non-empty code-grounded explanation (at least 15 characters)"
                    .to_string(),
            );
        }

        let trust_boundary = clean_optional_field(raw.trust_boundary);
        let attacker_model = clean_optional_field(raw.attacker_model);

        match security_class {
            SecurityClass::Vulnerability => Self::build_vulnerability(
                summary,
                trust_boundary,
                attacker_model,
                raw.cvss_v4,
                raw.metric_justifications,
                evaluated_on_sha,
                threat_model_sha,
            ),
            SecurityClass::Hardening | SecurityClass::NotSecurity => {
                if raw.cvss_v4.is_some() {
                    return Err(format!(
                        "cvss_v4 must be null when security_class is {:?}; set cvss_v4 to null for non-vulnerabilities or change security_class to \"vulnerability\" if a guaranteed trust boundary is breached",
                        security_class.as_str()
                    ));
                }
                Ok(Self {
                    security_class,
                    score: 0.0,
                    score_tenths: 0,
                    qualitative_rating: SecurityRating::None,
                    cvss_vector: None,
                    trust_boundary,
                    attacker_model,
                    summary,
                    cvss_v4: None,
                    metric_justifications: None,
                    evaluated_on_sha,
                    threat_model_sha,
                })
            }
        }
    }

    fn build_vulnerability(
        summary: String,
        trust_boundary: Option<String>,
        attacker_model: Option<String>,
        raw_metrics: Option<RawCvssV4Metrics>,
        raw_justifications: Option<RawMetricJustifications>,
        evaluated_on_sha: Option<String>,
        threat_model_sha: Option<String>,
    ) -> Result<Self, String> {
        if trust_boundary.is_none() {
            return Err(
                "trust_boundary must be a non-empty description of the breached security boundary when security_class is \"vulnerability\""
                    .to_string(),
            );
        }
        if attacker_model.is_none() {
            return Err(
                "attacker_model must be a non-empty description of who can trigger the bug and what privileges/capabilities are required when security_class is \"vulnerability\""
                    .to_string(),
            );
        }
        let raw_metrics = raw_metrics.ok_or_else(|| {
            "cvss_v4 object is required when security_class is \"vulnerability\"".to_string()
        })?;
        let metrics = raw_metrics.into_validated()?;
        if !metrics.has_any_impact() {
            return Err(
                "cvss_v4 has all six impact metrics (vc, vi, va, sc, si, sa) set to \"N\" (score 0.0), which contradicts security_class \"vulnerability\"; specify the concrete non-zero impact metric or reclassify as \"hardening\" or \"not_security\""
                    .to_string(),
            );
        }

        let justifications = validate_justifications(raw_justifications)?;
        let computed = metrics.compute_score()?;

        Ok(Self {
            security_class: SecurityClass::Vulnerability,
            score: computed.score,
            score_tenths: computed.score_tenths,
            qualitative_rating: computed.rating,
            cvss_vector: Some(computed.vector_string),
            trust_boundary,
            attacker_model,
            summary,
            cvss_v4: Some(metrics),
            metric_justifications: Some(justifications),
            evaluated_on_sha,
            threat_model_sha,
        })
    }
}

fn validate_justifications(
    raw: Option<RawMetricJustifications>,
) -> Result<MetricJustifications, String> {
    let raw = raw.ok_or_else(|| {
        "metric_justifications object is required when security_class is \"vulnerability\""
            .to_string()
    })?;
    let exploitability = raw.exploitability.trim().to_string();
    let vulnerable_system_impact = raw.vulnerable_system_impact.trim().to_string();
    let subsequent_system_impact = raw.subsequent_system_impact.trim().to_string();
    if exploitability.is_empty() {
        return Err(
            "metric_justifications.exploitability must be non-empty when security_class is \"vulnerability\""
                .to_string(),
        );
    }
    if vulnerable_system_impact.is_empty() {
        return Err(
            "metric_justifications.vulnerable_system_impact must be non-empty when security_class is \"vulnerability\""
                .to_string(),
        );
    }
    if subsequent_system_impact.is_empty() {
        return Err(
            "metric_justifications.subsequent_system_impact must be non-empty when security_class is \"vulnerability\""
                .to_string(),
        );
    }
    Ok(MetricJustifications {
        exploitability,
        vulnerable_system_impact,
        subsequent_system_impact,
    })
}

/// Parses a user-supplied CVSS score filter (`"0"`..=`"10.0"`) into integer
/// tenths (`0..=100`), rejecting NaN, infinities, negative numbers, values
/// above `10.0`, or precision finer than one decimal place.
pub fn parse_security_score_tenths(raw: &str) -> Result<u8, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("security score cannot be empty".to_string());
    }
    if trimmed.starts_with('-') {
        return Err(format!(
            "security score {trimmed:?} must be between 0.0 and 10.0"
        ));
    }
    let body = trimmed.strip_prefix('+').unwrap_or(trimmed);
    let tenths: u32 = match body.split_once('.') {
        None => {
            if !body.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!(
                    "invalid security score {trimmed:?}; expected a number between 0.0 and 10.0"
                ));
            }
            let whole = body
                .parse::<u32>()
                .map_err(|_| format!("security score {trimmed:?} must be between 0.0 and 10.0"))?;
            whole.saturating_mul(10)
        }
        Some((whole_str, frac_str)) => {
            if whole_str.is_empty()
                || frac_str.is_empty()
                || frac_str.len() > 1
                || !whole_str.bytes().all(|b| b.is_ascii_digit())
                || !frac_str.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(format!(
                    "invalid security score {trimmed:?}; expected a number between 0.0 and 10.0 with at most one decimal place"
                ));
            }
            let whole = whole_str
                .parse::<u32>()
                .map_err(|_| format!("security score {trimmed:?} must be between 0.0 and 10.0"))?;
            let frac = frac_str
                .parse::<u32>()
                .map_err(|_| format!("security score {trimmed:?} must be between 0.0 and 10.0"))?;
            whole.saturating_mul(10).saturating_add(frac)
        }
    };

    if tenths > 100 {
        return Err(format!(
            "security score {trimmed:?} exceeds maximum CVSS score 10.0"
        ));
    }
    Ok(tenths as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_justifications() -> RawMetricJustifications {
        RawMetricJustifications {
            exploitability: "Unprivileged local user invokes ioctl without capability check."
                .to_string(),
            vulnerable_system_impact:
                "Heap out-of-bounds write corrupts kernel objects in host namespace.".to_string(),
            subsequent_system_impact: "No separate subsequent system beyond the host kernel."
                .to_string(),
        }
    }

    #[test]
    fn computes_official_first_cvss_v4_examples() {
        // Local Linux kernel privilege escalation (AV:L/AC:L/AT:N/PR:L/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N -> 8.5 High).
        let lpe = CvssV4Metrics {
            av: AttackVector::L,
            ac: AttackComplexity::L,
            at: AttackRequirements::N,
            pr: PrivilegesRequired::L,
            ui: UserInteraction::N,
            vc: ImpactLevel::H,
            vi: ImpactLevel::H,
            va: ImpactLevel::H,
            sc: ImpactLevel::N,
            si: ImpactLevel::N,
            sa: ImpactLevel::N,
            au: Some(Automatable::Y),
            r: Some(Recovery::U),
            re: Some(ResponseEffort::L),
        }
        .compute_score()
        .unwrap();
        assert_eq!(
            lpe.vector_string,
            "CVSS:4.0/AV:L/AC:L/AT:N/PR:L/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N/AU:Y/R:U/RE:L"
        );
        assert_eq!(lpe.score_tenths, 85);
        assert_eq!(lpe.score, 8.5);
        assert_eq!(lpe.rating, SecurityRating::High);

        // Heartbleed-style remote memory disclosure (CVE-2014-0160 Base score:
        // CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:N/VA:N/SC:L/SI:L/SA:N -> 8.7 High).
        let heartbleed = CvssV4Metrics {
            av: AttackVector::N,
            ac: AttackComplexity::L,
            at: AttackRequirements::N,
            pr: PrivilegesRequired::N,
            ui: UserInteraction::N,
            vc: ImpactLevel::H,
            vi: ImpactLevel::N,
            va: ImpactLevel::N,
            sc: ImpactLevel::L,
            si: ImpactLevel::L,
            sa: ImpactLevel::N,
            au: None,
            r: None,
            re: None,
        }
        .compute_score()
        .unwrap();
        assert_eq!(heartbleed.score_tenths, 87);
        assert_eq!(heartbleed.score, 8.7);
        assert_eq!(heartbleed.rating, SecurityRating::High);

        // regreSSHion unauthenticated race RCE (CVE-2024-6387:
        // CVSS:4.0/AV:N/AC:H/AT:P/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N -> 9.2 Critical).
        let regresshion = CvssV4Metrics {
            av: AttackVector::N,
            ac: AttackComplexity::H,
            at: AttackRequirements::P,
            pr: PrivilegesRequired::N,
            ui: UserInteraction::N,
            vc: ImpactLevel::H,
            vi: ImpactLevel::H,
            va: ImpactLevel::H,
            sc: ImpactLevel::N,
            si: ImpactLevel::N,
            sa: ImpactLevel::N,
            au: None,
            r: None,
            re: None,
        }
        .compute_score()
        .unwrap();
        assert_eq!(regresshion.score_tenths, 92);
        assert_eq!(regresshion.score, 9.2);
        assert_eq!(regresshion.rating, SecurityRating::Critical);
    }

    #[test]
    fn validates_vulnerability_and_normalizes_non_security_and_hardening() {
        let vuln = SecurityEvaluation::from_raw_output(
            RawSecurityEvaluationOutput {
                security_class: "vulnerability".to_string(),
                trust_boundary: Some("Unprivileged userspace syscall -> kernel heap".to_string()),
                attacker_model: Some("Local unprivileged user invoking ioctl".to_string()),
                summary: "Unchecked length in foo_ioctl() allows local heap OOB write.".to_string(),
                cvss_v4: Some(RawCvssV4Metrics {
                    av: "Local".to_string(),
                    ac: "L".to_string(),
                    at: "None".to_string(),
                    pr: "L".to_string(),
                    ui: "N".to_string(),
                    vc: "H".to_string(),
                    vi: "H".to_string(),
                    va: "H".to_string(),
                    sc: "N".to_string(),
                    si: "N".to_string(),
                    sa: "N".to_string(),
                    au: Some("X".to_string()),
                    r: None,
                    re: Some("Low".to_string()),
                }),
                metric_justifications: Some(sample_justifications()),
            },
            Some("deadbeef1234".to_string()),
            Some("05441d697f7f".to_string()),
        )
        .unwrap();
        assert_eq!(vuln.security_class, SecurityClass::Vulnerability);
        assert_eq!(vuln.score_tenths, 85);
        assert_eq!(vuln.qualitative_rating, SecurityRating::High);
        assert_eq!(
            vuln.cvss_vector.as_deref(),
            Some("CVSS:4.0/AV:L/AC:L/AT:N/PR:L/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N/RE:L")
        );

        let non_sec = SecurityEvaluation::from_raw_output(
            RawSecurityEvaluationOutput {
                security_class: "not_security".to_string(),
                trust_boundary: Some("N/A".to_string()),
                attacker_model: Some("Requires CAP_SYS_ADMIN via debugfs write".to_string()),
                summary: "Debugfs handler requires root (0600) and CAP_SYS_ADMIN; excluded by kernel threat model.".to_string(),
                cvss_v4: None,
                metric_justifications: None,
            },
            Some("deadbeef1234".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(non_sec.security_class, SecurityClass::NotSecurity);
        assert_eq!(non_sec.score_tenths, 0);
        assert_eq!(non_sec.score, 0.0);
        assert_eq!(non_sec.qualitative_rating, SecurityRating::None);
        assert!(non_sec.cvss_vector.is_none());
        assert!(non_sec.trust_boundary.is_none());
        assert_eq!(
            non_sec.attacker_model.as_deref(),
            Some("Requires CAP_SYS_ADMIN via debugfs write")
        );
    }

    #[test]
    fn rejects_contradictory_outputs_with_actionable_errors() {
        // 1. Vulnerability with zero impact (all N).
        let zero_impact = SecurityEvaluation::from_raw_output(
            RawSecurityEvaluationOutput {
                security_class: "vulnerability".to_string(),
                trust_boundary: Some("Syscall boundary".to_string()),
                attacker_model: Some("Local user".to_string()),
                summary: "Alleged vulnerability with zero CIA impact.".to_string(),
                cvss_v4: Some(RawCvssV4Metrics {
                    av: "L".to_string(),
                    ac: "L".to_string(),
                    at: "N".to_string(),
                    pr: "L".to_string(),
                    ui: "N".to_string(),
                    vc: "N".to_string(),
                    vi: "N".to_string(),
                    va: "N".to_string(),
                    sc: "N".to_string(),
                    si: "N".to_string(),
                    sa: "N".to_string(),
                    ..Default::default()
                }),
                metric_justifications: Some(sample_justifications()),
            },
            None,
            None,
        )
        .unwrap_err();
        assert!(zero_impact.contains("contradicts security_class \"vulnerability\""));

        // 2. Non-security bug providing cvss_v4 metrics.
        let non_sec_with_cvss = SecurityEvaluation::from_raw_output(
            RawSecurityEvaluationOutput {
                security_class: "hardening".to_string(),
                trust_boundary: None,
                attacker_model: None,
                summary: "Defense-in-depth bounds check without exploit path.".to_string(),
                cvss_v4: Some(RawCvssV4Metrics {
                    av: "L".to_string(),
                    ac: "H".to_string(),
                    at: "P".to_string(),
                    pr: "H".to_string(),
                    ui: "N".to_string(),
                    vc: "L".to_string(),
                    vi: "N".to_string(),
                    va: "N".to_string(),
                    sc: "N".to_string(),
                    si: "N".to_string(),
                    sa: "N".to_string(),
                    ..Default::default()
                }),
                metric_justifications: None,
            },
            None,
            None,
        )
        .unwrap_err();
        assert!(non_sec_with_cvss.contains("cvss_v4 must be null"));

        // 3. Vulnerability missing trust_boundary.
        let missing_boundary = SecurityEvaluation::from_raw_output(
            RawSecurityEvaluationOutput {
                security_class: "vulnerability".to_string(),
                trust_boundary: Some("none".to_string()),
                attacker_model: Some("Local user".to_string()),
                summary: "Unprivileged local DoS via NULL dereference in syscall.".to_string(),
                cvss_v4: Some(RawCvssV4Metrics {
                    av: "L".to_string(),
                    ac: "L".to_string(),
                    at: "N".to_string(),
                    pr: "L".to_string(),
                    ui: "N".to_string(),
                    vc: "N".to_string(),
                    vi: "N".to_string(),
                    va: "H".to_string(),
                    sc: "N".to_string(),
                    si: "N".to_string(),
                    sa: "N".to_string(),
                    ..Default::default()
                }),
                metric_justifications: Some(sample_justifications()),
            },
            None,
            None,
        )
        .unwrap_err();
        assert!(missing_boundary.contains("trust_boundary"));
    }

    #[test]
    fn parses_security_score_filter_strictly() {
        assert_eq!(parse_security_score_tenths("0").unwrap(), 0);
        assert_eq!(parse_security_score_tenths("0.0").unwrap(), 0);
        assert_eq!(parse_security_score_tenths("7").unwrap(), 70);
        assert_eq!(parse_security_score_tenths("7.5").unwrap(), 75);
        assert_eq!(parse_security_score_tenths("10").unwrap(), 100);
        assert_eq!(parse_security_score_tenths("10.0").unwrap(), 100);

        assert!(parse_security_score_tenths("").is_err());
        assert!(parse_security_score_tenths("-0.1").is_err());
        assert!(parse_security_score_tenths("10.1").is_err());
        assert!(parse_security_score_tenths("7.55").is_err());
        assert!(parse_security_score_tenths("NaN").is_err());
        assert!(parse_security_score_tenths("inf").is_err());
    }
}
