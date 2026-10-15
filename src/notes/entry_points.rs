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

//! Language adapters: cheap, deterministic entry-point extraction that gives
//! the survey stage a starting map of each component before any LLM turn
//! is spent. Each language contributes a few `git grep` patterns over the
//! pinned revision; matches are mapped back to the scoped source files and
//! named with a stricter regex.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use regex::Regex;

use crate::git_cmd;
use crate::notes::profile::Language;
use crate::notes::scope::{SourceFile, parse_grep_records};

/// What kind of entry point a match is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Function,
    Type,
    Trait,
    TraitImpl,
    Module,
    Constant,
    Export,
    OpsTable,
}

/// One entry point found at the pinned revision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EntryPoint {
    pub kind: EntryKind,
    pub name: String,
    pub path: String,
    pub line: usize,
}

/// A `git grep` prefilter (POSIX ERE) plus the regex that names a match.
struct Pattern {
    language: Language,
    grep: &'static str,
    namer: fn(&str) -> Option<(EntryKind, String)>,
}

static RUST_ITEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*pub\s+(?:(?:async|unsafe|const|extern\s+\S+)\s+)*(fn|struct|enum|union|trait|type|mod|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)",
    )
    .expect("valid regex")
});
static RUST_IMPL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:unsafe\s+)?impl(?:<.*?>)?\s+([A-Za-z_][A-Za-z0-9_:]*)(?:<.*?>)?\s+for\s+([A-Za-z_][A-Za-z0-9_:]*)")
        .expect("valid regex")
});
static C_EXPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^EXPORT_SYMBOL(?:_GPL)?(?:_NS(?:_GPL)?)?\(\s*([A-Za-z_][A-Za-z0-9_]*)")
        .expect("valid regex")
});
static C_OPS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:static\s+)?(?:const\s+)?struct\s+([A-Za-z0-9_]+_(?:ops|operations))\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:\[\s*\])?\s*=")
        .expect("valid regex")
});

fn name_rust_item(line: &str) -> Option<(EntryKind, String)> {
    let caps = RUST_ITEM.captures(line)?;
    let kind = match &caps[1] {
        "fn" => EntryKind::Function,
        "trait" => EntryKind::Trait,
        "mod" => EntryKind::Module,
        "const" | "static" => EntryKind::Constant,
        _ => EntryKind::Type,
    };
    Some((kind, caps[2].to_string()))
}

fn name_rust_impl(line: &str) -> Option<(EntryKind, String)> {
    let caps = RUST_IMPL.captures(line)?;
    Some((
        EntryKind::TraitImpl,
        format!("{} for {}", &caps[1], &caps[2]),
    ))
}

fn name_c_export(line: &str) -> Option<(EntryKind, String)> {
    let caps = C_EXPORT.captures(line)?;
    Some((EntryKind::Export, caps[1].to_string()))
}

fn name_c_ops(line: &str) -> Option<(EntryKind, String)> {
    let caps = C_OPS.captures(line)?;
    Some((EntryKind::OpsTable, format!("{} {}", &caps[1], &caps[2])))
}

const PATTERNS: &[Pattern] = &[
    Pattern {
        language: Language::Rust,
        grep: r"^[[:space:]]*pub[[:space:]]+([a-z]+[[:space:]]+)*(fn|struct|enum|union|trait|type|mod|const|static)[[:space:]]",
        namer: name_rust_item,
    },
    Pattern {
        language: Language::Rust,
        grep: r"^[[:space:]]*(unsafe[[:space:]]+)?impl.*[[:space:]]for[[:space:]]",
        namer: name_rust_impl,
    },
    Pattern {
        language: Language::C,
        grep: r"^EXPORT_SYMBOL",
        namer: name_c_export,
    },
    Pattern {
        language: Language::C,
        grep: r"^(static[[:space:]]+)?(const[[:space:]]+)?struct[[:space:]]+[A-Za-z0-9_]+_(ops|operations)[[:space:]]",
        namer: name_c_ops,
    },
];

