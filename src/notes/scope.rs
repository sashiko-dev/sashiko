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

//! Deterministic scoping input: the source files of a project at a pinned
//! revision, with their language and line count. Everything is read from
//! git objects, never from the working tree, so a build is reproducible
//! from its revision alone.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::git_cmd;
use crate::notes::profile::{CompiledLanguages, Language};

/// One source file at the pinned revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub path: String,
    pub language: Language,
    /// Lines of production code: the whole file, except that a trailing
    /// inline test module (Rust `#[cfg(test)]`) is counted in `test_lines`
    /// instead.
    pub lines: usize,
    pub test_lines: usize,
}

/// Resolves `rev` to a full commit SHA. A revision starting with `-` is
/// rejected so it can never be read as an option.
pub async fn resolve_revision(repo: &Path, rev: &str) -> Result<String> {
    let rev = rev.trim();
    if rev.is_empty() || rev.starts_with('-') {
        bail!("invalid revision {rev:?}");
    }
    let output = git_cmd::in_dir_async(repo)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{rev}^{{commit}}"))
        .output()
        .await
        .context("failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("{rev:?} does not name a commit in {}", repo.display());
    }
    Ok(String::from_utf8(output.stdout)
        .context("git rev-parse printed non-UTF-8 output")?
        .trim()
        .to_string())
}

