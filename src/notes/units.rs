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

//! Unit sources: how a project's source files are divided into ownership
//! units, the research jobs of the notes engine. Each source answers which
//! units own a file, most specific first; the shared assembly below turns
//! that into units, applying the overlap policy of the design (section
//! 3.1): a file is explored by its most specific owner and listed as shared
//! by the others.

use std::collections::{BTreeMap, HashSet};

use crate::maintainers::{FileMatcher, MaintainersIndex};
use crate::notes::profile::{ShallowFiles, UnitsConfig};
use crate::notes::scope::SourceFile;

/// Answers which units own a file.
pub trait UnitSource {
    /// Names of the units owning `path`, most specific first. Empty when no
    /// unit owns the file.
    fn owners(&self, path: &str) -> Vec<String>;
}

/// One ownership unit with the files it explores and the files it shares
/// with more specific units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    pub slug: String,
    pub files: Vec<SourceFile>,
    pub shared: Vec<String>,
}

impl Unit {
    /// Total source lines of the files this unit explores.
    pub fn lines(&self) -> usize {
        self.files.iter().map(|f| f.lines).sum()
    }
}

/// The result of dividing a project's files into units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitAssignment {
    /// Units sorted by name; units that own no file are omitted.
    pub units: Vec<Unit>,
    /// Source files no unit owns.
    pub unassigned: Vec<SourceFile>,
}

/// Divides `files` into units using `source`.
pub fn assign_units(source: &dyn UnitSource, files: &[SourceFile]) -> UnitAssignment {
    let mut owned: BTreeMap<String, Vec<SourceFile>> = BTreeMap::new();
    let mut shared: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unassigned = Vec::new();
    for file in files {
        let owners = source.owners(&file.path);
        let Some((primary, others)) = owners.split_first() else {
            unassigned.push(file.clone());
            continue;
        };
        owned.entry(primary.clone()).or_default().push(file.clone());
        // A source may name one unit twice (two MAINTAINERS sections can
        // share a title); each unit lists a shared file once.
        let mut seen: HashSet<&str> = HashSet::from([primary.as_str()]);
        for other in others {
            if seen.insert(other.as_str()) {
                shared
                    .entry(other.clone())
                    .or_default()
                    .push(file.path.clone());
            }
        }
    }

    let mut used_slugs = HashSet::new();
    let units = owned
        .into_iter()
        .map(|(name, files)| {
            let slug = unique_slug(&name, &mut used_slugs);
            let shared = shared.remove(&name).unwrap_or_default();
            Unit {
                name,
                slug,
                files,
                shared,
            }
        })
        .collect();
    UnitAssignment { units, unassigned }
}

/// Lowercase ASCII slug for a unit name, made unique within one assignment.
fn unique_slug(name: &str, used: &mut HashSet<String>) -> String {
    let mut base = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            base.push(ch.to_ascii_lowercase());
        } else if !base.ends_with('-') {
            base.push('-');
        }
    }
    let base = base.trim_matches('-');
    let base = if base.is_empty() { "unit" } else { base };
    let mut slug = base.to_string();
    let mut n = 2;
    while !used.insert(slug.clone()) {
        slug = format!("{base}-{n}");
        n += 1;
    }
    slug
}

/// Units from directories below configured roots (`units.source =
/// "directories"`).
pub struct DirectoryUnits {
    roots: Vec<String>,
    depth: usize,
    shallow_files: ShallowFiles,
}

impl DirectoryUnits {
    pub fn new(roots: &[String], depth: usize, shallow_files: ShallowFiles) -> Self {
        Self {
            roots: roots.iter().map(|r| normalize_root(r)).collect(),
            depth,
            shallow_files,
        }
    }
}

/// Normalizes a configured directory root to a repository-relative prefix
/// without leading `./` or surrounding slashes. The repository root (`.`,
/// `./` or `/`) normalizes to the empty string. Profile validation rejects
/// roots that are absolute or contain `..` before they reach this.
pub fn normalize_root(root: &str) -> String {
    let mut root = root.trim();
    while let Some(rest) = root.strip_prefix("./") {
        root = rest;
    }
    let root = root.trim_matches('/');
    if root == "." {
        String::new()
    } else {
        root.to_string()
    }
}

