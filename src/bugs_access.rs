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

//! Subsystem bug and embargo access grants (`projects/<project>/bugs_access.toml`).
//!
//! Extends or overrides `MAINTAINERS` strictly for Sashiko access control
//! (bugs, embargoed review results, review transcripts, and sign-in
//! eligibility) without changing mail routing, contributor authority, or the
//! subsystem names displayed on bugs and patchsets. See
//! `designs/DESIGN_BUGS_ACCESS.md`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use tracing::warn;

use crate::maintainers::maintainer_address;
use crate::project::ProjectId;

/// Reserved prefix for synthetic authorization section keys produced by
/// `bugs_access.toml` overrides and custom rules.
pub const RESERVED_SECTION_PREFIX: &str = "bugs_access::";

/// Legacy Linux grant file path retained as a fallback for existing setups.
const LEGACY_LINUX_UPSTREAM_MAINTAINERS_PATH: &str = "projects/linux/upstream_maintainers.toml";

/// Access level conferred by a `bugs_access.toml` grant.
///
/// Ordered from least to most authority so overlapping grants compose via
/// `Ord::max`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum AccessLevel {
    /// Read-only: may sign in, list and read bugs in the section (including
    /// reproducers), and view embargoed patchset reviews and transcripts.
    #[serde(rename = "read")]
    Read,
    /// Read-write: full maintainer authority (`Read` plus commenting, closing,
    /// dismissing, assigning, deduplicating, security re-evaluation, and early
    /// embargo release).
    #[default]
    #[serde(rename = "read_write", alias = "read-write")]
    ReadWrite,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BugsAccessFile {
    #[serde(default)]
    grant: Vec<RawAccessGrant>,
}

/// One `[[grant]]` entry in `bugs_access.toml` before resolution against
/// [`crate::maintainers::MaintainersIndex`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAccessGrant {
    /// Existing `MAINTAINERS` section title or custom section title.
    /// Optional when `paths` and at least one of `read` / `read_write` are
    /// non-empty.
    #[serde(default)]
    pub section: Option<String>,
    /// `MAINTAINERS` `F:`-style file patterns to include.
    #[serde(default)]
    pub paths: Vec<String>,
    /// `MAINTAINERS` `X:`-style file patterns to exclude.
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    /// Discard inherited `MAINTAINERS` `F:`/`X:`/`N:` patterns for `section`.
    #[serde(default)]
    pub replace_paths: bool,
    /// Bare addresses (`user@domain`) or `Name <user@domain>` entries granted
    /// read-only access.
    #[serde(default)]
    pub read: Vec<String>,
    /// Bare addresses (`user@domain`) or `Name <user@domain>` entries granted
    /// full maintainer read-write access.
    #[serde(default, alias = "read-write")]
    pub read_write: Vec<String>,
    /// Addresses removed from `section`'s access map before `read` and
    /// `read_write` apply.
    #[serde(default)]
    pub exclude_emails: Vec<String>,
    /// Discard inherited `MAINTAINERS` `M:`/`R:` addresses for `section`.
    #[serde(default)]
    pub replace_emails: bool,
    /// Access level granted to `section`'s existing maintainers on a path-only
    /// grant (defaults to `read_write`).
    #[serde(default)]
    pub access: Option<AccessLevel>,
}

impl RawAccessGrant {
    /// Whether this entry names any `read` or `read_write` addresses.
    pub fn has_emails(&self) -> bool {
        !self.read.is_empty() || !self.read_write.is_empty()
    }
}

/// Normalizes a section title by trimming, collapsing whitespace, and
/// lowercasing so comparisons ignore incidental formatting.
pub fn normalize_section_title(title: &str) -> String {
    let mut normalized = String::with_capacity(title.len());
    for word in title.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.extend(word.chars().flat_map(char::to_lowercase));
    }
    normalized
}