/// Lists the source files at commit `sha` that the profile's languages
/// classify, with their line counts, sorted by path.
pub async fn list_source_files(
    repo: &Path,
    sha: &str,
    languages: &CompiledLanguages,
) -> Result<Vec<SourceFile>> {
    let paths = list_tree_paths(repo, sha).await?;
    let line_counts = count_lines(repo, sha).await?;
    let mut files: Vec<SourceFile> = paths
        .into_iter()
        .filter_map(|path| {
            let language = languages.classify(&path)?;
            let lines = line_counts.get(&path).copied().unwrap_or(0);
            Some(SourceFile {
                path,
                language,
                lines,
                test_lines: 0,
            })
        })
        .collect();
    if files.iter().any(|f| f.language == Language::Rust) {
        let boundaries = rust_test_boundaries(repo, sha).await?;
        for file in files.iter_mut().filter(|f| f.language == Language::Rust) {
            if let Some(first_test_line) = boundaries.get(&file.path) {
                let production = first_test_line.saturating_sub(1).min(file.lines);
                file.test_lines = file.lines - production;
                file.lines = production;
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

async fn list_tree_paths(repo: &Path, sha: &str) -> Result<Vec<String>> {
    let output = git_cmd::in_dir_async(repo)
        .args(["ls-tree", "-r", "--name-only", "-z", "--full-tree", sha])
        .output()
        .await
        .context("failed to run git ls-tree")?;
    if !output.status.success() {
        bail!(
            "git ls-tree {sha} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    output
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8(p.to_vec()).context("repository contains a non-UTF-8 path"))
        .collect()
}

/// Counts lines of every text file at `sha` in one `git grep` pass. Binary
/// files and empty files are absent from the result.
async fn count_lines(repo: &Path, sha: &str) -> Result<HashMap<String, usize>> {
    let output = git_cmd::in_dir_async(repo)
        .args(["grep", "--count", "-I", "--null", "-e", "", sha])
        .output()
        .await
        .context("failed to run git grep")?;
    // git grep exits 1 when nothing matched (a tree without text files).
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "git grep --count at {sha} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_count_output(&output.stdout, sha)
}

/// Finds, per Rust file at `sha`, the first line of a top-level inline test
/// module (`#[cfg(test)]` or `#[cfg(all(test, ...))]` at column 0). By
/// convention such modules close the file, so everything from that line on
/// is test code.
async fn rust_test_boundaries(repo: &Path, sha: &str) -> Result<HashMap<String, usize>> {
    let output = git_cmd::in_dir_async(repo)
        .args([
            "grep",
            "-n",
            "-I",
            "--null",
            "-E",
            "-e",
            r"^#\[cfg\((all\()?test[,)]",
            sha,
            "--",
            "*.rs",
        ])
        .output()
        .await
        .context("failed to run git grep")?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "git grep for Rust test modules failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let prefix = format!("{sha}:");
    let mut boundaries: HashMap<String, usize> = HashMap::new();
    for record in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = record.splitn(3, '\0');
        let (Some(name), Some(line)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(line) = line.parse::<usize>() else {
            continue;
        };
        let path = name.strip_prefix(&prefix).unwrap_or(name).to_string();
        boundaries
            .entry(path)
            .and_modify(|first| *first = (*first).min(line))
            .or_insert(line);
    }
    Ok(boundaries)
}

/// Parses `git grep --count --null` output: `<sha>:<path>\0<count>\n` per
/// file.
fn parse_count_output(stdout: &[u8], sha: &str) -> Result<HashMap<String, usize>> {
    let prefix = format!("{sha}:");
    let mut counts = HashMap::new();
    let mut rest = stdout;
    while !rest.is_empty() {
        let Some(nul) = rest.iter().position(|b| *b == 0) else {
            bail!("malformed git grep output: missing NUL separator");
        };
        let name = std::str::from_utf8(&rest[..nul]).context("non-UTF-8 path in git grep")?;
        let after = &rest[nul + 1..];
        let end = after
            .iter()
            .position(|b| *b == b'\n')
            .unwrap_or(after.len());
        let count: usize = std::str::from_utf8(&after[..end])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .with_context(|| format!("malformed line count for {name}"))?;
        let path = name.strip_prefix(&prefix).unwrap_or(name);
        counts.insert(path.to_string(), count);
        rest = after.get(end + 1..).unwrap_or_default();
    }
    Ok(counts)
}

#[cfg(test)]
pub(crate) mod test_repo {
    use std::path::Path;

    /// Creates a git repository at `dir` with `files` committed, and returns
    /// the commit SHA.
    pub fn commit_files(dir: &Path, files: &[(&str, &str)]) -> String {
        let git = |args: &[&str]| {
            let output = crate::git_cmd::in_dir(dir)
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).expect("utf-8")
        };
        if !dir.join(".git").exists() {
            git(&["init", "-q"]);
        }
        for (path, content) in files {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, content).expect("write");
        }
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "test commit"]);
        git(&["rev-parse", "HEAD"]).trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::profile::NotesProfile;

    fn languages() -> CompiledLanguages {
        NotesProfile::from_toml_str(
            "[units]\nsource = \"maintainers\"\n[languages]\nskip = [\"src/gen/\"]\n[languages.globs]\nrust = [\"src/**.rs\"]\nc = [\"**.c\", \"**.h\"]\n",
        )
        .unwrap()
        .compiled_languages()
    }

    #[tokio::test]
    async fn test_list_source_files_at_revision() {
        let dir = tempfile::tempdir().unwrap();
        let first = test_repo::commit_files(
            dir.path(),
            &[
                ("src/main.rs", "fn main() {}\n\n// end\n"),
                (
                    "src/lib.rs",
                    "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n",
                ),
                (
                    "src/db.rs",
                    "pub fn g() {}\n\n#[cfg(all(test, feature = \"server\"))]\nmod tests {}\n",
                ),
                ("src/gen/table.rs", "// generated\n"),
                ("lib/util.c", "int x;\n"),
                ("lib/empty.h", ""),
                ("README.md", "# readme\n"),
            ],
        );
        // A later commit must not leak into a listing pinned to `first`.
        test_repo::commit_files(dir.path(), &[("src/later.rs", "fn later() {}\n")]);

        let sha = resolve_revision(dir.path(), &first).await.unwrap();
        assert_eq!(sha, first);
        let files = list_source_files(dir.path(), &sha, &languages())
            .await
            .unwrap();
        let summary: Vec<(&str, Language, usize, usize)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.language, f.lines, f.test_lines))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("lib/empty.h", Language::C, 0, 0),
                ("lib/util.c", Language::C, 1, 0),
                ("src/db.rs", Language::Rust, 2, 2),
                ("src/lib.rs", Language::Rust, 1, 4),
                ("src/main.rs", Language::Rust, 3, 0),
            ]
        );
    }

    #[tokio::test]
    async fn test_resolve_revision_rejects_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        test_repo::commit_files(dir.path(), &[("a.c", "int a;\n")]);
        assert!(resolve_revision(dir.path(), "--all").await.is_err());
        assert!(resolve_revision(dir.path(), "").await.is_err());
        assert!(
            resolve_revision(dir.path(), "no-such-branch")
                .await
                .is_err()
        );
        assert_eq!(
            resolve_revision(dir.path(), "HEAD").await.unwrap().len(),
            40
        );
    }

    #[test]
    fn test_parse_count_output() {
        let out = b"abc:src/a.rs\x0012\nabc:dir/with:colon.c\x003\n";
        let counts = parse_count_output(out, "abc").unwrap();
        assert_eq!(counts["src/a.rs"], 12);
        assert_eq!(counts["dir/with:colon.c"], 3);
        assert!(parse_count_output(b"abc:x\x00nan\n", "abc").is_err());
        assert!(parse_count_output(b"abc:x 12\n", "abc").is_err());
    }
}
