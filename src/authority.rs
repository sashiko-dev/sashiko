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

//! Deterministic contributor authority resolution.
//!
//! Authority derives only from verifiable identity facts: the speaker's
//! canonical address (via `.mailmap`), its `MAINTAINERS` role (`M:` and `R:`
//! both count as maintainers) for the files touched by `vN`, membership in the
//! project's whitelist of trusted automated bots, and an authenticated forge
//! repository association. Commit history and contribution counts are
//! deliberately not used: they are expensive to compute, easy to game, and
//! only speculatively related to whether a claim is right. Authority is used
//! strictly to weigh claims that cannot be proven from repository code; it
//! never overrides concrete code evidence and never grants prompt-instruction
//! authority.

use crate::maintainers::{MaintainersIndex, get_global_maintainers, maintainer_address};
use crate::patch::{authors_match, extract_email};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

/// Trust tier derived from `MAINTAINERS` role, trusted-bot whitelist
/// membership, or authenticated forge repository association.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    Low,
    Moderate,
    High,
}

impl fmt::Display for TrustTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrustTier::Low => write!(f, "low"),
            TrustTier::Moderate => write!(f, "moderate"),
            TrustTier::High => write!(f, "high"),
        }
    }
}

/// Speaker relationship to the patchset under review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerRole {
    Reviewer,
    PatchAuthor,
    AutomatedBot,
}

impl fmt::Display for SpeakerRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpeakerRole::Reviewer => write!(f, "reviewer"),
            SpeakerRole::PatchAuthor => write!(f, "patch_author"),
            SpeakerRole::AutomatedBot => write!(f, "automated_bot"),
        }
    }
}

/// Speaker role in `MAINTAINERS` relative to the files touched by `vN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintainerRole {
    SubsystemMaintainer,
    DesignatedReviewer,
    OtherSubsystemMaintainer,
    None,
}

impl fmt::Display for MaintainerRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MaintainerRole::SubsystemMaintainer => write!(f, "subsystem_maintainer"),
            MaintainerRole::DesignatedReviewer => write!(f, "designated_reviewer"),
            MaintainerRole::OtherSubsystemMaintainer => write!(f, "other_subsystem_maintainer"),
            MaintainerRole::None => write!(f, "none"),
        }
    }
}

/// Deterministic authority summary attached to prior-version speakers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributorAuthority {
    pub canonical_email: String,
    pub tier: TrustTier,
    pub speaker_role: SpeakerRole,
    pub maintainer_role: MaintainerRole,
    pub unverifiable_claim_trust: TrustTier,
    pub matched_subsystems: Vec<String>,
}

impl ContributorAuthority {
    /// Effective trust tier when evaluating a claim that cannot be proven or
    /// disproven from repository source code alone.
    ///
    /// Self-serving unverifiable claims from a `PatchAuthor` are downgraded to
    /// `Low` unless the author is an `M:`/`R:` maintainer of the touched
    /// subsystem. Automated bots keep their tier, which is `High` only for
    /// whitelisted bots.
    pub fn effective_unverifiable_trust(
        tier: TrustTier,
        speaker_role: SpeakerRole,
        maintainer_role: MaintainerRole,
    ) -> TrustTier {
        match speaker_role {
            SpeakerRole::Reviewer | SpeakerRole::AutomatedBot => tier,
            SpeakerRole::PatchAuthor => match maintainer_role {
                MaintainerRole::SubsystemMaintainer | MaintainerRole::DesignatedReviewer => {
                    TrustTier::High
                }
                MaintainerRole::OtherSubsystemMaintainer | MaintainerRole::None => TrustTier::Low,
            },
        }
    }

    /// Elevates maintainer role and trust tier when an authenticated forge API
    /// confirms repository maintainer permission (`OWNER`, `MEMBER`, or
    /// `COLLABORATOR`), while preserving `AutomatedBot` and `PatchAuthor` rules.
    pub fn with_forge_association(mut self, author_association: Option<&str>) -> Self {
        let is_repo_maintainer = author_association.is_some_and(|assoc| {
            matches!(
                assoc.trim().to_ascii_uppercase().as_str(),
                "OWNER" | "MEMBER" | "COLLABORATOR"
            )
        });
        if is_repo_maintainer && self.speaker_role != SpeakerRole::AutomatedBot {
            if matches!(
                self.maintainer_role,
                MaintainerRole::None | MaintainerRole::OtherSubsystemMaintainer
            ) {
                self.maintainer_role = MaintainerRole::SubsystemMaintainer;
            }
            self.tier = TrustTier::High;
            self.unverifiable_claim_trust = Self::effective_unverifiable_trust(
                self.tier,
                self.speaker_role,
                self.maintainer_role,
            );
        }
        self
    }
}

/// Project whitelist of automated bots whose reports (build failures,
/// sanitizer splats, reproducers) are trusted. Entries are exact lowercase
/// addresses (`lkp@intel.com`) or whole-domain patterns (`*@domain`); display
/// names are never consulted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrustedBots(pub &'static [&'static str]);

impl TrustedBots {
    /// Returns whether `canonical_email` matches a whitelist entry.
    pub fn contains(&self, canonical_email: &str) -> bool {
        let email = canonical_email.trim().to_ascii_lowercase();
        if email.is_empty() {
            return false;
        }
        self.0.iter().any(|entry| match entry.strip_prefix("*@") {
            Some(domain) => email
                .rsplit_once('@')
                .is_some_and(|(local, d)| !local.is_empty() && d == domain),
            None => email == *entry,
        })
    }
}

/// Parsed `.mailmap` identity canonicalizer following `gitmailmap(5)` rules.
#[derive(Debug, Clone, Default)]
pub struct Mailmap {
    /// Pure `<proper@email> <commit@email>` mappings (lowercased).
    email_to_canonical: HashMap<String, String>,
    /// Name-qualified `Proper <proper@email> Commit Name <commit@email>` mappings.
    name_email_to_canonical: HashMap<(String, String), String>,
    /// All canonical target emails appearing in `.mailmap`.
    known_canonicals: HashSet<String>,
}

impl Mailmap {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_reader<R: BufRead>(reader: R) -> Self {
        let mut map = Self::default();
        for line_res in reader.lines() {
            let Ok(line) = line_res else {
                continue;
            };
            map.insert_mailmap_line(&line);
        }
        map
    }

