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

//! Review-time selection of code notes (design section 10).
//!
//! A project can ship several notes sets, each built from one revision
//! (`notes/<set>/manifest.json` in its prompt directory). A review uses the
//! set whose revision is the newest ancestor of the patch's baseline, so it
//! never reads notes describing code newer than what the patch was written
//! against; when no set is an ancestor (an old or diverged baseline) it uses
//! the oldest set. Within the set, units are chosen deterministically from
//! the files the patch touches, and their notes are inlined up to a byte
//! budget.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;

use crate::git_cmd;
use crate::notes::output::{MANIFEST_FILE, Manifest};

/// Directory holding notes sets inside a project's prompt directory.
pub const NOTES_RELDIR: &str = "notes";

/// One notes set found in the prompt directory.
#[derive(Debug, Clone)]
pub struct NotesSet {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// Finds every notes set (`notes/<set>/manifest.json`) in `prompts_dir`.
/// Sets whose manifest does not parse are skipped with a warning, so one
/// broken set never disables the others.
pub fn load_sets(prompts_dir: &Path) -> Vec<NotesSet> {
    let root = prompts_dir.join(NOTES_RELDIR);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut sets = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.join(MANIFEST_FILE).is_file() {
            continue;
        }
        match Manifest::load(&dir) {
            Ok(Some(manifest)) => sets.push(NotesSet { dir, manifest }),
            Ok(None) => {}
            Err(err) => tracing::warn!("skipping notes set {}: {err:#}", dir.display()),
        }
    }
    sets.sort_by(|a, b| a.manifest.set.cmp(&b.manifest.set));
    sets
}

/// Picks the set for a review of a patch on `baseline`: the newest set
/// whose revision is an ancestor of the baseline, else the oldest set.
/// Sets whose revision is unknown to the repository are ignored.
pub async fn choose_set<'a>(
    repo: &Path,
    sets: &'a [NotesSet],
    baseline: &str,
) -> Result<Option<&'a NotesSet>> {
    let mut dated = Vec::new();
    for set in sets {
        if let Some(time) = commit_time(repo, &set.manifest.revision).await? {
            dated.push((time, set));
        }
    }
    if dated.is_empty() {
        return Ok(None);
    }
    dated.sort_by_key(|(time, _)| *time);
    for (_, set) in dated.iter().rev() {
        if is_ancestor(repo, &set.manifest.revision, baseline).await? {
            return Ok(Some(set));
        }
    }
    Ok(dated.first().map(|(_, set)| *set))
}

async fn commit_time(repo: &Path, rev: &str) -> Result<Option<i64>> {
    if rev.starts_with('-') {
        return Ok(None);
    }
    let output = git_cmd::in_dir_async(repo)
        .args(["log", "-1", "--format=%ct", rev, "--"])
        .output()
        .await
        .context("failed to run git log")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().parse().ok())
}

async fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    if ancestor.starts_with('-') || descendant.starts_with('-') {
        return Ok(false);
    }
    let status = git_cmd::in_dir_async(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .await
        .context("failed to run git merge-base")?;
    Ok(status.success())
}

/// A unit note chosen for a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedNote {
    pub unit: String,
    pub text: String,
}

/// Picks the unit notes for `touched_files`, most relevant first, within
/// `max_bytes`. A unit is relevant when it covers a touched file; a touched
/// file no unit covers (for example a new file) goes to the unit sharing
/// the longest directory prefix with it. Notes that do not fit are skipped.
pub fn select_notes(
    set: &NotesSet,
    touched_files: &[String],
    max_bytes: usize,
) -> Vec<SelectedNote> {
    let mut covered: HashMap<&str, &str> = HashMap::new();
    for (slug, unit) in &set.manifest.units {
        for file in &unit.files {
            covered.insert(file.path.as_str(), slug.as_str());
        }
    }
    let mut score: BTreeMap<&str, usize> = BTreeMap::new();
    for file in touched_files {
        let slug = covered
            .get(file.as_str())
            .copied()
            .or_else(|| nearest_unit(&covered, file));
        if let Some(slug) = slug {
            *score.entry(slug).or_default() += 1;
        }
    }
    let mut ranked: Vec<(&str, usize)> = score.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

    let mut used = 0usize;
    let mut out = Vec::new();
    for (slug, _) in ranked {
        let Some(unit) = set.manifest.units.get(slug) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(set.dir.join(&unit.note)) else {
            tracing::warn!("notes set {} lacks {}", set.manifest.set, unit.note);
            continue;
        };
        let text = strip_citations(&text);
        if used + text.len() > max_bytes {
            continue;
        }
        used += text.len();
        out.push(SelectedNote {
            unit: unit.name.clone(),
            text,
        });
    }
    out
}

/// The unit whose covered files share the longest directory prefix (at
/// least one directory) with `file`.
fn nearest_unit<'a>(covered: &HashMap<&'a str, &'a str>, file: &str) -> Option<&'a str> {
    let dirs: Vec<&str> = file.split('/').collect();
    for depth in (1..dirs.len()).rev() {
        let prefix = format!("{}/", dirs[..depth].join("/"));
        let mut best: Option<(&str, usize)> = None;
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (path, slug) in covered {
            if path.starts_with(&prefix) {
                *counts.entry(slug).or_default() += 1;
            }
        }
        for (slug, count) in counts {
            if best.is_none_or(|(_, c)| count > c) {
                best = Some((slug, count));
            }
        }
        if let Some((slug, _)) = best {
            return Some(slug);
        }
    }
    None
}

static CITATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" ?\[F\d+(?:\s*,\s*F\d+)*\]").expect("valid regex"));