impl UnitSource for DirectoryUnits {
    /// A file `depth` or more directories below a root belongs to the
    /// directory at that depth. A shallower file belongs to its own
    /// directory, or with `ShallowFiles::PerModule` to a unit named after
    /// the file without its extension. The first matching root wins, so
    /// directory units never overlap.
    fn owners(&self, path: &str) -> Vec<String> {
        for root in &self.roots {
            let rel = if root.is_empty() {
                Some(path)
            } else {
                path.strip_prefix(root.as_str())
                    .and_then(|r| r.strip_prefix('/'))
            };
            let Some(rel) = rel else {
                continue;
            };
            let dirs: Vec<&str> = rel.split('/').collect();
            // The last component is the file name.
            let dir_count = dirs.len().saturating_sub(1);
            let take = dir_count.min(self.depth);
            let mut parts: Vec<&str> = Vec::new();
            if !root.is_empty() {
                parts.push(root);
            }
            parts.extend(&dirs[..take]);
            if take < self.depth && self.shallow_files == ShallowFiles::PerModule {
                let file_name = dirs[dirs.len() - 1];
                parts.push(
                    file_name
                        .split_once('.')
                        .map_or(file_name, |(stem, _)| stem),
                );
            }
            let unit = if parts.is_empty() {
                ".".to_string()
            } else {
                parts.join("/")
            };
            return vec![unit];
        }
        Vec::new()
    }
}

/// Units from `MAINTAINERS` sections (`units.source = "maintainers"`).
/// Catch-all sections (`F: *`) never own files: a file only they match is
/// unassigned rather than explored as part of a tree-wide unit.
pub struct MaintainersUnits<'a> {
    matcher: FileMatcher<'a>,
    catch_all: HashSet<String>,
}

impl<'a> MaintainersUnits<'a> {
    pub fn new(index: &'a MaintainersIndex) -> Self {
        let catch_all = index
            .sections()
            .iter()
            .filter(|s| s.catch_all)
            .map(|s| s.name.clone())
            .collect();
        Self {
            matcher: index.file_matcher(),
            catch_all,
        }
    }
}

impl UnitSource for MaintainersUnits<'_> {
    fn owners(&self, path: &str) -> Vec<String> {
        self.matcher
            .match_file(path)
            .into_iter()
            .filter(|name| !self.catch_all.contains(name))
            .collect()
    }
}