    pub fn from_repo(repo_path: &Path) -> Self {
        let disk_path = repo_path.join(".mailmap");
        if let Ok(file) = std::fs::File::open(&disk_path) {
            return Self::from_reader(BufReader::new(file));
        }
        for git_ref in [
            "origin/master:.mailmap",
            "master:.mailmap",
            "origin/main:.mailmap",
            "main:.mailmap",
            "HEAD:.mailmap",
        ] {
            if let Ok(out) = crate::git_cmd::in_dir(repo_path)
                .args(["show", git_ref])
                .output()
                && out.status.success()
                && !out.stdout.is_empty()
            {
                return Self::from_reader(BufReader::new(&out.stdout[..]));
            }
        }
        Self::empty()
    }

    /// Reads `.mailmap` as recorded in `commit`, so identities resolve the
    /// same way for every review on that baseline. A commit without one
    /// yields an empty map.
    pub async fn from_commit(repo_path: &Path, commit: &str) -> Self {
        // A revision starting with '-' would be parsed by git as an option.
        if commit.starts_with('-') {
            return Self::empty();
        }
        let spec = format!("{commit}:.mailmap");
        let Ok(out) = crate::git_cmd::in_dir_async(repo_path)
            .args(["show", spec.as_str()])
            .output()
            .await
        else {
            return Self::empty();
        };
        if !out.status.success() || out.stdout.is_empty() {
            return Self::empty();
        }
        tokio::task::spawn_blocking(move || Self::from_reader(BufReader::new(&out.stdout[..])))
            .await
            .unwrap_or_default()
    }

    fn insert_mailmap_line(&mut self, raw_line: &str) {
        let stripped = strip_mailmap_comment(raw_line).trim();
        if stripped.is_empty() {
            return;
        }
        let brackets = extract_bracketed_spans(stripped);
        match brackets.as_slice() {
            [(_, _, only_email)] => {
                if let Some(clean) = normalize_email_token(only_email) {
                    self.known_canonicals.insert(clean);
                }
            }
            [
                (_, end_first, proper_raw),
                (start_second, _, commit_raw),
                ..,
            ] => {
                let Some(proper_email) = normalize_email_token(proper_raw) else {
                    return;
                };
                let Some(commit_email) = normalize_email_token(commit_raw) else {
                    return;
                };
                self.known_canonicals.insert(proper_email.clone());
                let between = stripped[*end_first + 1..*start_second].trim();
                if between.is_empty() {
                    if commit_email != proper_email {
                        self.email_to_canonical.insert(commit_email, proper_email);
                    }
                } else {
                    let norm_name = normalize_display_name(between);
                    if !norm_name.is_empty() {
                        self.name_email_to_canonical
                            .insert((norm_name, commit_email), proper_email);
                    }
                }
            }
            [] => {}
        }
    }

    /// Maps an already-extracted email (and optional display name) to its
    /// canonical `.mailmap` email. Display names alone never match anything.
    pub fn canonicalize_email_with_name(&self, display_name: Option<&str>, email: &str) -> String {
        let mut current = email.trim().trim_matches('"').to_ascii_lowercase();
        if current.is_empty() {
            return current;
        }
        if let Some(name) = display_name {
            let norm_name = normalize_display_name(name);
            if !norm_name.is_empty()
                && let Some(mapped) = self
                    .name_email_to_canonical
                    .get(&(norm_name, current.clone()))
            {
                current = mapped.clone();
            }
        }
        for _ in 0..4 {
            if let Some(next) = self.email_to_canonical.get(&current)
                && next != &current
            {
                current = next.clone();
            } else {
                break;
            }
        }
        current
    }

    pub fn canonicalize_email(&self, email: &str) -> String {
        self.canonicalize_email_with_name(None, email)
    }

    pub fn knows_email(&self, email: &str) -> bool {
        let lower = email.trim().to_ascii_lowercase();
        self.known_canonicals.contains(&lower) || self.email_to_canonical.contains_key(&lower)
    }
}

