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

//! Project profiles for the code notes engine.
//!
//! A profile declares everything project-specific the engine needs: how the
//! code is divided into ownership units, which files are source in which
//! language, and size limits. See `designs/DESIGN_CODE_NOTES_ENGINE.md`,
//! section 3. The engine core reads only these types.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::maintainers::CompiledPattern;

/// Location of a project's profile inside its prompt directory. Profiles
/// ship in the prompt bundle so they are available when `sashiko` runs
/// inside the target project's repository.
pub const PROFILE_RELPATH: &str = "notes/profile.toml";

/// Default maximum number of source lines in one component.
const DEFAULT_COMPONENT_LINES: usize = 15_000;
/// Default byte budget of one rendered unit note.
const DEFAULT_UNIT_NOTE_BYTES: usize = 16 * 1024;
/// Default byte budget of all notes loaded into one review.
const DEFAULT_REVIEW_NOTES_BYTES: usize = 48 * 1024;
/// Deepest directory level the `directories` unit source may split at.
const MAX_DIRECTORY_DEPTH: usize = 4;

/// A project's notes profile, as written in `notes/profile.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotesProfile {
    pub units: UnitsConfig,
    pub languages: LanguagesConfig,
    #[serde(default)]
    pub limits: Limits,
}

/// How a project's files are divided into ownership units.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum UnitsConfig {
    /// Each directory `depth` levels below a root is one unit; files
    /// directly inside a root (or in directories shallower than `depth`)
    /// form one unit named after their directory.
    Directories {
        roots: Vec<String>,
        #[serde(default = "default_directory_depth")]
        depth: usize,
        #[serde(default)]
        shallow_files: ShallowFiles,
    },
    /// Sections of the project's `MAINTAINERS` file.
    Maintainers {},
}

fn default_directory_depth() -> usize {
    1
}

/// What happens to files shallower than the unit depth (for depth 1, the
/// files directly inside a root).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShallowFiles {
    /// They form one unit named after their directory.
    #[default]
    Together,
    /// Each is its own unit named after the file without its extension, so
    /// a module file (`src/db.rs`) joins the directory unit of the same
    /// name (`src/db/`) when one exists. Suits languages where each file is
    /// a module, such as Rust.
    PerModule,
}

/// Source languages the engine has entry-point extraction for. Files of any
/// other kind can still be listed as `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    C,
    Cpp,
    Other,
}

/// Which files are source, per language, and which are skipped.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguagesConfig {
    /// Path globs per language (`*` stays within one directory, `**`
    /// crosses directories, a trailing `/` matches a whole directory).
    pub globs: BTreeMap<Language, Vec<String>>,
    /// Path globs excluded even when a language glob matches (generated
    /// files, test data, documentation).
    #[serde(default)]
    pub skip: Vec<String>,
}

/// Size limits that bound exploration and the rendered notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub component_lines: usize,
    pub unit_note_bytes: usize,
    pub review_notes_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            component_lines: DEFAULT_COMPONENT_LINES,
            unit_note_bytes: DEFAULT_UNIT_NOTE_BYTES,
            review_notes_bytes: DEFAULT_REVIEW_NOTES_BYTES,
        }
    }
}

impl NotesProfile {
    /// Parses and validates a profile from TOML text.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let profile: NotesProfile = toml::from_str(text).context("invalid notes profile")?;
        profile.validate()?;
        Ok(profile)
    }

    /// Reads, parses and validates the profile at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read notes profile {}", path.display()))?;
        Self::from_toml_str(&text).with_context(|| format!("in notes profile {}", path.display()))
    }

    fn validate(&self) -> Result<()> {
        if let UnitsConfig::Directories { roots, depth, .. } = &self.units {
            if roots.is_empty() || roots.iter().any(|r| r.trim().is_empty()) {
                bail!("units.roots must list at least one non-empty directory");
            }
            if !(1..=MAX_DIRECTORY_DEPTH).contains(depth) {
                bail!("units.depth must be between 1 and {MAX_DIRECTORY_DEPTH}, got {depth}");
            }
        }
        if self.languages.globs.is_empty() {
            bail!("languages.globs must name at least one language");
        }
        for (language, globs) in &self.languages.globs {
            if globs.is_empty() || globs.iter().any(|g| g.trim().is_empty()) {
                bail!("languages.globs.{language:?} must list at least one non-empty glob");
            }
        }
        if self.limits.component_lines == 0 {
            bail!("limits.component_lines must be positive");
        }
        Ok(())
    }

    /// Compiles the language globs for repeated matching.
    pub fn compiled_languages(&self) -> CompiledLanguages {
        CompiledLanguages {
            globs: self
                .languages
                .globs
                .iter()
                .map(|(language, globs)| {
                    (
                        *language,
                        globs.iter().map(|g| CompiledPattern::compile(g)).collect(),
                    )
                })
                .collect(),
            skip: self
                .languages
                .skip
                .iter()
                .map(|g| CompiledPattern::compile(g))
                .collect(),
        }
    }
}