/// Fact citations trace a note to its facts sidecar; a reviewer does not
/// need them, so they are dropped to save tokens.
fn strip_citations(note: &str) -> String {
    CITATION.replace_all(note, "").into_owned()
}

/// Renders the review prompt block for `notes` from `set`.
pub fn render_block(set: &NotesSet, notes: &[SelectedNote]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut block = format!(
        "\n\n<code_notes set=\"{}\" revision=\"{}\">\nThese notes were built by reading and verifying the code of the units this patch touches, at the revision above. Use them to understand invariants, contracts and known pitfalls the patch must respect. They describe the code at that revision: if the patch, or code between that revision and the baseline, changed something a note describes, trust the current code.\n",
        set.manifest.set, set.manifest.revision
    );
    for note in notes {
        block.push_str(&format!(
            "\n<unit_note unit=\"{}\">\n{}\n</unit_note>\n",
            note.unit,
            note.text.trim()
        ));
    }
    block.push_str("</code_notes>");
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::output::{ManifestFile, ManifestUnit, UnitFacts, write_unit};
    use crate::notes::scope::test_repo;

    fn entry(name: &str, slug: &str, files: &[&str]) -> ManifestUnit {
        ManifestUnit {
            name: name.to_string(),
            note: format!("units/{slug}.md"),
            facts: format!("units/{slug}.facts.json"),
            agenda_version: "a".to_string(),
            model: "m".to_string(),
            built_at: "t".to_string(),
            note_bytes: 0,
            confirmed_facts: 1,
            files: files
                .iter()
                .map(|f| ManifestFile {
                    path: f.to_string(),
                    blob: "b".to_string(),
                })
                .collect(),
            symbols: vec![],
        }
    }

    fn write_set(root: &Path, set: &str, revision: &str, units: &[(&str, &str, &[&str], &str)]) {
        let dir = root.join(NOTES_RELDIR).join(set);
        for (name, slug, files, note) in units {
            let facts = UnitFacts {
                unit: name.to_string(),
                revision: revision.to_string(),
                facts: vec![],
                dismissed_hypotheses: vec![],
                open_questions: vec![],
            };
            write_unit(
                &dir,
                ("p", set, revision),
                slug,
                note,
                &facts,
                entry(name, slug, files),
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn test_choose_set_by_ancestry() {
        let repo = tempfile::tempdir().unwrap();
        let old = test_repo::commit_files(repo.path(), &[("a.c", "1\n")]);
        // Commit times have one-second resolution; keep the order unambiguous.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let mid = test_repo::commit_files(repo.path(), &[("a.c", "2\n")]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let new = test_repo::commit_files(repo.path(), &[("a.c", "3\n")]);

        let prompts = tempfile::tempdir().unwrap();
        write_set(
            prompts.path(),
            "old",
            &old,
            &[("u", "u", &["a.c"], "# u\n")],
        );
        write_set(
            prompts.path(),
            "new",
            &new,
            &[("u", "u", &["a.c"], "# u\n")],
        );
        let sets = load_sets(prompts.path());
        assert_eq!(sets.len(), 2);

        let chosen = |baseline: String| {
            let sets = sets.clone();
            let repo = repo.path().to_path_buf();
            async move {
                choose_set(&repo, &sets, &baseline)
                    .await
                    .unwrap()
                    .map(|s| s.manifest.set.clone())
            }
        };
        // The newest set that is an ancestor of the baseline.
        assert_eq!(chosen(new.clone()).await.as_deref(), Some("new"));
        assert_eq!(chosen(mid.clone()).await.as_deref(), Some("old"));
        assert_eq!(chosen(old.clone()).await.as_deref(), Some("old"));
        // A set whose revision the repository does not know is ignored.
        write_set(
            prompts.path(),
            "ghost",
            &"f".repeat(40),
            &[("u", "u", &["a.c"], "# u\n")],
        );
        let sets = load_sets(prompts.path());
        assert_eq!(sets.len(), 3);
        let picked = choose_set(repo.path(), &sets, &new).await.unwrap().unwrap();
        assert_eq!(picked.manifest.set, "new");
    }

    #[test]
    fn test_select_notes_by_touched_files_and_budget() {
        let prompts = tempfile::tempdir().unwrap();
        write_set(
            prompts.path(),
            "s",
            "r",
            &[
                (
                    "net",
                    "net",
                    &["net/core/dev.c", "net/core/sock.c"],
                    "# net\n- Hold the lock. [F1, F2]\n",
                ),
                ("mm", "mm", &["mm/slab.c"], "# mm\n- Slabs. [F3]\n"),
                (
                    "big",
                    "big",
                    &["drivers/x.c"],
                    &format!("# big\n{}\n", "x".repeat(500)),
                ),
            ],
        );
        let sets = load_sets(prompts.path());
        let set = &sets[0];

        let touched = vec![
            "net/core/dev.c".to_string(),
            "net/core/new_file.c".to_string(),
            "mm/slab.c".to_string(),
            "drivers/x.c".to_string(),
        ];
        let notes = select_notes(set, &touched, 200);
        let units: Vec<&str> = notes.iter().map(|n| n.unit.as_str()).collect();
        // net covers two touched files (one by directory), mm one; big does
        // not fit the budget.
        assert_eq!(units, vec!["net", "mm"]);
        assert_eq!(notes[0].text, "# net\n- Hold the lock.\n");

        let block = render_block(set, &notes);
        assert!(block.contains("<code_notes set=\"s\" revision=\"r\">"));
        assert!(block.contains("<unit_note unit=\"mm\">"));
        assert!(render_block(set, &[]).is_empty());
        assert!(select_notes(set, &["docs/x.rst".to_string()], 1000).is_empty());
    }
}