fn strip_mailmap_comment(line: &str) -> &str {
    let mut in_brackets = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '<' => in_brackets = true,
            '>' => in_brackets = false,
            '#' if !in_brackets => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn extract_bracketed_spans(s: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut open_idx: Option<usize> = None;
    for (idx, ch) in s.char_indices() {
        match ch {
            '<' => open_idx = Some(idx),
            '>' => {
                if let Some(start) = open_idx.take()
                    && idx > start + 1
                {
                    out.push((start, idx, &s[start + 1..idx]));
                }
            }
            _ => {}
        }
    }
    out
}

fn normalize_email_token(raw: &str) -> Option<String> {
    maintainer_address(raw)
}

fn normalize_display_name(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Splits `"Display Name <addr@domain>"` into `(Some("Display Name"), "addr@domain")`.
/// Never falls back to display name when no `@` address is present unless the
/// raw input is a Forge login handle (handled separately).
fn split_author_name_and_email(raw_author: &str) -> (Option<&str>, String) {
    let trimmed = raw_author.trim();
    if let (Some(open), Some(close)) = (trimmed.find('<'), trimmed.rfind('>'))
        && close > open + 1
    {
        let name_part = trimmed[..open].trim().trim_matches('"').trim();
        let email_part = trimmed[open + 1..close]
            .trim()
            .trim_matches('"')
            .to_ascii_lowercase();
        let opt_name = (!name_part.is_empty()).then_some(name_part);
        return (opt_name, email_part);
    }
    let extracted = extract_email(trimmed)
        .trim()
        .trim_matches('"')
        .to_ascii_lowercase();
    (None, extracted)
}

/// Decodes official `b4` web-relay addresses (`devnull+<encoded>@kernel.org` or
/// `@linux.dev`) into candidate underlying addresses ordered from most common
/// (`user@domain.tld`) to multi-label TLDs (`user@domain.co.uk`).
pub fn decode_b4_relay_candidates(email_lower: &str) -> Vec<String> {
    let Some(rest) = email_lower.strip_prefix("devnull+") else {
        return Vec::new();
    };
    let Some(at_idx) = rest.rfind('@') else {
        return Vec::new();
    };
    let encoded = &rest[..at_idx];
    let relay_domain = &rest[at_idx + 1..];
    if relay_domain != "kernel.org" && relay_domain != "linux.dev" {
        return Vec::new();
    }
    let dot_positions: Vec<usize> = encoded
        .char_indices()
        .filter_map(|(i, c)| (c == '.').then_some(i))
        .collect();
    let mut candidates = Vec::new();
    for &pos in dot_positions.iter().rev().skip(1).take(3) {
        let mut decoded = String::with_capacity(encoded.len());
        decoded.push_str(&encoded[..pos]);
        decoded.push('@');
        decoded.push_str(&encoded[pos + 1..]);
        if normalize_email_token(&decoded).is_some() && !candidates.contains(&decoded) {
            candidates.push(decoded);
        }
    }
    candidates
}

/// Canonicalizes a raw author header (`"Name <email>"`, bare email, `b4`
/// `devnull+` relay, or Forge login) strictly via email + `.mailmap`.
///
/// Display names alone never match another contributor: `"Linus Torvalds <x@gmail.com>"`
/// resolves strictly to `"x@gmail.com"`.
pub fn canonicalize_speaker_identity(
    raw_author: &str,
    mailmap: &Mailmap,
    is_known_canonical: &dyn Fn(&str) -> bool,
) -> String {
    let (opt_name, raw_email) = split_author_name_and_email(raw_author);
    if raw_email.is_empty() {
        return String::new();
    }

    let b4_candidates = decode_b4_relay_candidates(&raw_email);
    if !b4_candidates.is_empty() {
        let mut mapped_candidates = Vec::with_capacity(b4_candidates.len());
        for cand in b4_candidates {
            let clean_name = opt_name.map(|n| n.strip_suffix(" via B4 Relay").unwrap_or(n).trim());
            let mapped = mailmap.canonicalize_email_with_name(clean_name, &cand);
            if is_known_canonical(&mapped) || mailmap.knows_email(&cand) {
                return mapped;
            }
            mapped_candidates.push(mapped);
        }
        if let Some(primary) = mapped_candidates.into_iter().next() {
            return primary;
        }
    }

    mailmap.canonicalize_email_with_name(opt_name, &raw_email)
}

/// Returns `true` when `raw_author` / `canonical_email` is one of Sashiko's own
/// review identities across direct outbound mail, subsystem bot aliases (such as
/// `netdev-bot+sashiko@kernel.org`), Mailman DMARC rewrites (`sashiko-bot--- via ...`),
/// or Forge bot handles (`sashiko-bot`, `sashiko-bot[bot]`).
pub fn is_sashiko_identity(raw_author: &str, canonical_email: &str) -> bool {
    let lower_raw = raw_author
        .trim()
        .trim_matches(['"', '\''])
        .trim()
        .to_ascii_lowercase();
    let email = canonical_email.trim().to_ascii_lowercase();

    if email == "sashiko-bot"
        || email == "sashiko-bot[bot]"
        || email.starts_with("sashiko-bot@")
        || email.starts_with("sashiko@")
        || (email.starts_with("sashiko") && email.contains("bot"))
        || email.contains("+sashiko@")
        || email.contains("-sashiko@")
    {
        return true;
    }
    if lower_raw.starts_with("sashiko-bot--- via ") {
        return true;
    }
    !lower_raw.contains('@') && (lower_raw == "sashiko-bot" || lower_raw == "sashiko-bot[bot]")
}

/// Convenience wrapper around [`is_sashiko_identity`] when only the raw header
/// string is available.
pub fn is_sashiko_author(raw_author: &str) -> bool {
    let (_, email) = split_author_name_and_email(raw_author);
    is_sashiko_identity(raw_author, &email)
}

/// Detects known kernel and Forge automated CI / test bots strictly from email
/// or Forge account handle patterns (never from human display names).
pub fn is_automated_bot_identity(raw_author: &str, canonical_email: &str) -> bool {
    if is_sashiko_identity(raw_author, canonical_email) {
        return true;
    }
    let lower_raw = raw_author
        .trim()
        .trim_matches(['"', '\''])
        .trim()
        .to_ascii_lowercase();
    let email = canonical_email.trim().to_ascii_lowercase();

    if email.is_empty() {
        return false;
    }
    if email == "lkp@intel.com"
        || email == "bot@kernel.org"
        || email == "pr-tracker-bot@kernel.org"
        || ((email.starts_with("bot+")
            || email.contains("-bot+")
            || email.ends_with("-bot@kernel.org"))
            && email.ends_with("@kernel.org"))
        || email.starts_with("tip-bot")
        || email.ends_with("@syzkaller.appspotmail.com")
        || email.starts_with("syzbot+")
        || email.starts_with("patchwork@")
        || email.starts_with("patchwork-bot")
        || email.contains("patchew@")
        || email.ends_with("@patchew.org")
        || email.ends_with(".test.bot@gmail.com")
    {
        return true;
    }
    if email.contains("lists.")
        && lower_raw.contains("--- via ")
        && (lower_raw.starts_with("patchwork-bot") || lower_raw.starts_with("pr-tracker-bot"))
    {
        return true;
    }
    if email.ends_with("[bot]")
        || email == "dependabot"
        || email == "github-actions"
        || email == "gitlab-bot"
    {
        return true;
    }
    if let Some((local, domain)) = email.split_once('@')
        && (domain == "users.noreply.github.com" || domain == "github.com")
        && (local.ends_with("[bot]") || local.ends_with("-bot") || local == "dependabot")
    {
        return true;
    }
    // Bare Forge login in `"login[bot]"` form without `<...>`.
    !lower_raw.contains('@') && lower_raw.ends_with("[bot]")
}

/// Normalized display name used to recognize a patch author replying from
/// another address. Requires at least two name words so a bare first name
/// or handle never matches.
fn patch_author_name_key(raw_author: &str) -> Option<String> {
    let (name, _) = split_author_name_and_email(raw_author);
    let name = name?;
    let name = name.strip_suffix(" via B4 Relay").unwrap_or(name);
    let key = normalize_display_name(name);
    (key.split_whitespace().count() >= 2).then_some(key)
}

/// Matches a bare Forge login handle (such as `"octocat"`, `"Contributor"`, or
/// `"jane-doe"`, with no `@` or `<...>`) against a series/patch author string
/// `"Name <email>"` (and its `.mailmap`-canonicalized email `canon_pa`).
fn bare_forge_login_matches_patch_author(raw_speaker: &str, pa: &str, canon_pa: &str) -> bool {
    let trimmed = raw_speaker.trim().trim_matches(['"', '\'']).trim();
    if trimmed.is_empty() || trimmed.contains('@') || trimmed.contains('<') || trimmed.contains('>')
    {
        return false;
    }
    let login = normalize_display_name(trimmed);
    if login.is_empty() {
        return false;
    }
    let compact_alnum =
        |s: &str| -> String { s.chars().filter(char::is_ascii_alphanumeric).collect() };
    let login_compact = compact_alnum(&login);

    let (pa_name, pa_email) = split_author_name_and_email(pa);
    if let Some(name) = pa_name {
        let clean_name = name.strip_suffix(" via B4 Relay").unwrap_or(name);
        let norm_name = normalize_display_name(clean_name);
        if norm_name == login {
            return true;
        }
        if norm_name.split_whitespace().count() >= 2
            && login_compact.len() >= 3
            && compact_alnum(&norm_name) == login_compact
        {
            return true;
        }
    }

    for addr in [pa_email.as_str(), canon_pa] {
        let Some((local, domain)) = addr.split_once('@') else {
            continue;
        };
        if matches!(
            domain,
            "users.noreply.github.com" | "github.com" | "users.noreply.gitlab.com" | "gitlab.com"
        ) {
            let handle = local.rsplit('+').next().unwrap_or(local);
            if handle == login
                || (login_compact.len() >= 3 && compact_alnum(handle) == login_compact)
            {
                return true;
            }
        }
    }
    false
}

/// Checks whether a speaker is one of the series/patch authors: the same
/// address, the same `.mailmap` identity, or the same full display name.
///
/// Display names never grant identity elsewhere in this module because they
/// cannot be verified. Here a match only ever lowers trust (a `PatchAuthor`
/// cannot vouch for its own unverifiable claims), so it is the safe default
/// for an author replying from a second address.
pub fn is_speaker_patch_author(
    raw_speaker: &str,
    canonical_speaker: &str,
    patch_authors: &[String],
    mailmap: &Mailmap,
    is_known_canonical: &dyn Fn(&str) -> bool,
) -> bool {
    if canonical_speaker.is_empty() {
        return false;
    }
    let speaker_name = patch_author_name_key(raw_speaker);
    for pa in patch_authors {
        if pa.trim().is_empty() {
            continue;
        }
        if authors_match(raw_speaker, pa) {
            return true;
        }
        let canon_pa = canonicalize_speaker_identity(pa, mailmap, is_known_canonical);
        if !canon_pa.is_empty() && canon_pa == canonical_speaker {
            return true;
        }
        if speaker_name.is_some() && speaker_name == patch_author_name_key(pa) {
            return true;
        }
        if bare_forge_login_matches_patch_author(raw_speaker, pa, &canon_pa) {
            return true;
        }
    }
    false
}

/// Pre-canonicalized view of `MaintainersIndex` mapped through `.mailmap`.
///
/// Catch-all sections (THE REST) are kept as ordinary sections: their
/// addresses rank as subsystem maintainers only for files no specific section
/// covers (see `responsible_sections`), never for every file.
#[derive(Debug, Clone, Default)]
struct CanonicalMaintainersView {
    m_subsystems: HashMap<String, BTreeSet<String>>,
    r_subsystems: HashMap<String, BTreeSet<String>>,
}

impl CanonicalMaintainersView {
    fn build(index: &MaintainersIndex, mailmap: &Mailmap) -> Self {
        let mut view = Self::default();
        let canonicalize_entry = |raw_entry: &str| -> Option<String> {
            let addr = crate::maintainers::maintainer_address(raw_entry)?;
            let (opt_name, _) = split_author_name_and_email(raw_entry);
            Some(mailmap.canonicalize_email_with_name(opt_name, &addr))
        };
        for section in index.sections() {
            for raw in &section.m_maintainers {
                if let Some(canonical) = canonicalize_entry(raw) {
                    view.m_subsystems
                        .entry(canonical)
                        .or_default()
                        .insert(section.name.clone());
                }
            }
            for raw in &section.r_reviewers {
                if let Some(canonical) = canonicalize_entry(raw) {
                    view.r_subsystems
                        .entry(canonical)
                        .or_default()
                        .insert(section.name.clone());
                }
            }
        }
        view
    }

    fn knows(&self, canonical_email: &str) -> bool {
        self.m_subsystems.contains_key(canonical_email)
            || self.r_subsystems.contains_key(canonical_email)
    }

    /// `M:` entries of a touched section make a subsystem maintainer, `R:`
    /// entries a designated reviewer; entries elsewhere only count as
    /// maintainership of another subsystem.
    fn classify(
        &self,
        canonical_email: &str,
        touched_sections: &BTreeSet<String>,
    ) -> (MaintainerRole, Vec<String>) {
        let hits = |map: &HashMap<String, BTreeSet<String>>| -> Vec<String> {
            map.get(canonical_email)
                .map(|subs| subs.intersection(touched_sections).cloned().collect())
                .unwrap_or_default()
        };
        let m_hits = hits(&self.m_subsystems);
        if !m_hits.is_empty() {
            return (MaintainerRole::SubsystemMaintainer, m_hits);
        }
        let r_hits = hits(&self.r_subsystems);
        if !r_hits.is_empty() {
            return (MaintainerRole::DesignatedReviewer, r_hits);
        }
        if self.knows(canonical_email) {
            return (MaintainerRole::OtherSubsystemMaintainer, Vec::new());
        }
        (MaintainerRole::None, Vec::new())
    }
}

/// Pure deterministic tier computation from identity facts.
///
/// `M:` and `R:` entries of a touched section are both maintainers (`High`);
/// maintainers of other sections are `Moderate`; everyone else is `Low` and
/// must be proven right by code. Automated bots are `High` only when
/// `trusted_bot` (whitelisted), otherwise `Low`.
pub fn classify_contributor(
    maintainer_role: MaintainerRole,
    matched_subsystems: Vec<String>,
    speaker_role: SpeakerRole,
    canonical_email: String,
    trusted_bot: bool,
) -> ContributorAuthority {
    let tier = match speaker_role {
        SpeakerRole::AutomatedBot if trusted_bot => TrustTier::High,
        SpeakerRole::AutomatedBot => TrustTier::Low,
        SpeakerRole::Reviewer | SpeakerRole::PatchAuthor => match maintainer_role {
            MaintainerRole::SubsystemMaintainer | MaintainerRole::DesignatedReviewer => {
                TrustTier::High
            }
            MaintainerRole::OtherSubsystemMaintainer => TrustTier::Moderate,
            MaintainerRole::None => TrustTier::Low,
        },
    };
    let unverifiable_claim_trust =
        ContributorAuthority::effective_unverifiable_trust(tier, speaker_role, maintainer_role);
    ContributorAuthority {
        canonical_email,
        tier,
        speaker_role,
        maintainer_role,
        unverifiable_claim_trust,
        matched_subsystems,
    }
}

/// `MAINTAINERS` source for one review: a caller-supplied index (usually the
/// baseline worktree's file) or the process-wide one.
enum MaintainersSource<'a> {
    Borrowed(&'a MaintainersIndex),
    Shared(Arc<MaintainersIndex>),
    Missing,
}

impl MaintainersSource<'_> {
    fn index(&self) -> Option<&MaintainersIndex> {
        match self {
            MaintainersSource::Borrowed(idx) => Some(idx),
            MaintainersSource::Shared(idx) => Some(idx.as_ref()),
            MaintainersSource::Missing => None,
        }
    }
}

/// Authority inputs shared by every patch of one patchset review: the
/// baseline's `.mailmap`, the canonicalized `MAINTAINERS` view, the series
/// authors, and the project's trusted-bot whitelist. Built once per review;
/// per-patch evaluators then only add the touched sections.
pub struct PatchsetAuthority<'a> {
    mailmap: Arc<Mailmap>,
    maintainers: MaintainersSource<'a>,
    maintainers_view: Arc<CanonicalMaintainersView>,
    catch_all_sections: Arc<HashSet<String>>,
    patch_authors: Arc<Vec<String>>,
    trusted_bots: TrustedBots,
}