/// Extracts entry points of `files` at commit `sha`, sorted by path and
/// line. Languages without an adapter contribute nothing, and matches in a
/// file's trailing test module (beyond its production lines) are dropped.
pub async fn extract_entry_points(
    repo: &Path,
    sha: &str,
    files: &[SourceFile],
) -> Result<Vec<EntryPoint>> {
    let by_path: HashMap<&str, &SourceFile> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut entries = Vec::new();
    for pattern in PATTERNS {
        if !files.iter().any(|f| f.language == pattern.language) {
            continue;
        }
        for (path, line, text) in grep_lines(repo, sha, pattern.grep).await? {
            let Some(file) = by_path.get(path.as_str()) else {
                continue;
            };
            if file.language != pattern.language || (file.test_lines > 0 && line > file.lines) {
                continue;
            }
            if let Some((kind, name)) = (pattern.namer)(&text) {
                entries.push(EntryPoint {
                    kind,
                    name,
                    path,
                    line,
                });
            }
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    entries.dedup();
    Ok(entries)
}

/// Runs `git grep -E` over the whole tree at `sha`, returning
/// `(path, line, text)` for each match.
async fn grep_lines(repo: &Path, sha: &str, pattern: &str) -> Result<Vec<(String, usize, String)>> {
    let output = git_cmd::in_dir_async(repo)
        .args(["grep", "-n", "-I", "--null", "-E", "-e", pattern, sha])
        .output()
        .await
        .context("failed to run git grep")?;
    // git grep exits 1 when nothing matched.
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "git grep for entry points failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(parse_grep_records(&output.stdout, sha)
        .into_iter()
        .map(|r| (r.path, r.line, r.text))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::scope::test_repo;

    #[test]
    fn test_rust_namers() {
        assert_eq!(
            name_rust_item("pub async fn run(x: u8) {"),
            Some((EntryKind::Function, "run".to_string()))
        );
        assert_eq!(
            name_rust_item("    pub struct Worker {"),
            Some((EntryKind::Type, "Worker".to_string()))
        );
        assert_eq!(
            name_rust_item("pub trait AiProvider: Send {"),
            Some((EntryKind::Trait, "AiProvider".to_string()))
        );
        assert_eq!(
            name_rust_item("pub const LIMIT: usize = 3;"),
            Some((EntryKind::Constant, "LIMIT".to_string()))
        );
        assert_eq!(name_rust_item("pub(crate) fn hidden() {}"), None);
        assert_eq!(name_rust_item("fn private() {}"), None);
        assert_eq!(
            name_rust_impl("impl<S: Send> LlmTool<S> for GitGrepTool {"),
            Some((EntryKind::TraitImpl, "LlmTool for GitGrepTool".to_string()))
        );
        assert_eq!(name_rust_impl("impl Worker {"), None);
    }

    #[test]
    fn test_c_namers() {
        assert_eq!(
            name_c_export("EXPORT_SYMBOL_GPL(dev_queue_xmit);"),
            Some((EntryKind::Export, "dev_queue_xmit".to_string()))
        );
        assert_eq!(
            name_c_ops("static const struct file_operations foo_fops = {"),
            Some((EntryKind::OpsTable, "file_operations foo_fops".to_string()))
        );
        assert_eq!(name_c_ops("struct file_operations *fops;"), None);
        assert_eq!(
            name_c_ops("static const struct net_device_ops veth_netdev_ops = {"),
            Some((
                EntryKind::OpsTable,
                "net_device_ops veth_netdev_ops".to_string()
            ))
        );
    }

    #[tokio::test]
    async fn test_extract_entry_points_from_repo() {
        let dir = tempfile::tempdir().unwrap();
        let sha = test_repo::commit_files(
            dir.path(),
            &[
                ("src/lib.rs", "pub mod engine;\nfn private() {}\n"),
                (
                    "src/engine.rs",
                    "pub struct Engine;\nimpl Drop for Engine {\n    fn drop(&mut self) {}\n}\n#[cfg(test)]\nmod tests {\n    pub fn helper() {}\n}\n",
                ),
                ("drv/x.c", "int f(void) { return 0; }\nEXPORT_SYMBOL(f);\n"),
                ("docs/x.rs", "pub fn not_scoped() {}\n"),
            ],
        );
        let files = vec![
            SourceFile {
                path: "drv/x.c".into(),
                language: Language::C,
                lines: 2,
                test_lines: 0,
            },
            SourceFile {
                path: "src/engine.rs".into(),
                language: Language::Rust,
                lines: 4,
                test_lines: 4,
            },
            SourceFile {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                lines: 2,
                test_lines: 0,
            },
        ];
        let entries = extract_entry_points(dir.path(), &sha, &files)
            .await
            .unwrap();
        let summary: Vec<(EntryKind, &str, &str, usize)> = entries
            .iter()
            .map(|e| (e.kind, e.name.as_str(), e.path.as_str(), e.line))
            .collect();
        assert_eq!(
            summary,
            vec![
                (EntryKind::Export, "f", "drv/x.c", 2),
                (EntryKind::Type, "Engine", "src/engine.rs", 1),
                (EntryKind::TraitImpl, "Drop for Engine", "src/engine.rs", 2),
                (EntryKind::Module, "engine", "src/lib.rs", 1),
            ]
        );
    }
}