/// Resolves the profile path: an explicit override, or the profile shipped
/// in the project's prompt directory.
pub fn resolve_profile_path(prompts_dir: &Path, override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    let path = prompts_dir.join(PROFILE_RELPATH);
    if !path.is_file() {
        bail!(
            "no notes profile at {}; this project has no profile yet (pass --profile to use one)",
            path.display()
        );
    }
    Ok(path)
}

/// Language globs compiled for matching file paths.
#[derive(Debug, Clone)]
pub struct CompiledLanguages {
    globs: Vec<(Language, Vec<CompiledPattern>)>,
    skip: Vec<CompiledPattern>,
}

impl CompiledLanguages {
    /// Returns the language of `path`, or `None` when the path is skipped or
    /// matches no language glob. Languages are tried in their declared
    /// order (`Language` ordering), so a path matched by two languages gets
    /// the first.
    pub fn classify(&self, path: &str) -> Option<Language> {
        if self.skip.iter().any(|p| p.is_match(path)) {
            return None;
        }
        self.globs
            .iter()
            .find(|(_, patterns)| patterns.iter().any(|p| p.is_match(path)))
            .map(|(language, _)| *language)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SASHIKO_LIKE: &str = r#"
[units]
source = "directories"
roots = ["src"]
depth = 1
shallow_files = "together"

[languages]
skip = ["src/migrations/"]

[languages.globs]
rust = ["src/**.rs"]
other = ["src/**.sql"]

[limits]
component_lines = 4000
"#;

    #[test]
    fn test_parse_directories_profile() {
        let profile = NotesProfile::from_toml_str(SASHIKO_LIKE).unwrap();
        assert_eq!(
            profile.units,
            UnitsConfig::Directories {
                roots: vec!["src".to_string()],
                depth: 1,
                shallow_files: ShallowFiles::Together,
            }
        );
        assert_eq!(profile.limits.component_lines, 4000);
        assert_eq!(profile.limits.unit_note_bytes, DEFAULT_UNIT_NOTE_BYTES);
    }

    #[test]
    fn test_parse_maintainers_profile() {
        let profile = NotesProfile::from_toml_str(
            "[units]\nsource = \"maintainers\"\n[languages.globs]\nc = [\"**.c\", \"**.h\"]\n",
        )
        .unwrap();
        assert_eq!(profile.units, UnitsConfig::Maintainers {});
        assert_eq!(profile.limits, Limits::default());
    }

    #[test]
    fn test_rejects_invalid_profiles() {
        let cases = [
            (
                "[units]\nsource = \"directories\"\nroots = []\n[languages.globs]\nrust = [\"**.rs\"]\n",
                "units.roots",
            ),
            (
                "[units]\nsource = \"directories\"\nroots = [\"src\"]\ndepth = 9\n[languages.globs]\nrust = [\"**.rs\"]\n",
                "units.depth",
            ),
            (
                "[units]\nsource = \"maintainers\"\n[languages.globs]\n",
                "languages.globs",
            ),
            (
                "[units]\nsource = \"maintainers\"\n[languages.globs]\nrust = []\n",
                "languages.globs",
            ),
            (
                "[units]\nsource = \"maintainers\"\n[languages.globs]\nrust = [\"**.rs\"]\n[limits]\ncomponent_lines = 0\n",
                "component_lines",
            ),
            (
                "[units]\nsource = \"codeowners\"\n[languages.globs]\nrust = [\"**.rs\"]\n",
                "invalid notes profile",
            ),
            (
                "[units]\nsource = \"maintainers\"\n[languages.globs]\ncobol = [\"**.cob\"]\n",
                "invalid notes profile",
            ),
            (
                "[units]\nsource = \"maintainers\"\nextra = 1\n[languages.globs]\nrust = [\"**.rs\"]\n",
                "invalid notes profile",
            ),
        ];
        for (text, expected) in cases {
            let err = format!("{:#}", NotesProfile::from_toml_str(text).unwrap_err());
            assert!(err.contains(expected), "expected {expected:?} in {err:?}");
        }
    }

    #[test]
    fn test_classify_applies_skip_before_languages() {
        let languages = NotesProfile::from_toml_str(SASHIKO_LIKE)
            .unwrap()
            .compiled_languages();
        assert_eq!(languages.classify("src/main.rs"), Some(Language::Rust));
        assert_eq!(languages.classify("src/ai/gemini.rs"), Some(Language::Rust));
        assert_eq!(languages.classify("src/schema.sql"), Some(Language::Other));
        assert_eq!(languages.classify("src/migrations/001_init.sql"), None);
        assert_eq!(languages.classify("tests/foo.rs"), None);
        assert_eq!(languages.classify("README.md"), None);
    }

    #[test]
    fn test_resolve_profile_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_profile_path(dir.path(), None).is_err());
        std::fs::create_dir_all(dir.path().join("notes")).unwrap();
        std::fs::write(dir.path().join(PROFILE_RELPATH), SASHIKO_LIKE).unwrap();
        assert_eq!(
            resolve_profile_path(dir.path(), None).unwrap(),
            dir.path().join(PROFILE_RELPATH)
        );
        let explicit = Path::new("/elsewhere/profile.toml");
        assert_eq!(
            resolve_profile_path(dir.path(), Some(explicit)).unwrap(),
            explicit
        );
    }
}