impl<'a> PatchsetAuthority<'a> {
    /// Loads `.mailmap` as recorded in `baseline_commit` (falling back to the
    /// repository's default branch when it is `None`) and
    /// `maintainers_override` (falling back to `get_global_maintainers()`).
    pub async fn load(
        repo_path: Option<&Path>,
        baseline_commit: Option<&str>,
        patch_authors: Vec<String>,
        maintainers_override: Option<&'a MaintainersIndex>,
        trusted_bots: TrustedBots,
    ) -> Self {
        let baseline = baseline_commit.map(str::trim).filter(|s| !s.is_empty());
        let mailmap = match (repo_path, baseline) {
            (Some(repo), Some(commit)) => Mailmap::from_commit(repo, commit).await,
            (Some(repo), None) => {
                let repo_owned = repo.to_path_buf();
                tokio::task::spawn_blocking(move || Mailmap::from_repo(&repo_owned))
                    .await
                    .unwrap_or_default()
            }
            (None, _) => Mailmap::empty(),
        };

        let maintainers = match maintainers_override {
            Some(idx) => MaintainersSource::Borrowed(idx),
            None => get_global_maintainers()
                .map_or(MaintainersSource::Missing, MaintainersSource::Shared),
        };
        let (maintainers_view, catch_all_sections) = match maintainers.index() {
            Some(idx) => (
                CanonicalMaintainersView::build(idx, &mailmap),
                idx.sections()
                    .iter()
                    .filter(|s| s.catch_all)
                    .map(|s| s.name.clone())
                    .collect(),
            ),
            None => (CanonicalMaintainersView::default(), HashSet::new()),
        };

        Self {
            mailmap: Arc::new(mailmap),
            maintainers,
            maintainers_view: Arc::new(maintainers_view),
            catch_all_sections: Arc::new(catch_all_sections),
            patch_authors: Arc::new(patch_authors),
            trusted_bots,
        }
    }