/// Builds the unit source a profile asks for. `maintainers` is required for
/// the `maintainers` source and ignored otherwise.
pub fn unit_source<'a>(
    config: &UnitsConfig,
    maintainers: Option<&'a MaintainersIndex>,
) -> anyhow::Result<Box<dyn UnitSource + 'a>> {
    match config {
        UnitsConfig::Directories {
            roots,
            depth,
            shallow_files,
        } => Ok(Box::new(DirectoryUnits::new(roots, *depth, *shallow_files))),
        UnitsConfig::Maintainers {} => {
            let index = maintainers.ok_or_else(|| {
                anyhow::anyhow!("the maintainers unit source needs a MAINTAINERS file")
            })?;
            Ok(Box::new(MaintainersUnits::new(index)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::profile::Language;

    fn file(path: &str, lines: usize) -> SourceFile {
        SourceFile {
            path: path.to_string(),
            language: Language::Rust,
            lines,
            test_lines: 0,
        }
    }

    fn names(assignment: &UnitAssignment) -> Vec<(&str, Vec<&str>)> {
        assignment
            .units
            .iter()
            .map(|u| {
                (
                    u.name.as_str(),
                    u.files.iter().map(|f| f.path.as_str()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn test_directory_units_depth_one() {
        let source = DirectoryUnits::new(&["src/".to_string()], 1, ShallowFiles::Together);
        let files = [
            file("src/main.rs", 10),
            file("src/ai/gemini.rs", 20),
            file("src/ai/cache/mod.rs", 5),
            file("src/toolbox/mod.rs", 7),
            file("tests/it.rs", 3),
        ];
        let assignment = assign_units(&source, &files);
        assert_eq!(
            names(&assignment),
            vec![
                ("src", vec!["src/main.rs"]),
                ("src/ai", vec!["src/ai/gemini.rs", "src/ai/cache/mod.rs"]),
                ("src/toolbox", vec!["src/toolbox/mod.rs"]),
            ]
        );
        assert_eq!(assignment.unassigned, vec![file("tests/it.rs", 3)]);
        assert_eq!(assignment.units[1].lines(), 25);
        assert_eq!(assignment.units[1].slug, "src-ai");
    }

    #[test]
    fn test_directory_units_depth_two() {
        let source = DirectoryUnits::new(&["src".to_string()], 2, ShallowFiles::Together);
        assert_eq!(source.owners("src/lib.rs"), vec!["src"]);
        assert_eq!(source.owners("src/ai/gemini.rs"), vec!["src/ai"]);
        assert_eq!(source.owners("src/ai/cache/db.rs"), vec!["src/ai/cache"]);
        assert_eq!(source.owners("src/ai/cache/x/y.rs"), vec!["src/ai/cache"]);
        assert!(source.owners("srcx/lib.rs").is_empty());
    }

    #[test]
    fn test_directory_units_per_module() {
        let source = DirectoryUnits::new(&["src".to_string()], 1, ShallowFiles::PerModule);
        assert_eq!(source.owners("src/db.rs"), vec!["src/db"]);
        assert_eq!(source.owners("src/main.rs"), vec!["src/main"]);
        // A module file joins the directory unit of the same name.
        assert_eq!(source.owners("src/git_ops.rs"), vec!["src/git_ops"]);
        assert_eq!(source.owners("src/git_ops/apply.rs"), vec!["src/git_ops"]);
        assert_eq!(source.owners("src/ai/cache/db.rs"), vec!["src/ai"]);
    }

    #[test]
    fn test_directory_units_repository_root() {
        for root in [".", "./", "/", ""] {
            let source = DirectoryUnits::new(&[root.to_string()], 1, ShallowFiles::Together);
            assert_eq!(source.owners("build.rs"), vec!["."], "root {root:?}");
            assert_eq!(source.owners("src/main.rs"), vec!["src"], "root {root:?}");
            assert_eq!(source.owners("src/ai/a.rs"), vec!["src"], "root {root:?}");
        }
        let source = DirectoryUnits::new(&[".".to_string()], 1, ShallowFiles::PerModule);
        assert_eq!(source.owners("build.rs"), vec!["build"]);
        assert_eq!(normalize_root("././src/"), "src");
    }

    #[test]
    fn test_duplicate_owner_names_share_a_file_once() {
        struct Dup;
        impl UnitSource for Dup {
            fn owners(&self, path: &str) -> Vec<String> {
                let names: &[&str] = if path == "a.c" {
                    &["A", "B", "B", "A"]
                } else {
                    &["B"]
                };
                names.iter().map(|s| s.to_string()).collect()
            }
        }
        let assignment = assign_units(&Dup, &[file("a.c", 1), file("b.c", 1)]);
        let b = assignment.units.iter().find(|u| u.name == "B").unwrap();
        assert_eq!(b.shared, vec!["a.c"]);
        let a = assignment.units.iter().find(|u| u.name == "A").unwrap();
        assert!(a.shared.is_empty());
    }

    #[test]
    fn test_maintainers_units_overlap_and_catch_all() {
        let maintainers = "\
Intro text.

Maintainers List
----------------

NETWORKING [GENERAL]
M:\tA <a@example.com>
F:\tnet/
F:\tinclude/linux/netdevice.h

NETWORKING [IPv4]
M:\tB <b@example.com>
F:\tnet/ipv4/

THE REST
M:\tC <c@example.com>
F:\t*
F:\t*/
";
        let index = MaintainersIndex::from_reader(maintainers.as_bytes()).unwrap();
        let source = MaintainersUnits::new(&index);
        let files = [
            file("net/core/dev.c", 100),
            file("net/ipv4/tcp.c", 200),
            file("include/linux/netdevice.h", 50),
            file("Makefile", 9),
        ];
        let assignment = assign_units(&source, &files);
        assert_eq!(
            names(&assignment),
            vec![
                (
                    "NETWORKING [GENERAL]",
                    vec!["net/core/dev.c", "include/linux/netdevice.h"]
                ),
                ("NETWORKING [IPv4]", vec!["net/ipv4/tcp.c"]),
            ]
        );
        assert_eq!(assignment.units[0].shared, vec!["net/ipv4/tcp.c"]);
        assert_eq!(assignment.units[0].slug, "networking-general");
        assert_eq!(assignment.unassigned, vec![file("Makefile", 9)]);
    }

    #[test]
    fn test_unique_slugs() {
        let mut used = HashSet::new();
        assert_eq!(unique_slug("A/B", &mut used), "a-b");
        assert_eq!(unique_slug("a b", &mut used), "a-b-2");
        assert_eq!(unique_slug("***", &mut used), "unit");
    }

    #[test]
    fn test_unit_source_requires_maintainers() {
        assert!(unit_source(&UnitsConfig::Maintainers {}, None).is_err());
        let dirs = UnitsConfig::Directories {
            roots: vec!["src".to_string()],
            depth: 1,
            shallow_files: ShallowFiles::Together,
        };
        assert_eq!(
            unit_source(&dirs, None).unwrap().owners("src/a.rs"),
            vec!["src"]
        );
    }
}
