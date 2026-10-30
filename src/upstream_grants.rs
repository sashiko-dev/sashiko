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

//! Upstream maintainer grants (`projects/<project>/upstream_maintainers.toml`).
//!
//! MAINTAINERS records who owns a file, not who receives pull requests for
//! it. A grant gives the maintainers of an existing section the same access
//! to bugs and embargoed reviews in extra paths as if the section listed
//! them, without changing what MAINTAINERS says about ownership. See
//! `designs/DESIGN_UPSTREAM_MAINTAINER_BUG_ACCESS.md`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::maintainers::CompiledPattern;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantsFile {
    #[serde(default)]
    grant: Vec<RawUpstreamGrant>,
}

/// A grant as written in the file, before it is checked against MAINTAINERS.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawUpstreamGrant {
    /// Title of the existing MAINTAINERS section whose maintainers receive
    /// the grant.
    pub section: String,
    /// `F:`-style patterns the section's maintainers receive pull requests
    /// for.
    pub paths: Vec<String>,
}

/// A grant whose section exists and whose paths passed validation.
#[derive(Debug, Clone)]
pub struct UpstreamGrant {
    /// The section title exactly as MAINTAINERS spells it, so that it is the
    /// same string the access checks compare against.
    pub section: String,
    raw_paths: Vec<String>,
    paths: Vec<CompiledPattern>,
}

impl UpstreamGrant {
    /// Builds a grant for a section title already resolved against the index.
    /// Callers validate the paths first with [`validate_paths`].
    pub(crate) fn new(section: String, raw_paths: Vec<String>) -> Self {
        let paths = raw_paths
            .iter()
            .map(|p| CompiledPattern::compile(p))
            .collect();
        Self {
            section,
            raw_paths,
            paths,
        }
    }

    /// Whether this grant covers `file_path`.
    pub fn covers(&self, file_path: &str) -> bool {
        self.paths.iter().any(|p| p.is_match(file_path))
    }

    /// The paths as written, for fingerprinting and logs.
    pub fn raw_paths(&self) -> &[String] {
        &self.raw_paths
    }
}

/// Parses the grant file contents.
pub fn parse(text: &str) -> Result<Vec<RawUpstreamGrant>> {
    let file: GrantsFile = toml::from_str(text).context("invalid upstream maintainer grants")?;
    Ok(file.grant)
}

/// Loads the grant file. A missing file means no grants.
pub fn load(path: &Path) -> Result<Vec<RawUpstreamGrant>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("in {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Checks a grant's paths. Returns why the grant must be dropped, if it must.
///
/// A path made only of wildcards and slashes (`*`, `*/`, `**`) would grant
/// the whole tree, which is what THE REST is for, not an upstream grant.
pub fn validate_paths(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("it lists no paths".to_string());
    }
    for path in paths {
        let trimmed = path.trim().trim_start_matches("./");
        if trimmed.is_empty() {
            return Err("it has an empty path".to_string());
        }
        if trimmed.chars().all(|c| matches!(c, '*' | '?' | '/')) {
            return Err(format!("path {path:?} would cover the whole tree"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_and_reject_unknown_fields() {
        let grants = parse(
            "[[grant]]\nsection = \"SOC\"\npaths = [\"drivers/soc/\", \"arch/*/boot/dts/\"]\n",
        )
        .unwrap();
        assert_eq!(
            grants,
            vec![RawUpstreamGrant {
                section: "SOC".into(),
                paths: vec!["drivers/soc/".into(), "arch/*/boot/dts/".into()],
            }]
        );
        assert!(parse("").unwrap().is_empty());
        assert!(parse("[[grant]]\nsection = \"SOC\"\npaths = []\naccess = \"read\"\n").is_err());
    }

    #[test]
    fn test_validate_paths() {
        assert!(validate_paths(&["drivers/soc/".into()]).is_ok());
        assert!(validate_paths(&[]).is_err());
        for bad in ["", " ", "*", "*/", "**", "./*", "*/*"] {
            assert!(validate_paths(&[bad.into()]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_covers_uses_maintainers_pattern_syntax() {
        let grant = UpstreamGrant::new(
            "SOC".into(),
            vec!["arch/*/boot/dts/".into(), "drivers/soc/".into()],
        );
        assert!(grant.covers("arch/arm64/boot/dts/qcom/sm8650.dtsi"));
        assert!(grant.covers("drivers/soc/qcom/smem.c"));
        assert!(!grant.covers("drivers/watchdog/gunyah_wdt.c"));
    }

    #[test]
    fn test_load_missing_file_means_no_grants() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(load(&dir.path().join("absent.toml")).unwrap().is_empty());
    }

    #[test]
    fn test_shipped_grants_parse_and_validate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("projects");
        for project in std::fs::read_dir(&root).unwrap().flatten() {
            let path = project.path().join("upstream_maintainers.toml");
            for grant in load(&path).unwrap_or_else(|e| panic!("{e:#}")) {
                validate_paths(&grant.paths)
                    .unwrap_or_else(|e| panic!("{}: {}: {e}", path.display(), grant.section));
            }
        }
    }
}