    /// Builds the evaluator for one patch touching `touched_files`.
    pub fn evaluator_for(&self, touched_files: &[String]) -> PatchAuthorityEvaluator {
        let touched_sections = self
            .maintainers
            .index()
            .map(|idx| responsible_sections(idx, &self.catch_all_sections, touched_files))
            .unwrap_or_default();
        PatchAuthorityEvaluator {
            mailmap: Arc::clone(&self.mailmap),
            maintainers_view: Arc::clone(&self.maintainers_view),
            touched_sections,
            patch_authors: Arc::clone(&self.patch_authors),
            trusted_bots: self.trusted_bots,
        }
    }
}

/// Sections responsible for `touched_files`. A catch-all section such as
/// THE REST is responsible exactly for the files no specific section covers.
/// Its `F:` globs only match top-level paths, so it is added explicitly for
/// any uncovered file, including nested ones; it is never added for a file a
/// specific section covers, or its maintainers would rank as subsystem
/// maintainers of every patch.
fn responsible_sections(
    index: &MaintainersIndex,
    catch_all_sections: &HashSet<String>,
    touched_files: &[String],
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for file in touched_files {
        let specific: Vec<String> = index
            .match_file(file)
            .into_iter()
            .filter(|name| !catch_all_sections.contains(name))
            .collect();
        if specific.is_empty() {
            out.extend(catch_all_sections.iter().cloned());
        } else {
            out.extend(specific);
        }
    }
    out
}

/// Evaluates contributor authority for a batch of speakers on one patch.
pub struct PatchAuthorityEvaluator {
    mailmap: Arc<Mailmap>,
    maintainers_view: Arc<CanonicalMaintainersView>,
    touched_sections: BTreeSet<String>,
    patch_authors: Arc<Vec<String>>,
    trusted_bots: TrustedBots,
}

impl PatchAuthorityEvaluator {
    /// Builds a one-off evaluator for a single patch. Reviews covering
    /// several patches should load one `PatchsetAuthority` and call
    /// `evaluator_for` per patch instead.
    pub async fn load(
        repo_path: Option<&Path>,
        touched_files: &[String],
        patch_authors: Vec<String>,
        maintainers_override: Option<&MaintainersIndex>,
        trusted_bots: TrustedBots,
    ) -> Self {
        PatchsetAuthority::load(
            repo_path,
            None,
            patch_authors,
            maintainers_override,
            trusted_bots,
        )
        .await
        .evaluator_for(touched_files)
    }