/// Computes a deterministic authorization key for an anonymous path+email
/// grant from its canonicalized `paths` and `exclude_paths`.
///
/// Keying only on path patterns keeps stored database rows stable when `read`
/// or `read_write` lists are edited.
pub fn anonymous_rule_key(prefix_tag: &str, paths: &[String], exclude_paths: &[String]) -> String {
    let mut inc: Vec<&str> = paths.iter().map(|p| p.trim()).collect();
    inc.sort_unstable();
    inc.dedup();
    let mut exc: Vec<&str> = exclude_paths.iter().map(|p| p.trim()).collect();
    exc.sort_unstable();
    exc.dedup();

    let mut hasher = Sha256::new();
    hasher.update(prefix_tag.as_bytes());
    hasher.update(b"\n");
    for p in inc {
        hasher.update(b"F:");
        hasher.update(p.as_bytes());
        hasher.update(b"\n");
    }
    for x in exc {
        hasher.update(b"X:");
        hasher.update(x.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    format!("{RESERVED_SECTION_PREFIX}{prefix_tag}:{hex}")
}

/// Parses `bugs_access.toml` text.
pub fn parse(text: &str) -> Result<Vec<RawAccessGrant>> {
    let file: BugsAccessFile = toml::from_str(text).context("invalid bugs_access configuration")?;
    Ok(file.grant)
}

/// Loads `bugs_access.toml` from `path`. A missing file means no grants.
pub fn load(path: &Path) -> Result<Vec<RawAccessGrant>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("in {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Loads `bugs_access.toml` for `project`, falling back to legacy
/// `projects/linux/upstream_maintainers.toml` with a deprecation warning when
/// `projects/linux/bugs_access.toml` is absent.
pub fn load_for_project(project: ProjectId) -> Result<(&'static str, Vec<RawAccessGrant>)> {
    let primary = project.bugs_access_path();
    let primary_path = Path::new(primary);
    match std::fs::read_to_string(primary_path) {
        Ok(text) => {
            let grants = parse(&text).with_context(|| format!("in {primary}"))?;
            Ok((primary, grants))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if project == ProjectId::Linux {
                let legacy = Path::new(LEGACY_LINUX_UPSTREAM_MAINTAINERS_PATH);
                if legacy.exists() {
                    warn!(
                        "Loading deprecated {LEGACY_LINUX_UPSTREAM_MAINTAINERS_PATH}; \
                         rename it to {primary}"
                    );
                    let grants = load(legacy)?;
                    return Ok((LEGACY_LINUX_UPSTREAM_MAINTAINERS_PATH, grants));
                }
            }
            Ok((primary, Vec::new()))
        }
        Err(e) => Err(e).with_context(|| format!("cannot read {primary}")),
    }
}

/// Validates a single `F:`/`X:` pattern.
///
/// Rejects empty patterns, control characters, and patterns made solely of
/// wildcards and slashes (`*`, `*/`, `**`, `*/*`) that would claim the entire
/// repository tree.
pub fn validate_pattern(raw: &str) -> Result<(), String> {
    if raw.chars().any(char::is_control) {
        return Err(format!("path {raw:?} contains control characters"));
    }
    let trimmed = raw.trim().trim_start_matches("./");
    if trimmed.is_empty() {
        return Err("it has an empty path".to_string());
    }
    if trimmed.chars().all(|c| matches!(c, '*' | '?' | '/')) {
        return Err(format!("path {raw:?} would cover the whole tree"));
    }
    Ok(())
}

/// Validates a non-empty list of path patterns.
pub fn validate_paths(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("it lists no paths".to_string());
    }
    validate_pattern_list(paths)
}

fn validate_pattern_list(paths: &[String]) -> Result<(), String> {
    for path in paths {
        validate_pattern(path)?;
    }
    Ok(())
}

fn validate_email_list(field: &str, entries: &[String]) -> Result<(), String> {
    for entry in entries {
        if maintainer_address(entry).is_none() {
            return Err(format!("invalid email entry {entry:?} in `{field}`"));
        }
    }
    Ok(())
}

/// Validates a `[[grant]]` entry prior to resolving against `MAINTAINERS`.
pub fn validate_grant(grant: &RawAccessGrant) -> Result<(), String> {
    validate_pattern_list(&grant.paths)?;
    validate_pattern_list(&grant.exclude_paths)?;
    validate_email_list("read", &grant.read)?;
    validate_email_list("read_write", &grant.read_write)?;
    validate_email_list("exclude_emails", &grant.exclude_emails)?;

    if grant.access.is_some() {
        if grant.has_emails() || grant.replace_emails || !grant.exclude_emails.is_empty() {
            return Err(
                "`access` applies only to path-only grants on an existing section; \
                 use `read` and `read_write` directly when listing emails"
                    .to_string(),
            );
        }
        if grant.paths.is_empty() {
            return Err("`access` requires non-empty `paths`".to_string());
        }
    }

    match grant.section.as_deref() {
        Some(raw) => validate_named_grant(raw, grant),
        None => validate_anonymous_grant(grant),
    }
}

fn validate_named_grant(raw_section: &str, grant: &RawAccessGrant) -> Result<(), String> {
    if raw_section.chars().any(char::is_control) {
        return Err("section title contains control characters".to_string());
    }
    let norm = normalize_section_title(raw_section);
    if norm.is_empty() {
        return Err("section title is empty".to_string());
    }
    if norm.starts_with(RESERVED_SECTION_PREFIX) {
        return Err(format!(
            "section title {raw_section:?} uses reserved prefix {RESERVED_SECTION_PREFIX:?}"
        ));
    }
    if grant.replace_paths && grant.paths.is_empty() {
        return Err("`replace_paths = true` requires non-empty `paths` \
             (use `replace_emails = true` with empty `read` and `read_write` to strip access)"
            .to_string());
    }
    let has_any_change = !grant.paths.is_empty()
        || !grant.exclude_paths.is_empty()
        || grant.replace_paths
        || grant.has_emails()
        || !grant.exclude_emails.is_empty()
        || grant.replace_emails;
    if !has_any_change {
        return Err("grant specifies no paths, emails, or replacements".to_string());
    }
    Ok(())
}

fn validate_anonymous_grant(grant: &RawAccessGrant) -> Result<(), String> {
    if grant.paths.is_empty() || !grant.has_emails() {
        return Err(
            "a grant without `section` must specify non-empty `paths` and at least one address in `read` or `read_write`"
                .to_string(),
        );
    }
    if grant.replace_paths
        || grant.replace_emails
        || !grant.exclude_emails.is_empty()
        || grant.access.is_some()
    {
        return Err(
            "`replace_paths`, `replace_emails`, `exclude_emails`, and `access` require `section`"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_all_grant_shapes_and_reject_unknown_fields() {
        let parsed = parse(
            r#"
[[grant]]
section = "SOC"
paths = ["drivers/soc/", "arch/*/boot/dts/"]

[[grant]]
section = "BTRFS FILE SYSTEM"
read = ["dev1@example.org", "Dev Two <dev2@example.org>"]
read_write = ["lead@example.org"]

[[grant]]
paths = ["fs/btrfs/zoned.*"]
exclude_paths = ["fs/btrfs/tests/"]
"read-write" = ["zoned-lead@example.org"]
read = ["zoned@example.org"]

[[grant]]
section = "FILESYSTEMS"
paths = ["fs/"]
access = "read"
"#,
        )
        .unwrap();
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].access, None);
        assert_eq!(parsed[1].read.len(), 2);
        assert_eq!(parsed[1].read_write, vec!["lead@example.org"]);
        assert_eq!(parsed[2].section, None);
        assert_eq!(parsed[2].read_write, vec!["zoned-lead@example.org"]);
        assert_eq!(parsed[3].access, Some(AccessLevel::Read));

        assert!(parse("").unwrap().is_empty());
        assert!(parse("[[grant]]\nsection = \"SOC\"\nunknown_key = true\n").is_err());
        assert!(
            parse("[[grant]]\nsection = \"SOC\"\npaths = [\"fs/\"]\naccess = \"none\"\n").is_err()
        );
    }

    #[test]
    fn test_validate_grant_enforces_invariants() {
        // Valid path extension (#787).
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("SOC".into()),
                paths: vec!["drivers/soc/".into()],
                ..Default::default()
            })
            .is_ok()
        );
        // Valid read + read_write email grant (#867).
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("BTRFS FILE SYSTEM".into()),
                read: vec!["Developer <dev@example.org>".into()],
                read_write: vec!["Maint <maint@example.org>".into()],
                ..Default::default()
            })
            .is_ok()
        );
        // Valid replace_emails = true with empty lists (strips access).
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("BTRFS FILE SYSTEM".into()),
                replace_emails: true,
                ..Default::default()
            })
            .is_ok()
        );

        // Invalid catch-all or empty patterns.
        for bad in ["", " ", "*", "*/", "**", "./*", "*/*"] {
            assert!(validate_paths(&[bad.into()]).is_err(), "{bad:?}");
        }
        // Invalid emails in read or read_write.
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("SOC".into()),
                read: vec!["not-an-email".into()],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("SOC".into()),
                read_write: vec!["not-an-email".into()],
                ..Default::default()
            })
            .is_err()
        );
        // Mixing `access` with `read` / `read_write` or without `paths` is rejected.
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("SOC".into()),
                paths: vec!["drivers/soc/".into()],
                read: vec!["dev@example.org".into()],
                access: Some(AccessLevel::Read),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("SOC".into()),
                exclude_paths: vec!["drivers/soc/legacy/".into()],
                access: Some(AccessLevel::Read),
                ..Default::default()
            })
            .is_err()
        );
        // Reserved section prefix rejected.
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("bugs_access::override:SOC".into()),
                paths: vec!["drivers/soc/".into()],
                ..Default::default()
            })
            .is_err()
        );
        // Empty named grant rejected.
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("SOC".into()),
                ..Default::default()
            })
            .is_err()
        );
        // replace_paths requires non-empty paths.
        assert!(
            validate_grant(&RawAccessGrant {
                section: Some("SOC".into()),
                replace_paths: true,
                ..Default::default()
            })
            .is_err()
        );
        // Anonymous grant requires both paths and read/read_write.
        assert!(
            validate_grant(&RawAccessGrant {
                paths: vec!["fs/btrfs/".into()],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate_grant(&RawAccessGrant {
                paths: vec!["fs/btrfs/".into()],
                read: vec!["dev@example.org".into()],
                replace_emails: true,
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn test_anonymous_rule_key_is_order_independent_and_path_scoped() {
        let k1 = anonymous_rule_key(
            "rule",
            &["fs/btrfs/zoned.c".into(), "fs/btrfs/zoned.h".into()],
            &["fs/btrfs/tests/".into()],
        );
        let k2 = anonymous_rule_key(
            "rule",
            &[" fs/btrfs/zoned.h ".into(), "fs/btrfs/zoned.c".into()],
            &["fs/btrfs/tests/".into()],
        );
        let k3 = anonymous_rule_key("rule", &["fs/btrfs/zoned.c".into()], &[]);
        assert!(k1.starts_with("bugs_access::rule:"));
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
    }

    #[test]
    fn test_load_missing_file_means_no_grants() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(load(&dir.path().join("absent.toml")).unwrap().is_empty());
    }

    #[test]
    fn test_shipped_bugs_access_files_parse_and_validate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for project in ProjectId::ALL {
            let path = root.join(project.bugs_access_path());
            for grant in load(&path).unwrap_or_else(|e| panic!("{e:#}")) {
                validate_grant(&grant)
                    .unwrap_or_else(|e| panic!("{}: {:?}: {e}", path.display(), grant.section));
            }
        }
    }
}
