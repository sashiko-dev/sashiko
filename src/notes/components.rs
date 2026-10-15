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

//! Splits a large unit into components small enough for one exploration
//! stage. Splitting follows the directory tree: a directory that fits the
//! limit stays whole, a larger one is split along its subdirectories, and
//! files are never split. Adjacent pieces (in path order, which keeps a
//! Rust `foo.rs` next to its `foo/` directory) are then packed together up
//! to the limit, so a unit does not dissolve into many tiny components.

use std::collections::BTreeMap;

use crate::notes::scope::SourceFile;
use crate::notes::units::Unit;

/// A group of files explored together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub name: String,
    pub files: Vec<SourceFile>,
}

impl Component {
    pub fn lines(&self) -> usize {
        self.files.iter().map(|f| f.lines).sum()
    }
}

/// Splits `unit` into components of at most `max_lines` lines each. A
/// single file larger than the limit becomes a component of its own.
pub fn split_unit(unit: &Unit, max_lines: usize) -> Vec<Component> {
    let mut files = unit.files.clone();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    if files.iter().map(|f| f.lines).sum::<usize>() <= max_lines {
        return vec![Component {
            name: unit.name.clone(),
            files,
        }];
    }
    let mut pieces = Vec::new();
    split_dir(&files, common_dir_depth(&files), max_lines, &mut pieces);
    let mut packed = pack(pieces, max_lines);
    // Grouping by directory key orders `acp/` before `acp-config.c`; keep
    // each component's files in plain path order.
    for group in &mut packed {
        group.sort_by(|a, b| a.path.cmp(&b.path));
    }
    name_components(packed)
}