    /// Evaluates deterministic authority and role for a single speaker string
    /// (e.g., `"Jakub Kicinski <kuba@kernel.org>"` or a Forge login).
    pub fn evaluate(&self, raw_speaker: &str) -> ContributorAuthority {
        let is_known =
            |cand: &str| self.maintainers_view.knows(cand) || self.mailmap.knows_email(cand);
        let canonical = canonicalize_speaker_identity(raw_speaker, &self.mailmap, &is_known);
        let speaker_role = if is_automated_bot_identity(raw_speaker, &canonical)
            || self.trusted_bots.contains(&canonical)
        {
            SpeakerRole::AutomatedBot
        } else if is_speaker_patch_author(
            raw_speaker,
            &canonical,
            &self.patch_authors,
            &self.mailmap,
            &is_known,
        ) {
            SpeakerRole::PatchAuthor
        } else {
            SpeakerRole::Reviewer
        };

        let (maintainer_role, matched_subsystems) = self
            .maintainers_view
            .classify(&canonical, &self.touched_sections);
        let trusted_bot = self.trusted_bots.contains(&canonical);
        classify_contributor(
            maintainer_role,
            matched_subsystems,
            speaker_role,
            canonical,
            trusted_bot,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MAINTAINERS: &str = "\
Maintainers List
----------------

MEMORY MANAGEMENT - DAMON
M:\tSeongJae Park <sj@kernel.org>
R:\tDamon Reviewer <damon.rev@kernel.org>
L:\tdamon@lists.linux.dev
F:\tmm/damon/

BPF
M:\tAlexei Starovoitov <ast@kernel.org>
F:\tkernel/bpf/

THE REST
M:\tLinus Torvalds <torvalds@linux-foundation.org>
F:\t*
F:\t*/
";

    const TEST_MAILMAP: &str = "\
# Sample mailmap
SeongJae Park <sj@kernel.org> <sjpark@amazon.de>
Alexei Starovoitov <ast@kernel.org> <ast@fb.com>
Role Target <specific@kernel.org> Specific Name <shared-role@company.com>
";

    #[test]
    fn test_mailmap_b4_relay_and_display_name_spoofing_rejection() {
        let mailmap = Mailmap::from_reader(TEST_MAILMAP.as_bytes());
        let maint = MaintainersIndex::from_reader(TEST_MAINTAINERS.as_bytes()).unwrap();
        let view = CanonicalMaintainersView::build(&maint, &mailmap);
        let is_known = |e: &str| view.knows(e);

        // Corporate alias maps to canonical kernel.org identity.
        assert_eq!(
            canonicalize_speaker_identity("SeongJae Park <sjpark@amazon.de>", &mailmap, &is_known),
            "sj@kernel.org"
        );

        // Official b4 web-relay alias decodes and maps through .mailmap.
        assert_eq!(
            canonicalize_speaker_identity(
                "SeongJae Park via B4 Relay <devnull+sjpark.amazon.de@kernel.org>",
                &mailmap,
                &is_known
            ),
            "sj@kernel.org"
        );

        // Non-official devnull+ domain is NOT decoded as a trusted b4 relay.
        assert_eq!(
            canonicalize_speaker_identity(
                "Spoofer <devnull+sj.kernel.org@evil.example.com>",
                &mailmap,
                &is_known
            ),
            "devnull+sj.kernel.org@evil.example.com"
        );

        // Display-name spoofing: "Linus Torvalds <random@gmail.com>" resolves strictly to random@gmail.com.
        assert_eq!(
            canonicalize_speaker_identity("Linus Torvalds <random@gmail.com>", &mailmap, &is_known),
            "random@gmail.com"
        );

        // Name-qualified mailmap entry only applies when Commit Name also matches.
        assert_eq!(
            canonicalize_speaker_identity(
                "Specific Name <shared-role@company.com>",
                &mailmap,
                &is_known
            ),
            "specific@kernel.org"
        );
        assert_eq!(
            canonicalize_speaker_identity(
                "Someone Else <shared-role@company.com>",
                &mailmap,
                &is_known
            ),
            "shared-role@company.com"
        );
    }

    const LINUX_BOTS: TrustedBots = TrustedBots(&["lkp@intel.com", "*@syzkaller.appspotmail.com"]);

    #[tokio::test]
    async fn test_patch_authority_evaluator_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        std::fs::write(repo.join(".mailmap"), TEST_MAILMAP).unwrap();

        let maint = MaintainersIndex::from_reader(TEST_MAINTAINERS.as_bytes()).unwrap();
        let eval_damon = PatchAuthorityEvaluator::load(
            Some(repo),
            &["mm/damon/core.c".to_string()],
            vec!["Patch Submitter <submitter@example.org>".to_string()],
            Some(&maint),
            LINUX_BOTS,
        )
        .await;

        // 1. Subsystem M: maintainer via a .mailmap alias -> High.
        let sj = eval_damon.evaluate("SeongJae Park <sjpark@amazon.de>");
        assert_eq!(sj.canonical_email, "sj@kernel.org");
        assert_eq!(sj.maintainer_role, MaintainerRole::SubsystemMaintainer);
        assert_eq!(sj.tier, TrustTier::High);
        assert_eq!(sj.unverifiable_claim_trust, TrustTier::High);

        // 2. R: reviewer of the touched files is a maintainer too -> High.
        let rev = eval_damon.evaluate("Damon Reviewer <damon.rev@kernel.org>");
        assert_eq!(rev.maintainer_role, MaintainerRole::DesignatedReviewer);
        assert_eq!(rev.tier, TrustTier::High);
        assert_eq!(rev.unverifiable_claim_trust, TrustTier::High);

        // 3. Catch-all maintainer (THE REST) is not a maintainer of files a
        // specific section covers.
        let torvalds = eval_damon.evaluate("Linus Torvalds <torvalds@linux-foundation.org>");
        assert_eq!(
            torvalds.maintainer_role,
            MaintainerRole::OtherSubsystemMaintainer
        );
        assert_eq!(torvalds.tier, TrustTier::Moderate);

        // 4. Spoofed display name -> Low.
        let spoofer = eval_damon.evaluate("Linus Torvalds <impostor@gmail.com>");
        assert_eq!(spoofer.canonical_email, "impostor@gmail.com");
        assert_eq!(spoofer.maintainer_role, MaintainerRole::None);
        assert_eq!(spoofer.tier, TrustTier::Low);

        // 5. Maintainer of an unrelated subsystem (BPF) -> Moderate.
        let ast = eval_damon.evaluate("Alexei Starovoitov <ast@fb.com>");
        assert_eq!(ast.canonical_email, "ast@kernel.org");
        assert_eq!(
            ast.maintainer_role,
            MaintainerRole::OtherSubsystemMaintainer
        );
        assert_eq!(ast.tier, TrustTier::Moderate);

        // 6. Patch author (via b4 relay) -> PatchAuthor, Low claim trust.
        let author = eval_damon
            .evaluate("Patch Submitter via B4 Relay <devnull+submitter.example.org@kernel.org>");
        assert_eq!(author.speaker_role, SpeakerRole::PatchAuthor);
        assert_eq!(author.unverifiable_claim_trust, TrustTier::Low);

        // 7. Whitelisted bots -> High; other bots -> Low.
        let lkp = eval_damon.evaluate("kernel test robot <lkp@intel.com>");
        assert_eq!(lkp.speaker_role, SpeakerRole::AutomatedBot);
        assert_eq!(lkp.tier, TrustTier::High);
        assert_eq!(lkp.unverifiable_claim_trust, TrustTier::High);
        let syz = eval_damon.evaluate("syzbot <syzbot+0123abcd@syzkaller.appspotmail.com>");
        assert_eq!(syz.speaker_role, SpeakerRole::AutomatedBot);
        assert_eq!(syz.tier, TrustTier::High);
        let pw = eval_damon.evaluate("patchwork-bot+netdevbpf@kernel.org");
        assert_eq!(pw.speaker_role, SpeakerRole::AutomatedBot);
        assert_eq!(pw.tier, TrustTier::Low);
        assert_eq!(pw.unverifiable_claim_trust, TrustTier::Low);

        // 8. Unknown reviewer -> Low, regardless of how active they are.
        let reg = eval_damon.evaluate("Regular Dev <reg@example.com>");
        assert_eq!(reg.speaker_role, SpeakerRole::Reviewer);
        assert_eq!(reg.maintainer_role, MaintainerRole::None);
        assert_eq!(reg.tier, TrustTier::Low);

        // 9. A file only THE REST covers makes its M: entry the maintainer.
        let eval_root = PatchAuthorityEvaluator::load(
            Some(repo),
            &["Makefile".to_string()],
            Vec::new(),
            Some(&maint),
            LINUX_BOTS,
        )
        .await;
        let torvalds_root = eval_root.evaluate("Linus Torvalds <torvalds@linux-foundation.org>");
        assert_eq!(
            torvalds_root.maintainer_role,
            MaintainerRole::SubsystemMaintainer
        );
        assert_eq!(
            torvalds_root.matched_subsystems,
            vec!["THE REST".to_string()]
        );
        assert_eq!(torvalds_root.tier, TrustTier::High);

        // 10. Without a whitelist even lkp stays Low.
        let eval_no_bots = PatchAuthorityEvaluator::load(
            Some(repo),
            &["mm/damon/core.c".to_string()],
            Vec::new(),
            Some(&maint),
            TrustedBots::default(),
        )
        .await;
        let lkp = eval_no_bots.evaluate("kernel test robot <lkp@intel.com>");
        assert_eq!(lkp.speaker_role, SpeakerRole::AutomatedBot);
        assert_eq!(lkp.tier, TrustTier::Low);
    }

    #[test]
    fn test_catch_all_reviewer_is_not_a_maintainer_of_specific_files() {
        let content = "\
Maintainers List
----------------

THE REST
M:\tLinus Torvalds <torvalds@linux-foundation.org>
R:\tCatch All Reviewer <rest.rev@example.com>
F:\t*
F:\t*/

MEMORY MANAGEMENT - DAMON
M:\tSeongJae Park <sj@kernel.org>
F:\tmm/damon/
";
        let maint = MaintainersIndex::from_reader(content.as_bytes()).unwrap();
        let mailmap = Mailmap::empty();
        let view = CanonicalMaintainersView::build(&maint, &mailmap);
        let catch_all: HashSet<String> = maint
            .sections()
            .iter()
            .filter(|s| s.catch_all)
            .map(|s| s.name.clone())
            .collect();
        let touched = responsible_sections(&maint, &catch_all, &["mm/damon/core.c".to_string()]);
        assert_eq!(
            touched.iter().cloned().collect::<Vec<_>>(),
            vec!["MEMORY MANAGEMENT - DAMON".to_string()]
        );
        let (role, _) = view.classify("rest.rev@example.com", &touched);
        assert_eq!(role, MaintainerRole::OtherSubsystemMaintainer);
        let (role, _) = view.classify("torvalds@linux-foundation.org", &touched);
        assert_eq!(role, MaintainerRole::OtherSubsystemMaintainer);

        let root = responsible_sections(&maint, &catch_all, &["Makefile".to_string()]);
        let (role, _) = view.classify("rest.rev@example.com", &root);
        assert_eq!(role, MaintainerRole::DesignatedReviewer);

        // A nested file no specific section covers belongs to THE REST too,
        // even though its F: globs only match top-level paths.
        let nested = responsible_sections(&maint, &catch_all, &["init/main.c".to_string()]);
        assert_eq!(
            nested.iter().cloned().collect::<Vec<_>>(),
            vec!["THE REST".to_string()]
        );
        let (role, _) = view.classify("torvalds@linux-foundation.org", &nested);
        assert_eq!(role, MaintainerRole::SubsystemMaintainer);
    }

    #[test]
    fn test_tier_comes_only_from_identity() {
        let classify = |role, speaker, bot| {
            classify_contributor(role, Vec::new(), speaker, "x@example.com".to_string(), bot)
        };
        use MaintainerRole as M;
        use SpeakerRole as S;
        assert_eq!(
            classify(M::SubsystemMaintainer, S::Reviewer, false).tier,
            TrustTier::High
        );
        assert_eq!(
            classify(M::DesignatedReviewer, S::Reviewer, false).tier,
            TrustTier::High
        );
        assert_eq!(
            classify(M::OtherSubsystemMaintainer, S::Reviewer, false).tier,
            TrustTier::Moderate
        );
        assert_eq!(classify(M::None, S::Reviewer, false).tier, TrustTier::Low);
        assert_eq!(
            classify(M::None, S::AutomatedBot, true).tier,
            TrustTier::High
        );
        assert_eq!(
            classify(M::None, S::AutomatedBot, false).tier,
            TrustTier::Low
        );

        // Patch authors vouch for their own unverifiable claims only as
        // maintainers of the touched files.
        let pa = classify(M::OtherSubsystemMaintainer, S::PatchAuthor, false);
        assert_eq!(pa.tier, TrustTier::Moderate);
        assert_eq!(pa.unverifiable_claim_trust, TrustTier::Low);
        let pa = classify(M::DesignatedReviewer, S::PatchAuthor, false);
        assert_eq!(pa.unverifiable_claim_trust, TrustTier::High);
    }

    #[test]
    fn test_trusted_bots_match_exact_address_or_whole_domain() {
        let bots = TrustedBots(&["lkp@intel.com", "*@syzkaller.appspotmail.com"]);
        assert!(bots.contains("lkp@intel.com"));
        assert!(bots.contains("LKP@Intel.com"));
        assert!(bots.contains("syzbot+abc@syzkaller.appspotmail.com"));
        assert!(!bots.contains("oliver.sang@intel.com"));
        assert!(!bots.contains("x@evil-syzkaller.appspotmail.com"));
        assert!(!bots.contains("x@sub.syzkaller.appspotmail.com"));
        assert!(!bots.contains("@syzkaller.appspotmail.com"));
        assert!(!bots.contains(""));
    }

    #[test]
    fn test_forge_association_grants_high_trust() {
        let auth = classify_contributor(
            MaintainerRole::None,
            Vec::new(),
            SpeakerRole::Reviewer,
            "octocat".to_string(),
            false,
        )
        .with_forge_association(Some("MEMBER"));
        assert_eq!(auth.tier, TrustTier::High);
        assert_eq!(auth.maintainer_role, MaintainerRole::SubsystemMaintainer);
        assert_eq!(auth.unverifiable_claim_trust, TrustTier::High);
    }

    #[test]
    fn test_patch_author_replying_from_second_address_stays_patch_author() {
        let mailmap = Mailmap::from_reader(TEST_MAILMAP.as_bytes());
        let authors = vec!["Jane Q Developer <jane@corp.example.com>".to_string()];
        let is_known = |_: &str| false;
        let speaker = "Jane Q Developer <jane.personal@gmail.com>";
        let canonical = canonicalize_speaker_identity(speaker, &mailmap, &is_known);
        assert!(is_speaker_patch_author(
            speaker, &canonical, &authors, &mailmap, &is_known
        ));

        // A .mailmap alias of the author also counts.
        let authors = vec!["SeongJae Park <sj@kernel.org>".to_string()];
        let speaker = "SJ <sjpark@amazon.de>";
        let canonical = canonicalize_speaker_identity(speaker, &mailmap, &is_known);
        assert!(is_speaker_patch_author(
            speaker, &canonical, &authors, &mailmap, &is_known
        ));

        // A single shared first name is not enough when both sides carry emails.
        let authors = vec!["Jane <jane@corp.example.com>".to_string()];
        let speaker = "Jane <someone.else@example.org>";
        let canonical = canonicalize_speaker_identity(speaker, &mailmap, &is_known);
        assert!(!is_speaker_patch_author(
            speaker, &canonical, &authors, &mailmap, &is_known
        ));

        // Bare Forge logins match the commit author by display name or
        // GitHub/GitLab noreply handle.
        let authors = vec![
            "Contributor <c@example.com>".to_string(),
            "Jane Doe <jane@example.com>".to_string(),
            "Some Author <12345+octocat@users.noreply.github.com>".to_string(),
        ];
        for login in ["Contributor", "jane-doe", "octocat"] {
            let canonical = canonicalize_speaker_identity(login, &mailmap, &is_known);
            assert!(
                is_speaker_patch_author(login, &canonical, &authors, &mailmap, &is_known),
                "expected bare Forge login {login} to match patch authors"
            );
        }
        let reviewer_login = "repo-maintainer";
        let canonical = canonicalize_speaker_identity(reviewer_login, &mailmap, &is_known);
        assert!(!is_speaker_patch_author(
            reviewer_login,
            &canonical,
            &authors,
            &mailmap,
            &is_known
        ));
    }

    #[test]
    fn test_sashiko_and_automated_bot_identities() {
        for raw in [
            "sashiko-bot <sashiko-bot@kernel.org>",
            "netdev-bot+sashiko@kernel.org",
            "sashiko-bot--- via Linux-pm <linux-pm@lists.linux.dev>",
            "sashiko-bot[bot]",
        ] {
            assert!(is_sashiko_author(raw), "expected Sashiko identity: {raw}");
            let (_, email) = split_author_name_and_email(raw);
            let email_arg = if email.is_empty() { raw } else { &email };
            assert!(
                is_automated_bot_identity(raw, email_arg),
                "expected automated bot identity: {raw}"
            );
        }

        for (raw, email) in [
            ("bot+bpf-ci@kernel.org", "bot+bpf-ci@kernel.org"),
            ("netdev-bot+sinfo@kernel.org", "netdev-bot+sinfo@kernel.org"),
            ("pr-tracker-bot@kernel.org", "pr-tracker-bot@kernel.org"),
            (
                "\"pr-tracker-bot--- via Linux-f2fs-devel\" <linux-f2fs-devel@lists.sourceforge.net>",
                "linux-f2fs-devel@lists.sourceforge.net",
            ),
            ("bluez.test.bot@gmail.com", "bluez.test.bot@gmail.com"),
            (
                "tip-bot2 for Foo <tip-bot2@linutronix.de>",
                "tip-bot2@linutronix.de",
            ),
            (
                "Patchwork <patchwork@emeril.freedesktop.org>",
                "patchwork@emeril.freedesktop.org",
            ),
            (
                "MPTCP CI <wpasupplicant.patchew@gmail.com>",
                "wpasupplicant.patchew@gmail.com",
            ),
        ] {
            assert!(
                !is_sashiko_identity(raw, email),
                "non-Sashiko bot must not match is_sashiko_identity: {raw}"
            );
            assert!(
                is_automated_bot_identity(raw, email),
                "expected automated bot identity: {raw}"
            );
        }
    }

    #[test]
    fn test_canonical_maintainers_view_respects_name_qualified_mailmap() {
        let mailmap = Mailmap::from_reader(
            b"Alice Canonical <alice@kernel.org> Alice Maintainer <shared@corp.example.com>\n"
                .as_slice(),
        );
        let maintainers_raw = "\
Maintainers List
----------------

FOO SUBSYSTEM
M:\tAlice Maintainer <shared@corp.example.com>
F:\tdrivers/foo/
";
        let index = MaintainersIndex::from_reader(maintainers_raw.as_bytes()).unwrap();
        let view = CanonicalMaintainersView::build(&index, &mailmap);
        let is_known = |e: &str| view.knows(e);
        let canonical = canonicalize_speaker_identity(
            "Alice Canonical <alice@kernel.org>",
            &mailmap,
            &is_known,
        );
        let touched =
            responsible_sections(&index, &HashSet::new(), &["drivers/foo/bar.c".to_string()]);
        let (role, subsys) = view.classify(&canonical, &touched);
        assert_eq!(role, MaintainerRole::SubsystemMaintainer);
        assert_eq!(subsys, vec!["FOO SUBSYSTEM".to_string()]);
    }
}
