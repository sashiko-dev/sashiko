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

//! Writes a notes set (design section 8.1):
//!
//! ```text
//! <set dir>/manifest.json
//! <set dir>/units/<slug>.md          review-facing note
//! <set dir>/units/<slug>.facts.json  every fact with evidence and verdict
//! ```
//!
//! A set holds notes built from one revision of one project. The manifest
//! records, per unit, the files it covers with their blob IDs (so refresh
//! can tell what changed), the symbols its facts are about, and how and when
//! it was built.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::notes::facts::{DismissedHypothesis, VerifiedFact};

pub const MANIFEST_FILE: &str = "manifest.json";
/// Lock file serializing manifest updates by concurrent builds of one set.
const LOCK_FILE: &str = ".manifest.lock";
pub const UNITS_DIR: &str = "units";

/// One covered file and its blob at the set's revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub blob: String,
}

/// Manifest entry for one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestUnit {
    pub name: String,
    pub note: String,
    pub facts: String,
    pub agenda_version: String,
    pub model: String,
    pub built_at: String,
    pub note_bytes: usize,
    pub confirmed_facts: usize,
    pub files: Vec<ManifestFile>,
    pub symbols: Vec<String>,
}

/// The manifest of one notes set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub project: String,
    pub set: String,
    pub revision: String,
    #[serde(default)]
    pub units: BTreeMap<String, ManifestUnit>,
}

impl Manifest {
    /// Reads the manifest in `set_dir`, or returns `None` if there is none.
    pub fn load(set_dir: &Path) -> Result<Option<Self>> {
        let path = set_dir.join(MANIFEST_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let manifest =
            serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        Ok(Some(manifest))
    }
}

/// Everything written for one unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitFacts {
    pub unit: String,
    pub revision: String,
    pub facts: Vec<VerifiedFact>,
    pub dismissed_hypotheses: Vec<DismissedHypothesis>,
    pub open_questions: Vec<String>,
}

/// Writes one unit's note and facts into `set_dir` and records it in the
/// manifest. A set holds one revision of one project: writing a unit built
/// from another revision or project into an existing set is an error.
pub fn write_unit(
    set_dir: &Path,
    manifest_header: (&str, &str, &str),
    slug: &str,
    note: &str,
    facts: &UnitFacts,
    entry: ManifestUnit,
) -> Result<()> {
    let (project, set, revision) = manifest_header;
    let units_dir = set_dir.join(UNITS_DIR);
    std::fs::create_dir_all(&units_dir)
        .with_context(|| format!("failed to create {}", units_dir.display()))?;

    // Several builds may write units of one set at once; the manifest is a
    // read-modify-write, so it is updated under an exclusive lock, released
    // when the file is dropped.
    let lock_path = set_dir.join(LOCK_FILE);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;

    let mut manifest = Manifest::load(set_dir)?.unwrap_or_else(|| Manifest {
        project: project.to_string(),
        set: set.to_string(),
        revision: revision.to_string(),
        units: BTreeMap::new(),
    });
    if manifest.project != project || manifest.set != set || manifest.revision != revision {
        bail!(
            "{} holds set {:?} of {} at {}; refusing to add a unit of set {set:?} of {project} at {revision}",
            set_dir.display(),
            manifest.set,
            manifest.project,
            manifest.revision
        );
    }

    write_atomic(&units_dir.join(format!("{slug}.md")), note.as_bytes())?;
    write_atomic(
        &units_dir.join(format!("{slug}.facts.json")),
        format!("{}\n", serde_json::to_string_pretty(facts)?).as_bytes(),
    )?;
    manifest.units.insert(slug.to_string(), entry);
    let written = write_atomic(
        &set_dir.join(MANIFEST_FILE),
        format!("{}\n", serde_json::to_string_pretty(&manifest)?).as_bytes(),
    );
    drop(lock);
    written
}

/// Writes through a temporary file and renames, so an interrupted build
/// never leaves a truncated note or manifest behind. The temporary name is
/// unique per process, so concurrent builds never share one.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to move {} into place", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> ManifestUnit {
        ManifestUnit {
            name: name.to_string(),
            note: format!("units/{name}.md"),
            facts: format!("units/{name}.facts.json"),
            agenda_version: "abc".to_string(),
            model: "m".to_string(),
            built_at: "2026-10-15T00:00:00Z".to_string(),
            note_bytes: 10,
            confirmed_facts: 1,
            files: vec![ManifestFile {
                path: "src/a.rs".to_string(),
                blob: "123".to_string(),
            }],
            symbols: vec!["f".to_string()],
        }
    }

    fn facts(unit: &str) -> UnitFacts {
        UnitFacts {
            unit: unit.to_string(),
            revision: "r1".to_string(),
            facts: vec![],
            dismissed_hypotheses: vec![],
            open_questions: vec![],
        }
    }

    #[test]
    fn test_write_units_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let header = ("sashiko", "dev", "r1");
        write_unit(
            dir.path(),
            header,
            "src-a",
            "# src/a\n",
            &facts("src/a"),
            entry("src-a"),
        )
        .unwrap();
        write_unit(
            dir.path(),
            header,
            "src-b",
            "# src/b\n",
            &facts("src/b"),
            entry("src-b"),
        )
        .unwrap();
        let manifest = Manifest::load(dir.path()).unwrap().unwrap();
        assert_eq!(manifest.revision, "r1");
        assert_eq!(
            manifest.units.keys().collect::<Vec<_>>(),
            vec!["src-a", "src-b"]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("units/src-b.md")).unwrap(),
            "# src/b\n"
        );
        assert!(dir.path().join("units/src-a.facts.json").exists());
        assert!(
            std::fs::read_dir(dir.path().join("units"))
                .unwrap()
                .all(|e| !e.unwrap().file_name().to_string_lossy().contains(".tmp"))
        );

        // Rebuilding a unit replaces its entry.
        let mut rebuilt = entry("src-a");
        rebuilt.note_bytes = 99;
        write_unit(
            dir.path(),
            header,
            "src-a",
            "# src/a v2\n",
            &facts("src/a"),
            rebuilt,
        )
        .unwrap();
        let manifest = Manifest::load(dir.path()).unwrap().unwrap();
        assert_eq!(manifest.units["src-a"].note_bytes, 99);

        let err = write_unit(
            dir.path(),
            ("sashiko", "dev", "r2"),
            "src-c",
            "# c\n",
            &facts("c"),
            entry("src-c"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing"));
    }

    #[test]
    fn test_concurrent_writers_keep_every_unit() {
        let dir = tempfile::tempdir().unwrap();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    let slug = format!("u{i}");
                    write_unit(
                        &path,
                        ("sashiko", "dev", "r1"),
                        &slug,
                        "# u\n",
                        &facts(&slug),
                        entry(&slug),
                    )
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let manifest = Manifest::load(dir.path()).unwrap().unwrap();
        assert_eq!(manifest.units.len(), 8);
    }
}