/// Number of leading directory components shared by every file.
fn common_dir_depth(files: &[SourceFile]) -> usize {
    let mut iter = files.iter().map(|f| dir_components(&f.path));
    let Some(first) = iter.next() else {
        return 0;
    };
    let mut depth = first.len();
    for dirs in iter {
        depth = depth.min(
            first
                .iter()
                .zip(dirs.iter())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    depth
}

fn dir_components(path: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    parts
}

/// Splits `files`, all sharing their first `depth` directories, into pieces
/// that each fit `max_lines` where possible.
fn split_dir(files: &[SourceFile], depth: usize, max_lines: usize, out: &mut Vec<Vec<SourceFile>>) {
    // Group by the next path component: a subdirectory, or the file itself.
    let mut groups: BTreeMap<String, Vec<SourceFile>> = BTreeMap::new();
    for file in files {
        let parts: Vec<&str> = file.path.split('/').collect();
        let key = parts[..=depth.min(parts.len() - 1)].join("/");
        groups.entry(key).or_default().push(file.clone());
    }
    for (_, group) in groups {
        let lines: usize = group.iter().map(|f| f.lines).sum();
        let is_single_file = group.len() == 1 && dir_components(&group[0].path).len() <= depth;
        if lines <= max_lines || is_single_file {
            out.push(group);
        } else {
            split_dir(&group, depth + 1, max_lines, out);
        }
    }
}

/// Packs adjacent pieces greedily into components of at most `max_lines`.
fn pack(pieces: Vec<Vec<SourceFile>>, max_lines: usize) -> Vec<Vec<SourceFile>> {
    let mut out: Vec<Vec<SourceFile>> = Vec::new();
    let mut current: Vec<SourceFile> = Vec::new();
    let mut current_lines = 0usize;
    for piece in pieces {
        let lines: usize = piece.iter().map(|f| f.lines).sum();
        if !current.is_empty() && current_lines.saturating_add(lines) > max_lines {
            out.push(std::mem::take(&mut current));
            current_lines = 0;
        }
        current_lines = current_lines.saturating_add(lines);
        current.extend(piece);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Names each component after the deepest directory its files share, or
/// the file for a single-file component, numbering duplicates.
fn name_components(groups: Vec<Vec<SourceFile>>) -> Vec<Component> {
    let base_names: Vec<String> = groups
        .iter()
        .map(|files| {
            if let [only] = files.as_slice() {
                return only.path.clone();
            }
            let depth = common_dir_depth(files);
            let dirs = dir_components(&files[0].path);
            if depth == 0 {
                "(top level)".to_string()
            } else {
                dirs[..depth].join("/")
            }
        })
        .collect();
    let mut totals: BTreeMap<&str, usize> = BTreeMap::new();
    for name in &base_names {
        *totals.entry(name.as_str()).or_default() += 1;
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    groups
        .into_iter()
        .zip(base_names.iter())
        .map(|(files, base)| {
            let name = if totals[base.as_str()] > 1 {
                let n = seen.entry(base.clone()).or_default();
                *n += 1;
                format!("{base} (part {n})")
            } else {
                base.clone()
            };
            Component { name, files }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::profile::Language;

    fn unit(files: &[(&str, usize)]) -> Unit {
        Unit {
            name: "src".to_string(),
            slug: "src".to_string(),
            files: files
                .iter()
                .map(|(path, lines)| SourceFile {
                    path: path.to_string(),
                    language: Language::Rust,
                    lines: *lines,
                    test_lines: 0,
                })
                .collect(),
            shared: Vec::new(),
        }
    }

    fn summary(components: &[Component]) -> Vec<(String, usize, usize)> {
        components
            .iter()
            .map(|c| (c.name.clone(), c.files.len(), c.lines()))
            .collect()
    }

    #[test]
    fn test_small_unit_is_one_component() {
        let u = unit(&[("src/a.rs", 10), ("src/b/c.rs", 20)]);
        assert_eq!(
            summary(&split_unit(&u, 100)),
            vec![("src".to_string(), 2, 30)]
        );
    }

    #[test]
    fn test_splits_along_directories_and_packs() {
        let u = unit(&[
            ("src/ai/gemini.rs", 300),
            ("src/ai/claude.rs", 300),
            ("src/main.rs", 900),
            ("src/toolbox/git_grep.rs", 100),
            ("src/toolbox/mod.rs", 100),
            ("src/workflow/engine.rs", 500),
            ("src/workflow/stage.rs", 450),
        ]);
        let components = split_unit(&u, 1000);
        assert_eq!(
            summary(&components),
            vec![
                ("src/ai".to_string(), 2, 600),
                ("src/main.rs".to_string(), 1, 900),
                ("src/toolbox".to_string(), 2, 200),
                ("src/workflow".to_string(), 2, 950),
            ]
        );
        assert!(components.iter().all(|c| c.lines() <= 1000));
    }

    #[test]
    fn test_oversized_file_stands_alone_and_parts_are_named() {
        let u = unit(&[
            ("src/big.rs", 5000),
            ("src/w/a.rs", 600),
            ("src/w/b.rs", 600),
            ("src/w/c.rs", 600),
        ]);
        let components = split_unit(&u, 1000);
        assert_eq!(
            summary(&components),
            vec![
                ("src/big.rs".to_string(), 1, 5000),
                ("src/w/a.rs".to_string(), 1, 600),
                ("src/w/b.rs".to_string(), 1, 600),
                ("src/w/c.rs".to_string(), 1, 600),
            ]
        );
        // Two packed pieces under the same directory get numbered names.
        let u = unit(&[
            ("src/w/a.rs", 400),
            ("src/w/b.rs", 400),
            ("src/w/c.rs", 400),
            ("src/w/d.rs", 400),
        ]);
        assert_eq!(
            summary(&split_unit(&u, 1000)),
            vec![
                ("src/w (part 1)".to_string(), 2, 800),
                ("src/w (part 2)".to_string(), 2, 800),
            ]
        );
    }

    #[test]
    fn test_every_file_lands_in_exactly_one_component() {
        let files: Vec<(String, usize)> = (0..50)
            .map(|i| (format!("src/d{}/f{}.rs", i % 7, i), 37 + i * 11))
            .collect();
        let refs: Vec<(&str, usize)> = files.iter().map(|(p, l)| (p.as_str(), *l)).collect();
        let u = unit(&refs);
        let components = split_unit(&u, 800);
        let mut seen: Vec<&str> = components
            .iter()
            .flat_map(|c| c.files.iter().map(|f| f.path.as_str()))
            .collect();
        seen.sort();
        let mut expected: Vec<&str> = refs.iter().map(|(p, _)| *p).collect();
        expected.sort();
        assert_eq!(seen, expected);
        assert!(
            components
                .iter()
                .all(|c| c.lines() <= 800 || c.files.len() == 1)
        );
        assert!(
            components
                .iter()
                .all(|c| c.files.windows(2).all(|w| w[0].path < w[1].path))
        );
    }
}
