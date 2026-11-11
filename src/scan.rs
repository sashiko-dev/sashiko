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

//! Active codebase scanner target resolution and Tree-sitter structural chunking.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use tree_sitter::{Node, Parser};

/// Files up to this line count are audited as a single chunk without splitting.
pub const SINGLE_CHUNK_MAX_LINES: usize = 500;

/// Soft target line count when grouping consecutive functions into a chunk.
pub const TARGET_CHUNK_LINES: usize = 400;

/// Hard upper bound when extending a chunk to keep mutually-calling functions together.
pub const MAX_CHUNK_LINES: usize = 650;

/// Maximum number of functions rendered in the per-file outline before summarizing.
const MAX_INDEXED_FUNCTIONS: usize = 140;

/// Line overlap when splitting a single giant function that exceeds `MAX_CHUNK_LINES`.
pub const GIANT_FUNCTION_OVERLAP_LINES: usize = 50;

/// Maximum character budget for pre-fetched external header definitions per chunk.
pub const MAX_HEADER_PREFETCH_CHARS: usize = 22_000;

/// Maximum character budget for deterministic out-of-chunk intra-file context per chunk.
pub const MAX_INTRA_FILE_CONTEXT_CHARS: usize = 18_000;

/// Maximum file byte size eligible for a single-turn whole-file invariant synthesis pass.
const MAX_WHOLE_FILE_SYNTHESIS_BYTES: usize = 260_000;

/// Maximum lines of file-scope preamble (structs, macros, statics) prepended to later chunks.
const MAX_PREAMBLE_LINES: usize = 80;

/// Default directories skipped during recursive directory expansion unless explicitly targeted.
const DEFAULT_IGNORED_PREFIXES: &[&str] = &[
    "Documentation/",
    "samples/",
    "scripts/",
    "tools/testing/selftests/",
    "LICENSES/",
];

/// Metadata for a single function extracted from a source file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FunctionSpan {
    pub name: String,
    pub signature: String,
    /// 1-based inclusive start line.
    pub start_line: usize,
    /// 1-based inclusive end line.
    pub end_line: usize,
    /// Intra-file or external functions directly called by this function.
    pub called_functions: Vec<String>,
    /// Functions defined in the same file called or referenced by this function.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intra_file_callees: Vec<String>,
    /// Functions or vtable initializers in the same file that call or reference this function.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intra_file_callers: Vec<String>,
    /// High-signal kernel operations detected inside this function body.
    pub risk_signals: Vec<String>,
}

/// A bounded, structure-aware slice of a source file for active scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanChunk {
    pub file_path: String,
    /// 1-based index of this chunk (`1..=total_chunks`).
    pub chunk_index: usize,
    pub total_chunks: usize,
    /// 1-based inclusive start line of the primary chunk span.
    pub start_line: usize,
    /// 1-based inclusive end line of the primary chunk span.
    pub end_line: usize,
    /// Numbered source lines (`"   123: ..."`), including file-scope preamble when applicable.
    pub numbered_source: String,
    /// Compact summary of all functions in the file so the model knows what else exists.
    pub file_function_index: String,
    /// Deterministic intra-file context (out-of-chunk file-local types/macros, 1-hop static
    /// callees, and intra-file caller call-sites).
    pub intra_file_context: String,
    /// Optional synthesized whole-file locking, nullable-field, and lifecycle invariants.
    pub file_invariants: String,
    /// Normative architecture & contract specification from `ContractCartographer` (`Agent 1`).
    pub contract_notes: String,
    /// Pre-fetched 1-hop external call-sites and ops-table registrations for exported functions.
    pub external_callers: String,
    /// Functions overlapping this chunk.
    pub functions: Vec<FunctionSpan>,
    /// Pre-fetched struct/enum/macro definitions from headers in the same subsystem.
    pub prefetched_headers: String,
}

impl ScanChunk {
    /// Stable identifier for this chunk (`"<file_path>:<start_line>-<end_line>"`).
    pub fn chunk_id(&self) -> String {
        format!("{}:{}-{}", self.file_path, self.start_line, self.end_line)
    }

    /// Reconstructs the unnumbered source lines of this chunk (excluding synthetic
    /// preamble separator banners) for deterministic identifier extraction.
    pub fn raw_source_lines(&self) -> String {
        let mut out = String::with_capacity(self.numbered_source.len());
        for line in self.numbered_source.lines() {
            if line.starts_with("// --- File-scope preamble") || line.starts_with("// ---< lines ")
            {
                continue;
            }
            let trimmed = line.trim_start();
            let digits_len = trimmed.bytes().take_while(u8::is_ascii_digit).count();
            let raw = if digits_len > 0 && trimmed[digits_len..].starts_with(": ") {
                &trimmed[digits_len + 2..]
            } else {
                line
            };
            out.push_str(raw);
            out.push('\n');
        }
        out
    }
}

/// Sanitizes a user-supplied target path into a clean repository-relative path string.
pub fn sanitize_scan_path(path: &Path) -> Result<String> {
    let raw = path.to_string_lossy();
    let trimmed = raw.trim();
    ensure!(!trimmed.is_empty(), "Scan target path cannot be empty");
    ensure!(
        !trimmed.starts_with('-'),
        "Scan target path '{}' must not start with '-'",
        trimmed
    );
    ensure!(
        !trimmed.chars().any(char::is_control),
        "Scan target path '{}' must not contain control characters",
        trimmed
    );
    ensure!(
        !path.is_absolute(),
        "Scan target path '{}' must be relative to the repository root",
        trimmed
    );

    let mut parts = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(s) => {
                let part = s.to_string_lossy();
                if !part.is_empty() {
                    parts.push(part.into_owned());
                }
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "Scan target path '{}' must not contain '..' or root components",
                    trimmed
                );
            }
        }
    }

    if parts.is_empty() {
        Ok(".".to_string())
    } else {
        Ok(parts.join("/"))
    }
}

/// Resolves `--ref` (or the default mainline/HEAD revision) to a full commit SHA.
pub async fn resolve_scan_revision(repo_path: &Path, rev: Option<&str>) -> Result<String> {
    if let Some(user_rev) = rev.map(str::trim).filter(|s| !s.is_empty()) {
        ensure!(
            !user_rev.starts_with('-'),
            "Revision '{}' must not start with '-'",
            user_rev
        );
        let output = crate::git_cmd::in_dir_async(repo_path)
            .args([
                "-c",
                "safe.bareRepository=all",
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{user_rev}^{{commit}}"),
            ])
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("Failed to resolve revision '{user_rev}'"))?;
        ensure!(
            output.status.success(),
            "Cannot resolve revision '{}' in {}: {}",
            user_rev,
            repo_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    if let Some(sha) = crate::workflows::linux_bug::resolve_mainline_sha(
        repo_path,
        crate::project::ProjectId::Linux,
    )
    .await
    {
        return Ok(sha);
    }

    for candidate in ["origin/master", "origin/main", "master", "main", "HEAD"] {
        let output = crate::git_cmd::in_dir_async(repo_path)
            .args([
                "-c",
                "safe.bareRepository=all",
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{candidate}^{{commit}}"),
            ])
            .kill_on_drop(true)
            .output()
            .await;
        if let Ok(out) = output
            && out.status.success()
        {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !sha.is_empty() {
                return Ok(sha);
            }
        }
    }

    bail!(
        "Failed to resolve a default Git commit SHA in {}",
        repo_path.display()
    )
}

/// Expands `<paths...>` at `target_sha` into a sorted, filtered list of scannable source files.
pub async fn resolve_scan_targets(
    repo_path: &Path,
    target_sha: &str,
    raw_paths: &[PathBuf],
    ignore_patterns: &[String],
    max_files: usize,
) -> Result<Vec<String>> {
    ensure!(
        !raw_paths.is_empty(),
        "At least one file or directory path must be provided to scan"
    );

    let mut sanitized_paths = Vec::with_capacity(raw_paths.len());
    for p in raw_paths {
        sanitized_paths.push(sanitize_scan_path(p)?);
    }

    // Verify every explicit non-root target path exists in the tree at target_sha so a
    // misspelled path alongside a valid path fails fast instead of being silently ignored.
    for sp in &sanitized_paths {
        if sp == "." {
            continue;
        }
        let spec = format!("{target_sha}:{sp}");
        let exists = crate::git_cmd::in_dir_async(repo_path)
            .args([
                "-c",
                "safe.bareRepository=all",
                "cat-file",
                "-e",
                "--end-of-options",
                &spec,
            ])
            .kill_on_drop(true)
            .status()
            .await
            .with_context(|| format!("Failed to check scan target path '{sp}'"))?;
        ensure!(
            exists.success(),
            "Scan target path '{}' does not exist at commit {}",
            sp,
            &target_sha[..target_sha.len().min(12)]
        );
    }

    let scan_whole_tree = sanitized_paths.iter().any(|sp| sp == ".");
    let mut cmd = crate::git_cmd::in_dir_async(repo_path);
    cmd.args([
        "-c",
        "safe.bareRepository=all",
        "ls-tree",
        "-r",
        "-z",
        "--name-only",
        "--end-of-options",
        target_sha,
    ]);
    if !scan_whole_tree {
        cmd.arg("--");
        for sp in &sanitized_paths {
            cmd.arg(sp);
        }
    }
    cmd.kill_on_drop(true);

    let output = cmd
        .output()
        .await
        .context("Failed to run git ls-tree for scan target resolution")?;
    ensure!(
        output.status.success(),
        "git ls-tree failed at {}: {}",
        target_sha,
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut matched = BTreeSet::new();
    for entry in stdout.split('\0') {
        let file = entry.trim();
        if file.is_empty() || !is_scannable_source_file(file) {
            continue;
        }
        if should_ignore_scan_file(file, &sanitized_paths, ignore_patterns) {
            continue;
        }
        matched.insert(file.to_string());
    }

    ensure!(
        !matched.is_empty(),
        "No scannable .c, .h, or .rs source files found under {:?} at commit {}",
        sanitized_paths,
        &target_sha[..target_sha.len().min(12)]
    );

    if max_files > 0 && matched.len() > max_files {
        bail!(
            "Matched {} source files under {:?}, which exceeds --max-files {}. \
             Narrow the target path(s) or pass --max-files {} (or --max-files 0 for unlimited).",
            matched.len(),
            sanitized_paths,
            max_files,
            matched.len()
        );
    }

    Ok(matched.into_iter().collect())
}

fn is_scannable_source_file(file: &str) -> bool {
    file.ends_with(".c") || file.ends_with(".h") || file.ends_with(".rs")
}

fn should_ignore_scan_file(
    file: &str,
    explicit_targets: &[String],
    ignore_patterns: &[String],
) -> bool {
    let exact_file_requested = explicit_targets.iter().any(|t| t == file);
    if !exact_file_requested && file.ends_with(".mod.c") {
        return true;
    }

    let explicitly_requested = exact_file_requested
        || explicit_targets
            .iter()
            .any(|t| t != "." && file.starts_with(&format!("{t}/")) && is_noisy_prefix(t));

    if !explicitly_requested && is_noisy_prefix(file) {
        return true;
    }

    for pat in ignore_patterns {
        let p = pat.trim();
        if p.is_empty() || exact_file_requested {
            continue;
        }
        if pattern_matches_path(p, file) {
            return true;
        }
    }

    false
}

fn is_noisy_prefix(path: &str) -> bool {
    DEFAULT_IGNORED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix) || path == prefix.trim_end_matches('/'))
}

fn pattern_matches_path(pattern: &str, file: &str) -> bool {
    if let Some(prefix) = pattern
        .strip_suffix("/*")
        .or_else(|| pattern.strip_suffix('/'))
    {
        file.starts_with(&format!("{prefix}/")) || file == prefix
    } else if let Some(ext) = pattern.strip_prefix("*.") {
        file.ends_with(&format!(".{ext}"))
    } else {
        file == pattern || file.starts_with(&format!("{pattern}/")) || file.contains(pattern)
    }
}

/// Reads a file blob at `target_sha` and constructs its `ScanChunk`s along with header prefetch.
pub async fn build_scan_chunks_for_file(
    repo_path: &Path,
    target_sha: &str,
    file_path: &str,
) -> Result<Vec<ScanChunk>> {
    build_scan_chunks_for_file_with_limit(repo_path, target_sha, file_path, None).await
}

struct LoadedFileChunks {
    file_path: String,
    raw_source: String,
    whole_file_external_callers: String,
    chunks: Vec<ScanChunk>,
}

/// Reads a file blob at `target_sha`, partitions it into `ScanChunk`s (truncating to
/// `max_chunks` if specified), and prefetches external header definitions in a single
/// batched pass over the retained chunks.
pub async fn build_scan_chunks_for_file_with_limit(
    repo_path: &Path,
    target_sha: &str,
    file_path: &str,
    max_chunks: Option<usize>,
) -> Result<Vec<ScanChunk>> {
    Ok(
        load_file_chunks_with_limit(repo_path, target_sha, file_path, max_chunks)
            .await?
            .chunks,
    )
}

async fn load_file_chunks_with_limit(
    repo_path: &Path,
    target_sha: &str,
    file_path: &str,
    max_chunks: Option<usize>,
) -> Result<LoadedFileChunks> {
    if max_chunks == Some(0) {
        return Ok(LoadedFileChunks {
            file_path: file_path.to_string(),
            raw_source: String::new(),
            whole_file_external_callers: String::new(),
            chunks: Vec::new(),
        });
    }
    let spec = format!("{target_sha}:{file_path}");
    let output = crate::git_cmd::in_dir_async(repo_path)
        .args([
            "-c",
            "safe.bareRepository=all",
            "show",
            "--end-of-options",
            &spec,
        ])
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("Failed to read {spec}"))?;
    ensure!(
        output.status.success(),
        "git show {} failed: {}",
        spec,
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let source = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut chunks = chunk_source_file(file_path, &source);
    if let Some(limit) = max_chunks
        && chunks.len() > limit
    {
        chunks.truncate(limit);
    }

    let ranges: Vec<(usize, usize)> = chunks.iter().map(|c| (c.start_line, c.end_line)).collect();
    if let Ok(prefetched_blocks) =
        crate::worker::prefetch::prefetch_external_definitions_for_chunks(
            repo_path,
            target_sha,
            file_path,
            &ranges,
            MAX_HEADER_PREFETCH_CHARS,
        )
        .await
    {
        for (chunk, prefetched) in chunks.iter_mut().zip(prefetched_blocks) {
            chunk.prefetched_headers = prefetched;
        }
    }

    let mut whole_file_external_callers = String::new();
    if !file_path.ends_with(".h") && !chunks.is_empty() {
        let per_chunk_exported_fns: Vec<Vec<String>> = chunks
            .iter()
            .map(|c| {
                c.functions
                    .iter()
                    .filter(|f| !is_file_static_signature(file_path, &f.signature))
                    .map(|f| f.name.clone())
                    .collect()
            })
            .collect();
        if let Ok((whole_callers, chunk_callers)) =
            crate::worker::prefetch::prefetch_external_callers_for_chunks(
                repo_path,
                target_sha,
                file_path,
                &per_chunk_exported_fns,
                14_000,
                7_000,
            )
            .await
        {
            whole_file_external_callers = whole_callers;
            for (chunk, callers) in chunks.iter_mut().zip(chunk_callers) {
                chunk.external_callers = callers;
            }
        }
    }

    Ok(LoadedFileChunks {
        file_path: file_path.to_string(),
        raw_source: source,
        whole_file_external_callers,
        chunks,
    })
}

fn is_file_static_signature(file_path: &str, signature: &str) -> bool {
    let trimmed = signature.trim_start();
    if file_path.ends_with(".rs") {
        !trimmed.starts_with("pub ") && !trimmed.contains(" pub fn ")
    } else {
        trimmed.starts_with("static ")
            || trimmed.starts_with("static\t")
            || trimmed.starts_with("static\n")
            || trimmed.contains(" static ")
    }
}

#[derive(Debug, Clone)]
struct FileLocalDef {
    names: Vec<String>,
    referenced_idents: BTreeSet<String>,
    start_line: usize,
    end_line: usize,
}

/// Deterministically partitions `source` into locality-aware `ScanChunk`s.
pub fn chunk_source_file(file_path: &str, source: &str) -> Vec<ScanChunk> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let total_lines = lines.len();
    let local_defs = if file_path.ends_with(".rs") {
        Vec::new()
    } else {
        extract_c_file_local_defs(source)
    };
    let mut functions = extract_functions(file_path, source);
    populate_intra_file_graph(&mut functions, &local_defs, &lines);
    let file_function_index = format_function_index(file_path, total_lines, &functions);

    if total_lines <= SINGLE_CHUNK_MAX_LINES {
        return vec![ScanChunk {
            file_path: file_path.to_string(),
            chunk_index: 1,
            total_chunks: 1,
            start_line: 1,
            end_line: total_lines,
            numbered_source: format_numbered_lines(&lines, 1, total_lines, None),
            file_function_index,
            intra_file_context: String::new(),
            file_invariants: String::new(),
            contract_notes: String::new(),
            external_callers: String::new(),
            functions,
            prefetched_headers: String::new(),
        }];
    }

    let raw_windows = if functions.is_empty() {
        build_line_windows(total_lines)
    } else {
        build_function_windows(total_lines, &functions)
    };

    let preamble_end = functions
        .first()
        .map(|f| f.start_line.saturating_sub(1).min(MAX_PREAMBLE_LINES))
        .unwrap_or(0);

    let total_chunks = raw_windows.len();
    raw_windows
        .into_iter()
        .enumerate()
        .map(|(idx, (start_line, end_line))| {
            let chunk_fns: Vec<FunctionSpan> = functions
                .iter()
                .filter(|f| f.end_line >= start_line && f.start_line <= end_line)
                .cloned()
                .collect();
            let preamble = if start_line > 1 && preamble_end >= 1 && preamble_end < start_line {
                Some((1, preamble_end))
            } else {
                None
            };
            let intra_file_context = if total_chunks >= 2 {
                build_chunk_intra_file_context(
                    &lines,
                    start_line,
                    end_line,
                    preamble.map(|(_, pe)| pe).unwrap_or(0),
                    &chunk_fns,
                    &functions,
                    &local_defs,
                    MAX_INTRA_FILE_CONTEXT_CHARS,
                )
            } else {
                String::new()
            };
            ScanChunk {
                file_path: file_path.to_string(),
                chunk_index: idx + 1,
                total_chunks,
                start_line,
                end_line,
                numbered_source: format_numbered_lines(&lines, start_line, end_line, preamble),
                file_function_index: file_function_index.clone(),
                intra_file_context,
                file_invariants: String::new(),
                contract_notes: String::new(),
                external_callers: String::new(),
                functions: chunk_fns,
                prefetched_headers: String::new(),
            }
        })
        .collect()
}

fn build_line_windows(total_lines: usize) -> Vec<(usize, usize)> {
    let mut windows = Vec::new();
    let mut start = 1;
    while start <= total_lines {
        let end = (start + TARGET_CHUNK_LINES - 1).min(total_lines);
        windows.push((start, end));
        if end == total_lines {
            break;
        }
        start = end.saturating_sub(GIANT_FUNCTION_OVERLAP_LINES) + 1;
    }
    windows
}

fn build_function_windows(total_lines: usize, functions: &[FunctionSpan]) -> Vec<(usize, usize)> {
    if total_lines == 0 {
        return Vec::new();
    }
    let mut windows: Vec<(usize, usize)> = Vec::new();
    let mut idx = 0;

    while idx < functions.len() {
        let first_fn = &functions[idx];
        let chunk_start = if idx == 0 {
            1
        } else {
            let prev_end = windows
                .last()
                .map(|&(_, end)| end)
                .unwrap_or(functions[idx - 1].end_line);
            (prev_end + 1)
                .min(first_fn.start_line.max(1))
                .min(total_lines)
        };
        let first_end = first_fn.end_line.max(chunk_start).min(total_lines);

        // If a single function exceeds MAX_CHUNK_LINES, split it into overlapping windows.
        if first_end.saturating_sub(chunk_start) + 1 > MAX_CHUNK_LINES {
            let mut sub_start = chunk_start;
            while sub_start <= first_end {
                let sub_end = (sub_start + TARGET_CHUNK_LINES - 1).min(first_end);
                windows.push((sub_start, sub_end));
                if sub_end == first_end {
                    break;
                }
                sub_start = sub_end.saturating_sub(GIANT_FUNCTION_OVERLAP_LINES) + 1;
            }
            idx += 1;
            continue;
        }

        let mut chunk_end = first_end;
        let mut chunk_names: BTreeSet<&str> = BTreeSet::new();
        let mut chunk_calls: BTreeSet<&str> = BTreeSet::new();
        chunk_names.insert(first_fn.name.as_str());
        for c in &first_fn.called_functions {
            chunk_calls.insert(c.as_str());
        }
        idx += 1;

        while idx < functions.len() {
            let candidate = &functions[idx];
            let candidate_end = candidate.end_line.max(chunk_end).min(total_lines);
            let candidate_span = candidate_end.saturating_sub(chunk_start) + 1;
            let has_call_affinity = chunk_calls.contains(candidate.name.as_str())
                || candidate
                    .called_functions
                    .iter()
                    .any(|c| chunk_names.contains(c.as_str()));

            let limit = if has_call_affinity {
                MAX_CHUNK_LINES
            } else {
                TARGET_CHUNK_LINES
            };

            if candidate_span <= limit {
                chunk_end = candidate_end;
                chunk_names.insert(candidate.name.as_str());
                for c in &candidate.called_functions {
                    chunk_calls.insert(c.as_str());
                }
                idx += 1;
            } else {
                break;
            }
        }

        if idx == functions.len() && total_lines.saturating_sub(chunk_start) < MAX_CHUNK_LINES {
            chunk_end = total_lines;
        }
        windows.push((chunk_start, chunk_end));
    }

    if let Some(&(_, last_end)) = windows.last()
        && last_end < total_lines
    {
        let mut trail_start = last_end + 1;
        while trail_start <= total_lines {
            let trail_end = (trail_start + TARGET_CHUNK_LINES - 1).min(total_lines);
            windows.push((trail_start, trail_end));
            if trail_end == total_lines {
                break;
            }
            trail_start = trail_end.saturating_sub(GIANT_FUNCTION_OVERLAP_LINES) + 1;
        }
    }

    windows
}

fn format_numbered_lines(
    lines: &[&str],
    start_line: usize,
    end_line: usize,
    preamble: Option<(usize, usize)>,
) -> String {
    let mut out = String::new();
    if let Some((p_start, p_end)) = preamble {
        out.push_str("// --- File-scope preamble (declarations & types) ---\n");
        for line_no in p_start..=p_end.min(lines.len()) {
            out.push_str(&format!("{:>6}: {}\n", line_no, lines[line_no - 1]));
        }
        out.push_str(&format!(
            "// ---< lines {}..{} elided; see chunk lines {}..{} below >---\n",
            p_end + 1,
            start_line.saturating_sub(1),
            start_line,
            end_line
        ));
    }
    let clamped_start = start_line.max(1);
    let clamped_end = end_line.min(lines.len());
    for line_no in clamped_start..=clamped_end {
        out.push_str(&format!("{:>6}: {}\n", line_no, lines[line_no - 1]));
    }
    out
}

fn format_function_index(
    file_path: &str,
    total_lines: usize,
    functions: &[FunctionSpan],
) -> String {
    if functions.is_empty() {
        return format!(
            "{file_path} ({total_lines} lines): no top-level function definitions detected."
        );
    }
    let mut out = format!(
        "{file_path} ({total_lines} lines, {} functions):\n",
        functions.len()
    );
    let shown: Vec<&FunctionSpan> = if functions.len() <= MAX_INDEXED_FUNCTIONS {
        functions.iter().collect()
    } else {
        let mut selected: Vec<&FunctionSpan> = functions
            .iter()
            .filter(|f| !f.risk_signals.is_empty())
            .take(MAX_INDEXED_FUNCTIONS)
            .collect();
        if selected.len() < MAX_INDEXED_FUNCTIONS {
            for f in functions {
                if selected.len() >= MAX_INDEXED_FUNCTIONS {
                    break;
                }
                if f.risk_signals.is_empty() {
                    selected.push(f);
                }
            }
            selected.sort_by_key(|f| f.start_line);
        }
        selected
    };
    for f in &shown {
        let signals = if f.risk_signals.is_empty() {
            String::new()
        } else {
            format!(" [signals: {}]", f.risk_signals.join(", "))
        };
        out.push_str(&format!(
            "- {}() [lines {}-{}]{}\n",
            f.name, f.start_line, f.end_line, signals
        ));
    }
    if functions.len() > shown.len() {
        out.push_str(&format!(
            "... and {} more functions in {file_path} (use git_grep / git_read_files to inspect)\n",
            functions.len() - shown.len()
        ));
    }
    out
}

/// Extracts top-level function spans from C, header, or Rust source code.
pub fn extract_functions(file_path: &str, source: &str) -> Vec<FunctionSpan> {
    if file_path.ends_with(".rs") {
        extract_rust_functions(source)
    } else {
        extract_c_functions(source)
    }
}

fn extract_c_functions(source: &str) -> Vec<FunctionSpan> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let bytes = source.as_bytes();
    let lines: Vec<&str> = source.lines().collect();
    let mut spans = Vec::new();
    collect_c_function_nodes(tree.root_node(), bytes, &lines, &mut spans);
    spans.sort_by_key(|f| f.start_line);
    spans
}

fn collect_c_function_nodes(
    node: Node<'_>,
    source: &[u8],
    lines: &[&str],
    out: &mut Vec<FunctionSpan>,
) {
    if node.kind() == "function_definition" {
        if let Some(span) = build_c_function_span(node, source, lines) {
            out.push(span);
        }
        return;
    }

    // Descend into translation_unit and preprocessor conditional blocks (#ifdef / #if / #else)
    // so functions guarded by CONFIG_* macros are still indexed.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition"
            | "preproc_if"
            | "preproc_ifdef"
            | "preproc_elif"
            | "preproc_else"
            | "declaration_list"
            | "linkage_specification" => {
                collect_c_function_nodes(child, source, lines, out);
            }
            _ => {}
        }
    }
}

fn build_c_function_span(node: Node<'_>, source: &[u8], lines: &[&str]) -> Option<FunctionSpan> {
    let name = extract_c_declarator_name(node.child_by_field_name("declarator")?, source)?;
    let start_line = node.start_position().row + 1;
    let end_line = (node.end_position().row + 1).min(lines.len().max(1));

    let body = node.child_by_field_name("body")?;
    let sig_end_row = body.start_position().row;
    let sig_lines: Vec<&str> = lines
        .get(node.start_position().row..=sig_end_row.min(lines.len().saturating_sub(1)))
        .unwrap_or(&[])
        .iter()
        .map(|l| l.trim())
        .collect();
    let raw_sig = sig_lines.join(" ");
    let signature = raw_sig
        .split('{')
        .next()
        .unwrap_or(&raw_sig)
        .trim()
        .to_string();

    let mut calls = BTreeSet::new();
    collect_call_identifiers(body, source, &mut calls);
    calls.remove(name.as_str());

    let body_text = node.utf8_text(source).unwrap_or("");
    let risk_signals = detect_risk_signals(body_text);

    Some(FunctionSpan {
        name,
        signature,
        start_line,
        end_line,
        called_functions: calls.into_iter().collect(),
        intra_file_callees: Vec::new(),
        intra_file_callers: Vec::new(),
        risk_signals,
    })
}

fn extract_c_file_local_defs(source: &str) -> Vec<FileLocalDef> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let bytes = source.as_bytes();
    let total_lines = source.lines().count().max(1);
    let mut out = Vec::new();
    collect_c_file_local_defs(tree.root_node(), bytes, total_lines, &mut out);
    out.sort_by_key(|d| d.start_line);
    out
}

fn collect_c_file_local_defs(
    node: Node<'_>,
    source: &[u8],
    total_lines: usize,
    out: &mut Vec<FileLocalDef>,
) {
    match node.kind() {
        "function_definition" => {}
        "preproc_if"
        | "preproc_ifdef"
        | "preproc_elif"
        | "preproc_else"
        | "translation_unit"
        | "declaration_list"
        | "linkage_specification" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_c_file_local_defs(child, source, total_lines, out);
            }
        }
        "struct_specifier" | "union_specifier" | "enum_specifier" => {
            if node.child_by_field_name("body").is_some()
                && let Some(def) = build_file_local_def(node, source, total_lines)
            {
                out.push(def);
            }
        }
        "type_definition" | "preproc_def" | "preproc_function_def" | "declaration" => {
            if let Some(def) = build_file_local_def(node, source, total_lines) {
                out.push(def);
            }
        }
        _ => {}
    }
}

fn build_file_local_def(node: Node<'_>, source: &[u8], total_lines: usize) -> Option<FileLocalDef> {
    let start_line = node.start_position().row + 1;
    let end_line = (node.end_position().row + 1).min(total_lines);
    let mut names = BTreeSet::new();
    collect_defined_names_in_node(node, source, &mut names);
    if names.is_empty() {
        return None;
    }
    let mut referenced_idents = BTreeSet::new();
    collect_all_identifiers(node, source, &mut referenced_idents);
    for n in &names {
        referenced_idents.remove(n);
    }
    Some(FileLocalDef {
        names: names.into_iter().collect(),
        referenced_idents,
        start_line,
        end_line,
    })
}

fn collect_defined_names_in_node(node: Node<'_>, source: &[u8], out: &mut BTreeSet<String>) {
    match node.kind() {
        "preproc_def" | "preproc_function_def" => {
            if let Some(n) = node.child_by_field_name("name")
                && let Ok(txt) = n.utf8_text(source)
            {
                let trimmed = txt.trim();
                if trimmed.len() >= 2 {
                    out.insert(trimmed.to_string());
                }
            }
        }
        "struct_specifier" | "union_specifier" | "enum_specifier" => {
            if node.child_by_field_name("body").is_some()
                && let Some(n) = node.child_by_field_name("name")
                && let Ok(txt) = n.utf8_text(source)
            {
                let trimmed = txt.trim();
                if trimmed.len() >= 2 {
                    out.insert(trimmed.to_string());
                }
            }
        }
        "type_definition" => {
            if let Some(decl) = node.child_by_field_name("declarator")
                && let Some(name) = extract_c_type_or_var_name(decl, source)
            {
                out.insert(name);
            }
            if let Some(ty) = node.child_by_field_name("type") {
                collect_defined_names_in_node(ty, source, out);
            }
        }
        "declaration" => {
            if let Some(ty) = node.child_by_field_name("type") {
                collect_defined_names_in_node(ty, source, out);
            }
            let mut cursor = node.walk();
            for child in node.children_by_field_name("declarator", &mut cursor) {
                // Skip pure forward function prototypes (`static int foo(int x);`).
                if is_function_prototype_declarator(child) {
                    continue;
                }
                if let Some(name) = extract_c_type_or_var_name(child, source) {
                    out.insert(name);
                }
            }
        }
        _ => {}
    }
}

fn is_function_prototype_declarator(mut cur: Node<'_>) -> bool {
    loop {
        match cur.kind() {
            "function_declarator" => return true,
            "init_declarator" => return false,
            "pointer_declarator" | "parenthesized_declarator" | "attributed_declarator" => {
                let Some(next) = cur.child_by_field_name("declarator") else {
                    return false;
                };
                cur = next;
            }
            _ => return false,
        }
    }
}

fn extract_c_type_or_var_name(mut cur: Node<'_>, source: &[u8]) -> Option<String> {
    loop {
        match cur.kind() {
            "identifier" | "type_identifier" => {
                return cur
                    .utf8_text(source)
                    .ok()
                    .map(str::trim)
                    .filter(|s| s.len() >= 2)
                    .map(str::to_string);
            }
            "init_declarator"
            | "array_declarator"
            | "pointer_declarator"
            | "function_declarator"
            | "parenthesized_declarator"
            | "attributed_declarator" => {
                cur = cur.child_by_field_name("declarator")?;
            }
            _ => return None,
        }
    }
}

fn collect_all_identifiers(node: Node<'_>, source: &[u8], out: &mut BTreeSet<String>) {
    if matches!(node.kind(), "identifier" | "type_identifier")
        && let Ok(txt) = node.utf8_text(source)
    {
        let trimmed = txt.trim();
        if trimmed.len() >= 2 {
            out.insert(trimmed.to_string());
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_all_identifiers(child, source, out);
    }
}

fn extract_identifiers_from_lines(
    lines: &[&str],
    start_line_1: usize,
    end_line_1: usize,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if lines.is_empty() || start_line_1 == 0 || start_line_1 > end_line_1 {
        return out;
    }
    let hi = end_line_1.min(lines.len());
    for line in &lines[(start_line_1 - 1)..hi] {
        for tok in line.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            if tok.len() >= 2 && !tok.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
                out.insert(tok.to_string());
            }
        }
    }
    out
}

fn populate_intra_file_graph(
    functions: &mut [FunctionSpan],
    local_defs: &[FileLocalDef],
    lines: &[&str],
) {
    let fn_names: BTreeSet<String> = functions.iter().map(|f| f.name.clone()).collect();
    if fn_names.is_empty() {
        return;
    }

    let mut callers_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for f in functions.iter_mut() {
        let body_idents = extract_identifiers_from_lines(lines, f.start_line, f.end_line);
        let mut callees = BTreeSet::new();
        for cand in body_idents.intersection(&fn_names) {
            if cand != &f.name {
                callees.insert(cand.clone());
                callers_map
                    .entry(cand.clone())
                    .or_default()
                    .insert(f.name.clone());
            }
        }
        f.intra_file_callees = callees.into_iter().collect();
    }

    for def in local_defs {
        let def_label = def
            .names
            .first()
            .cloned()
            .unwrap_or_else(|| format!("decl@L{}", def.start_line));
        for target_fn in def.referenced_idents.intersection(&fn_names) {
            callers_map
                .entry(target_fn.clone())
                .or_default()
                .insert(def_label.clone());
        }
    }

    for f in functions.iter_mut() {
        if let Some(callers) = callers_map.remove(&f.name) {
            f.intra_file_callers = callers.into_iter().collect();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_chunk_intra_file_context(
    lines: &[&str],
    chunk_start: usize,
    chunk_end: usize,
    preamble_end: usize,
    chunk_fns: &[FunctionSpan],
    all_fns: &[FunctionSpan],
    local_defs: &[FileLocalDef],
    max_chars: usize,
) -> String {
    let chunk_idents = extract_identifiers_from_lines(lines, chunk_start, chunk_end);
    let chunk_fn_names: BTreeSet<&str> = chunk_fns.iter().map(|f| f.name.as_str()).collect();
    let in_chunk_or_preamble = |s: usize, e: usize| -> bool {
        (preamble_end > 0 && e <= preamble_end) || (s >= chunk_start && e <= chunk_end)
    };

    let mut out = String::new();
    let append_snippet = |out: &mut String, header: &str, s_line: usize, e_line: usize| -> bool {
        let clamped_s = s_line.max(1);
        let clamped_e = e_line.min(lines.len());
        if clamped_s > clamped_e {
            return true;
        }
        let mut block = String::from(header);
        for ln in clamped_s..=clamped_e {
            block.push_str(&format!("{:>6}: {}\n", ln, lines[ln - 1]));
        }
        block.push('\n');
        if out.len() + block.len() > max_chars {
            return false;
        }
        out.push_str(&block);
        true
    };

    // 1. Out-of-chunk file-local types, macros, locks, and ops tables referenced by this chunk
    //    (or referencing functions defined in this chunk).
    for def in local_defs {
        if in_chunk_or_preamble(def.start_line, def.end_line) {
            continue;
        }
        let referenced_by_chunk = def.names.iter().any(|n| chunk_idents.contains(n));
        let references_chunk_fn = def
            .referenced_idents
            .iter()
            .any(|id| chunk_fn_names.contains(id.as_str()));
        if !referenced_by_chunk && !references_chunk_fn {
            continue;
        }
        let label = def.names.join(", ");
        let end_capped = def.end_line.min(def.start_line + 60);
        let header = format!(
            "// --- File-local definition `{label}` [lines {}-{}] ---\n",
            def.start_line, def.end_line
        );
        if !append_snippet(&mut out, &header, def.start_line, end_capped) {
            return out;
        }
    }

    // 2. Out-of-chunk 1-hop intra-file callees called/referenced by functions in this chunk.
    let mut wanted_callees = BTreeSet::new();
    for cf in chunk_fns {
        for callee in &cf.intra_file_callees {
            if !chunk_fn_names.contains(callee.as_str()) {
                wanted_callees.insert(callee.as_str());
            }
        }
    }
    for f in all_fns {
        if !wanted_callees.contains(f.name.as_str())
            || in_chunk_or_preamble(f.start_line, f.end_line)
        {
            continue;
        }
        let span_len = f.end_line.saturating_sub(f.start_line) + 1;
        if span_len <= 70 {
            let header = format!(
                "// --- Intra-file callee `{}()` [lines {}-{}] ---\n",
                f.name, f.start_line, f.end_line
            );
            if !append_snippet(&mut out, &header, f.start_line, f.end_line) {
                return out;
            }
        } else {
            let head_end = f.start_line + 24;
            let tail_start = f.end_line.saturating_sub(19).max(head_end + 1);
            let mut block = format!(
                "// --- Intra-file callee `{}()` [lines {}-{}, middle elided] ---\n",
                f.name, f.start_line, f.end_line
            );
            for ln in f.start_line..=head_end.min(lines.len()) {
                block.push_str(&format!("{:>6}: {}\n", ln, lines[ln - 1]));
            }
            block.push_str(&format!(
                "// ... lines {}..{} elided ...\n",
                head_end + 1,
                tail_start - 1
            ));
            for ln in tail_start..=f.end_line.min(lines.len()) {
                block.push_str(&format!("{:>6}: {}\n", ln, lines[ln - 1]));
            }
            block.push('\n');
            if out.len() + block.len() > max_chars {
                return out;
            }
            out.push_str(&block);
        }
    }

    // 3. Out-of-chunk intra-file caller call-site windows for functions defined in this chunk.
    let mut wanted_caller_fns = BTreeSet::new();
    for cf in chunk_fns {
        for caller in &cf.intra_file_callers {
            if !chunk_fn_names.contains(caller.as_str()) {
                wanted_caller_fns.insert(caller.as_str());
            }
        }
    }
    let mut call_windows_emitted = 0usize;
    for caller_fn in all_fns {
        if call_windows_emitted >= 12 {
            break;
        }
        if !wanted_caller_fns.contains(caller_fn.name.as_str())
            || in_chunk_or_preamble(caller_fn.start_line, caller_fn.end_line)
        {
            continue;
        }
        let mut last_emitted_end = 0usize;
        for ln in caller_fn.start_line..=caller_fn.end_line.min(lines.len()) {
            if ln <= last_emitted_end {
                continue;
            }
            let has_hit = lines[ln - 1]
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|tok| chunk_fn_names.contains(tok));
            if !has_hit {
                continue;
            }
            let win_s = ln.saturating_sub(8).max(caller_fn.start_line);
            let win_e = (ln + 8).min(caller_fn.end_line).min(lines.len());
            let header = format!(
                "// --- Intra-file caller `{}()` call-site [lines {}-{}] ---\n",
                caller_fn.name, win_s, win_e
            );
            if !append_snippet(&mut out, &header, win_s, win_e) {
                return out;
            }
            last_emitted_end = win_e;
            call_windows_emitted += 1;
            if call_windows_emitted >= 12 {
                break;
            }
        }
    }

    out
}

fn extract_c_declarator_name(mut cur: Node<'_>, source: &[u8]) -> Option<String> {
    loop {
        match cur.kind() {
            "identifier" => return cur.utf8_text(source).ok().map(str::to_string),
            "function_declarator"
            | "pointer_declarator"
            | "parenthesized_declarator"
            | "attributed_declarator" => {
                cur = cur.child_by_field_name("declarator")?;
            }
            _ => return None,
        }
    }
}

fn collect_call_identifiers(node: Node<'_>, source: &[u8], out: &mut BTreeSet<String>) {
    if node.kind() == "call_expression"
        && let Some(func) = node.child_by_field_name("function")
        && func.kind() == "identifier"
        && let Ok(ident) = func.utf8_text(source)
    {
        let trimmed = ident.trim();
        if trimmed.len() >= 2 && !is_ignorable_call_macro(trimmed) {
            out.insert(trimmed.to_string());
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_call_identifiers(child, source, out);
    }
}

fn is_ignorable_call_macro(name: &str) -> bool {
    matches!(
        name,
        "likely"
            | "unlikely"
            | "IS_ERR"
            | "PTR_ERR"
            | "ERR_PTR"
            | "IS_ERR_OR_NULL"
            | "WARN_ON"
            | "WARN_ON_ONCE"
            | "BUG_ON"
            | "BUILD_BUG_ON"
            | "pr_debug"
            | "pr_info"
            | "pr_warn"
            | "pr_err"
            | "dev_dbg"
            | "dev_info"
            | "dev_warn"
            | "dev_err"
    )
}

fn detect_risk_signals(body_text: &str) -> Vec<String> {
    const SIGNAL_RULES: &[(&str, &[&str])] = &[
        (
            "user_copy",
            &[
                "copy_from_user",
                "copy_to_user",
                "get_user(",
                "put_user(",
                "__get_user(",
                "__put_user(",
                "memdup_user",
                "strndup_user",
            ],
        ),
        (
            "alloc_free",
            &[
                "kmalloc",
                "kzalloc",
                "kcalloc",
                "krealloc",
                "kvmalloc",
                "kvzalloc",
                "kfree",
                "kvfree",
                "vfree",
                "kmem_cache_alloc",
                "kmem_cache_free",
                "alloc_pages",
                "folio_alloc",
            ],
        ),
        (
            "locking",
            &[
                "mutex_lock",
                "spin_lock",
                "raw_spin_lock",
                "read_lock",
                "write_lock",
                "down_read",
                "down_write",
                "rcu_read_lock",
                "synchronize_rcu",
                "call_rcu",
            ],
        ),
        (
            "refcount",
            &[
                "kref_get",
                "kref_put",
                "refcount_inc",
                "refcount_dec",
                "get_device",
                "put_device",
                "fget",
                "fput",
                "igrab",
                "iput",
                "sock_hold",
                "sock_put",
            ],
        ),
        (
            "async_work",
            &[
                "schedule_work",
                "queue_work",
                "queue_delayed_work",
                "cancel_work_sync",
                "cancel_delayed_work_sync",
                "irq_work_queue",
                "irq_work_sync",
                "mod_timer",
                "del_timer_sync",
                "timer_delete_sync",
                "timer_shutdown_sync",
            ],
        ),
        (
            "xarray_idr",
            &[
                "xa_store",
                "xas_store",
                "xas_nomem",
                "xa_destroy",
                "idr_alloc",
                "ida_alloc",
            ],
        ),
        (
            "cgroup_kernfs",
            &[
                "css_free",
                "css_released",
                "css_offline",
                "percpu_ref_",
                "kernfs_break_active_protection",
                "__GFP_ACCOUNT",
                "XA_FLAGS_ACCOUNT",
            ],
        ),
        (
            "dma_mmio",
            &[
                "dma_map_",
                "dma_unmap_",
                "dma_alloc_",
                "dma_free_",
                "readl(",
                "writel(",
                "ioremap",
                "iounmap",
            ],
        ),
        (
            "error_goto",
            &[
                "goto err",
                "goto out",
                "goto fail",
                "goto free",
                "goto unlock",
                "goto put",
            ],
        ),
    ];

    let mut matched = Vec::new();
    for &(label, patterns) in SIGNAL_RULES {
        if patterns.iter().any(|pat| body_text.contains(pat)) {
            matched.push(label.to_string());
        }
    }
    matched
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustScanLexState {
    Normal,
    BlockComment(usize),
    StringLiteral,
    RawStringLiteral(usize),
}

fn scan_rust_line_braces(
    line: &str,
    state: &mut RustScanLexState,
    brace_depth: &mut i32,
    seen_open_brace: &mut bool,
) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut ended_with_semi_outside_body = false;

    while i < bytes.len() {
        match *state {
            RustScanLexState::BlockComment(depth) => {
                if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    *state = RustScanLexState::BlockComment(depth + 1);
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    if depth <= 1 {
                        *state = RustScanLexState::Normal;
                    } else {
                        *state = RustScanLexState::BlockComment(depth - 1);
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            RustScanLexState::StringLiteral => {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == b'"' {
                    *state = RustScanLexState::Normal;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            RustScanLexState::RawStringLiteral(hashes) => {
                if bytes[i] == b'"' {
                    let mut matched = 0;
                    while i + 1 + matched < bytes.len()
                        && matched < hashes
                        && bytes[i + 1 + matched] == b'#'
                    {
                        matched += 1;
                    }
                    if matched == hashes {
                        *state = RustScanLexState::Normal;
                        i += 1 + hashes;
                        continue;
                    }
                }
                i += 1;
            }
            RustScanLexState::Normal => {
                if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                    break;
                }
                if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    *state = RustScanLexState::BlockComment(1);
                    i += 2;
                    continue;
                }
                if bytes[i] == b'r'
                    || (i + 1 < bytes.len() && bytes[i] == b'b' && bytes[i + 1] == b'r')
                {
                    let r_pos = if bytes[i] == b'b' { i + 1 } else { i };
                    let mut hashes = 0;
                    while r_pos + 1 + hashes < bytes.len() && bytes[r_pos + 1 + hashes] == b'#' {
                        hashes += 1;
                    }
                    if r_pos + 1 + hashes < bytes.len() && bytes[r_pos + 1 + hashes] == b'"' {
                        *state = RustScanLexState::RawStringLiteral(hashes);
                        i = r_pos + 2 + hashes;
                        continue;
                    }
                }
                if bytes[i] == b'"' {
                    *state = RustScanLexState::StringLiteral;
                    i += 1;
                    continue;
                }
                if bytes[i] == b'\'' {
                    if i + 2 < bytes.len() && bytes[i + 1] == b'\\' {
                        // Character escape literal like '\'' or '\x7f'.
                        let mut close = i + 3;
                        while close < bytes.len() && bytes[close] != b'\'' {
                            close += 1;
                        }
                        if close < bytes.len() {
                            i = close + 1;
                            continue;
                        }
                    } else if let Some(ch) = line[i + 1..].chars().next() {
                        let next_quote = i + 1 + ch.len_utf8();
                        if ch != '\'' && next_quote < bytes.len() && bytes[next_quote] == b'\'' {
                            i = next_quote + 1;
                            continue;
                        }
                    }
                }
                if bytes[i] == b'{' {
                    *brace_depth += 1;
                    *seen_open_brace = true;
                    ended_with_semi_outside_body = false;
                } else if bytes[i] == b'}' {
                    *brace_depth -= 1;
                    ended_with_semi_outside_body = false;
                    if *seen_open_brace && *brace_depth <= 0 {
                        return false;
                    }
                } else if bytes[i] == b';' && !*seen_open_brace {
                    ended_with_semi_outside_body = true;
                } else if !bytes[i].is_ascii_whitespace() {
                    ended_with_semi_outside_body = false;
                }
                i += 1;
            }
        }
    }

    ended_with_semi_outside_body
}

fn extract_rust_functions(source: &str) -> Vec<FunctionSpan> {
    let lines: Vec<&str> = source.lines().collect();
    let Ok(fn_re) = regex::Regex::new(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:async\s+)?(?:const\s+)?fn\s+([a-zA-Z_][a-zA-Z0-9_]*)",
    ) else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        let Some(caps) = fn_re.captures(line) else {
            idx += 1;
            continue;
        };
        let name = caps[1].to_string();
        let start_line = idx + 1;

        let mut brace_depth: i32 = 0;
        let mut seen_open_brace = false;
        let mut found_end_idx: Option<usize> = None;
        let mut lex_state = RustScanLexState::Normal;
        for (scan_idx, scan_line) in lines.iter().enumerate().skip(idx) {
            let ended_with_semi = scan_rust_line_braces(
                scan_line,
                &mut lex_state,
                &mut brace_depth,
                &mut seen_open_brace,
            );
            if !seen_open_brace && ended_with_semi {
                break;
            }
            if seen_open_brace && brace_depth <= 0 {
                found_end_idx = Some(scan_idx);
                break;
            }
        }

        if let Some(end_idx) = found_end_idx {
            let body_text = lines[idx..=end_idx].join("\n");
            let risk_signals = detect_risk_signals(&body_text);
            spans.push(FunctionSpan {
                name,
                signature: line.trim().trim_end_matches('{').trim().to_string(),
                start_line,
                end_line: end_idx + 1,
                called_functions: Vec::new(),
                intra_file_callees: Vec::new(),
                intra_file_callers: Vec::new(),
                risk_signals,
            });
            idx = end_idx + 1;
        } else {
            idx += 1;
        }
    }
    spans
}

/// Configuration options for `sashiko scan`.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub project: crate::project::ProjectId,
    pub repo_path: PathBuf,
    pub paths: Vec<PathBuf>,
    pub git_ref: Option<String>,
    pub concurrency: usize,
    pub max_files: usize,
    pub max_chunks: Option<usize>,
    pub exclude: Vec<String>,
    pub submit_bugs: bool,
    pub min_severity: crate::db::SeverityFloor,
    pub prompts: PathBuf,
    pub custom_prompt: Option<String>,
    pub stages: Option<Vec<String>>,
    pub effort: Option<crate::ai::EffortMode>,
}

/// A verified defect discovered by `sashiko scan` and processed through the
/// `linux_bug` verification, origin-tracing, and severity calibration pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedScanBug {
    pub canonical_title: String,
    pub canonical_description: String,
    pub severity: String,
    pub severity_explanation: String,
    pub subsystems: Vec<String>,
    pub source_files: Vec<String>,
    pub locations: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduced_in_commit: Option<String>,
    pub verified_on_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_bugid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of_bugid: Option<String>,
}

impl VerifiedScanBug {
    pub fn from_standalone(bug: crate::workflows::linux_bug::StandaloneVerifiedBug) -> Self {
        Self {
            canonical_title: bug.canonical_title,
            canonical_description: bug.canonical_description,
            severity: bug.severity.as_str().to_string(),
            severity_explanation: bug.severity_explanation,
            subsystems: bug.subsystems.into_iter().map(|s| s.name).collect(),
            source_files: bug.source_files,
            locations: bug.locations.unwrap_or_else(|| serde_json::json!([])),
            introduced_in_commit: bug.introduced_in_commit,
            verified_on_sha: bug.verified_on_sha,
            submitted_bugid: None,
            duplicate_of_bugid: None,
        }
    }

    #[cfg(feature = "server")]
    pub fn from_db_bug(
        bug: &crate::db::Bug,
        fallback_input: &crate::api::BugInput,
        target_sha: &str,
        submitted_bugid: Option<String>,
        duplicate_of_bugid: Option<String>,
    ) -> Self {
        let subsystems = if bug.subsystems.is_empty() {
            fallback_input
                .subsystems
                .iter()
                .map(|s| s.name.clone())
                .collect()
        } else {
            bug.subsystems.clone()
        };
        Self {
            canonical_title: bug.title.clone(),
            canonical_description: bug
                .description()
                .unwrap_or_else(|| fallback_input.reasoning.clone()),
            severity: bug.severity().as_str().to_string(),
            severity_explanation: bug.severity_explanation().unwrap_or_default(),
            subsystems,
            source_files: bug
                .source_files()
                .unwrap_or_else(|| fallback_input.source_files.clone()),
            locations: bug
                .locations()
                .or_else(|| fallback_input.locations.clone())
                .unwrap_or_else(|| serde_json::json!([])),
            introduced_in_commit: bug.introduced_in_commit(),
            verified_on_sha: bug
                .verified_on_sha()
                .unwrap_or_else(|| target_sha.to_string()),
            submitted_bugid,
            duplicate_of_bugid,
        }
    }
}

/// Progress events emitted during `sashiko scan`.
#[derive(Debug, Clone)]
pub enum ScanProgressEvent {
    ResolvedTargets {
        target_sha: String,
        files_count: usize,
        chunks_count: usize,
    },
    ChunkStarted {
        chunk_id: String,
        file_path: String,
        start_line: usize,
        end_line: usize,
    },
    ChunkStageStarted {
        chunk_id: String,
        stage: String,
    },
    ChunkFinished {
        chunk_id: String,
        findings_count: usize,
        early_exited: bool,
    },
    ChunkFailed {
        chunk_id: String,
        error: String,
    },
    CandidateVerifyStarted {
        index: usize,
        total: usize,
        problem: String,
    },
    CandidateVerified {
        canonical_title: String,
        severity: String,
    },
    CandidateDiscarded {
        problem: String,
        reason: String,
    },
    CandidateFailed {
        problem: String,
        error: String,
    },
    BugSubmitted {
        canonical_title: String,
        bugid: String,
        is_duplicate: bool,
    },
}

pub type ScanProgressCallback<'a> = dyn Fn(ScanProgressEvent) + Send + Sync + 'a;

fn emit_scan_progress(progress: Option<&ScanProgressCallback<'_>>, event: ScanProgressEvent) {
    if let Some(cb) = progress {
        cb(event);
    }
}

/// Complete summary of a `sashiko scan` execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanSummary {
    pub target_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_severity: Option<crate::db::SeverityFloor>,
    pub files_scanned: usize,
    pub chunks_total: usize,
    pub chunks_analyzed: usize,
    pub chunks_early_exited: usize,
    pub chunks_failed: usize,
    pub candidates_failed: usize,
    pub findings: Vec<VerifiedScanBug>,
    pub dismissed_concerns: Vec<serde_json::Value>,
    pub tokens_in: usize,
    pub tokens_out: usize,
    pub tokens_cached: usize,
}

fn normalize_title_tokens(title: &str) -> BTreeSet<String> {
    title
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 3)
        .map(str::to_string)
        .collect()
}

fn extract_location_symbols(locations: &serde_json::Value) -> BTreeSet<(String, String)> {
    let mut set = BTreeSet::new();
    if let Some(arr) = locations.as_array() {
        for loc in arr {
            let file = loc
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let sym = loc
                .get("function_or_symbol")
                .or_else(|| loc.get("function"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .trim_end_matches("()")
                .to_string();
            if !file.is_empty() && !sym.is_empty() {
                set.insert((file, sym));
            }
        }
    }
    set
}

fn strip_title_subsystem_prefix(title: &str) -> &str {
    let mut rest = title.trim();
    while let Some((head, tail)) = rest.split_once(':') {
        // Keep C++/Rust `Type::method` intact.
        if tail.starts_with(':') {
            break;
        }
        let head_trimmed = head.trim();
        let word_count = head_trimmed.split_whitespace().count();
        if head_trimmed.len() <= 28 && word_count <= 3 && !tail.trim().is_empty() {
            rest = tail.trim();
        } else {
            break;
        }
    }
    rest
}

fn is_dedup_stopword(w: &str) -> bool {
    matches!(
        w,
        "the" | "and" | "for" | "with" | "from" | "when" | "into" | "upon" | "via"
    )
}

fn extract_defect_mechanism_tokens(
    title: &str,
    syms: &BTreeSet<(String, String)>,
) -> BTreeSet<String> {
    let stripped = strip_title_subsystem_prefix(title);
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    for (file, sym) in syms {
        excluded.insert(sym.to_ascii_lowercase());
        for part in file.split(['/', '.']) {
            if part.len() >= 3 {
                excluded.insert(part.to_ascii_lowercase());
            }
        }
    }
    let filtered: BTreeSet<String> = stripped
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 3 && !is_dedup_stopword(w) && !excluded.contains(*w))
        .map(str::to_string)
        .collect();
    if filtered.is_empty() {
        normalize_title_tokens(stripped)
    } else {
        filtered
    }
}

fn extract_location_lines(locations: &serde_json::Value) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    if let Some(arr) = locations.as_array() {
        for loc in arr {
            let file = loc
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if let Some(line) = loc.get("line").and_then(|v| v.as_i64())
                && !file.is_empty()
                && line > 0
            {
                out.push((file, line));
            }
        }
    }
    out
}

fn min_same_file_line_distance(
    locs_a: &serde_json::Value,
    locs_b: &serde_json::Value,
) -> Option<i64> {
    let a = extract_location_lines(locs_a);
    let b = extract_location_lines(locs_b);
    let mut best: Option<i64> = None;
    for (fa, la) in &a {
        for (fb, lb) in &b {
            if fa == fb {
                let d = (la - lb).abs();
                best = Some(best.map_or(d, |cur| cur.min(d)));
            }
        }
    }
    best
}

fn extract_title_function_names(title: &str) -> BTreeSet<String> {
    let mut fns = BTreeSet::new();
    for raw in title.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':')) {
        let token = raw.trim_matches(|c: char| matches!(c, '\'' | '"' | '`'));
        if let Some(stem) = token.strip_suffix("()") {
            let ident = stem.rsplit("::").next().unwrap_or(stem).trim();
            if ident.len() >= 3
                && ident
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                fns.insert(ident.to_ascii_lowercase());
            }
        }
    }
    fns
}

fn is_title_and_locations_duplicate(
    existing_title: &str,
    existing_locations: &serde_json::Value,
    cand_title_norm: &str,
    cand_locations: &serde_json::Value,
    cand_tokens: &BTreeSet<String>,
    cand_syms: &BTreeSet<(String, String)>,
) -> bool {
    if existing_title.trim().to_ascii_lowercase() == cand_title_norm {
        return true;
    }
    let existing_title_fns = extract_title_function_names(existing_title);
    let cand_title_fns = extract_title_function_names(cand_title_norm);
    if !existing_title_fns.is_empty()
        && !cand_title_fns.is_empty()
        && existing_title_fns.is_disjoint(&cand_title_fns)
    {
        return false;
    }
    let existing_syms = extract_location_symbols(existing_locations);
    let shares_file_and_symbol = !cand_syms.is_empty() && !cand_syms.is_disjoint(&existing_syms);
    if shares_file_and_symbol && !cand_tokens.is_empty() {
        let mut combined_syms = existing_syms;
        combined_syms.extend(cand_syms.iter().cloned());
        let existing_tokens = extract_defect_mechanism_tokens(existing_title, &combined_syms);
        let cand_mech_tokens = extract_defect_mechanism_tokens(cand_title_norm, &combined_syms);
        let intersection = cand_mech_tokens.intersection(&existing_tokens).count();
        let union = cand_mech_tokens.union(&existing_tokens).count();
        let threshold = match min_same_file_line_distance(existing_locations, cand_locations) {
            Some(dist) if dist > 25 => 0.70,
            _ => 0.45,
        };
        if union > 0 && (intersection as f64 / union as f64) >= threshold {
            return true;
        }
    }
    false
}

/// Checks whether `candidate` is an in-run duplicate of any candidate [`crate::api::BugInput`]
/// in `existing` (e.g., discovered in overlapping giant-function windows or caller/callee chunks).
pub fn is_in_run_duplicate_input(
    existing: &[crate::api::BugInput],
    candidate: &crate::api::BugInput,
) -> bool {
    let cand_title_norm = candidate.problem.trim().to_ascii_lowercase();
    let empty_locs = serde_json::json!([]);
    let cand_locs = candidate.locations.as_ref().unwrap_or(&empty_locs);
    let cand_syms = extract_location_symbols(cand_locs);
    let cand_tokens = extract_defect_mechanism_tokens(&candidate.problem, &cand_syms);

    existing.iter().any(|bug| {
        let existing_locs = bug.locations.as_ref().unwrap_or(&empty_locs);
        is_title_and_locations_duplicate(
            &bug.problem,
            existing_locs,
            &cand_title_norm,
            cand_locs,
            &cand_tokens,
            &cand_syms,
        )
    })
}

/// Checks whether `candidate` is an in-run duplicate of any finding in `existing`.
pub fn is_in_run_duplicate(existing: &[VerifiedScanBug], candidate: &VerifiedScanBug) -> bool {
    let cand_title_norm = candidate.canonical_title.trim().to_ascii_lowercase();
    let cand_syms = extract_location_symbols(&candidate.locations);
    let cand_tokens = extract_defect_mechanism_tokens(&candidate.canonical_title, &cand_syms);

    existing.iter().any(|bug| {
        is_title_and_locations_duplicate(
            &bug.canonical_title,
            &bug.locations,
            &cand_title_norm,
            &candidate.locations,
            &cand_tokens,
            &cand_syms,
        )
    })
}

struct ChunkNotesContext {
    maintainer_notes: std::sync::Arc<Vec<crate::notes::maintainer_notes::MaintainerNote>>,
    code_notes_set: Option<std::sync::Arc<crate::notes::select::NotesSet>>,
    code_notes_scope: crate::settings::CodeNotesScope,
    kernel_threat_model: Option<crate::threat_model::KernelThreatModel>,
}

fn render_chunk_notes_blocking(
    notes_ctx: &ChunkNotesContext,
    file_path: &str,
    raw_source: &str,
) -> (String, String) {
    let files = [file_path.to_string()];

    let maintainer_block = if notes_ctx.maintainer_notes.is_empty() {
        String::new()
    } else {
        let identifiers = crate::notes::maintainer_notes::source_identifiers_all(raw_source);
        let selected: Vec<_> = notes_ctx
            .maintainer_notes
            .iter()
            .filter(|note| note.applies(&files, &identifiers))
            .collect();
        crate::notes::maintainer_notes::render_maintainer_notes(&selected)
    };

    let code_block = if let Some(ref set) = notes_ctx.code_notes_set {
        let specific = crate::notes::select::source_identifiers(raw_source);
        let identifiers = match notes_ctx.code_notes_scope {
            crate::settings::CodeNotesScope::Unit => None,
            crate::settings::CodeNotesScope::Touched => Some(&specific),
        };
        let mut selected = crate::notes::select::select_notes(set, &files, identifiers);
        let symbol_notes = crate::notes::select::select_symbol_notes(set, &specific, &selected);
        selected.extend(symbol_notes);
        crate::notes::select::render_block(set, &selected)
    } else {
        String::new()
    };

    (maintainer_block, code_block)
}

async fn preload_scan_notes(
    repo_path: &Path,
    prompts_dir: &Path,
    target_sha: &str,
    use_code_notes: bool,
    code_notes_scope: crate::settings::CodeNotesScope,
) -> ChunkNotesContext {
    let prompts_owned = prompts_dir.to_path_buf();
    let (maintainer_notes, sets) = tokio::task::spawn_blocking(move || {
        let maint = crate::notes::maintainer_notes::load_maintainer_notes(&prompts_owned);
        let sets = if use_code_notes {
            crate::notes::select::load_sets(&prompts_owned)
        } else {
            Vec::new()
        };
        (maint, sets)
    })
    .await
    .unwrap_or_default();

    let code_notes_set = if sets.is_empty() {
        None
    } else {
        match crate::notes::select::choose_set(repo_path, &sets, target_sha).await {
            Ok(Some(chosen)) => Some(std::sync::Arc::new(chosen.clone())),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!("code notes skipped for scan: {err:#}");
                None
            }
        }
    };

    let kernel_threat_model = match crate::threat_model::KernelThreatModel::load(repo_path).await {
        Some(tm) => Some(tm),
        None => {
            crate::threat_model::KernelThreatModel::load_from_ref(
                repo_path,
                &crate::threat_model::MainlineRef {
                    git_ref: "HEAD".to_string(),
                    commit: target_sha.to_string(),
                },
            )
            .await
        }
    };

    ChunkNotesContext {
        maintainer_notes: std::sync::Arc::new(maintainer_notes),
        code_notes_set,
        code_notes_scope,
        kernel_threat_model,
    }
}

async fn load_or_init_maintainers(
    repo_path: &Path,
) -> Option<std::sync::Arc<crate::maintainers::MaintainersIndex>> {
    if let Some(idx) = crate::maintainers::get_global_maintainers() {
        return Some(idx);
    }
    let repo_for_maintainers = repo_path.to_path_buf();
    let loaded = tokio::task::spawn_blocking(move || {
        crate::maintainers::MaintainersIndex::from_top_of_trunk(&repo_for_maintainers)
            .ok()
            .map(std::sync::Arc::new)
    })
    .await
    .ok()
    .flatten();
    if let Some(ref idx) = loaded {
        crate::maintainers::init_global_maintainers(idx.clone());
    }
    loaded
}

async fn build_scan_provider(
    settings: &crate::settings::Settings,
    concurrency: usize,
) -> Result<std::sync::Arc<dyn crate::ai::AiProvider>> {
    let raw_provider = crate::ai::create_provider_cached(&settings.ai, None)
        .await
        .context("Failed to create AI provider for scan")?;
    // Active-scan prompts include full source chunks, intra-file context, and
    // prefetched headers (~35K-55K tokens each). Cap simultaneous in-flight LLM
    // requests tighter than diff-review llm_permits() so parallel specialist stages
    // do not trigger prefill-queue preemption storms on shared model backends.
    let llm_concurrency = concurrency.saturating_mul(2).clamp(3, 6);
    let llm_semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(llm_concurrency));
    let quota = std::sync::Arc::new(crate::ai::quota::QuotaManager::with_limits(
        settings.ai.max_rps,
        settings.ai.max_tpm,
    ));
    Ok(crate::local_review::decorate_provider(
        raw_provider,
        &settings.ai,
        &llm_semaphore,
        &quota,
        &None,
    ))
}

struct ChunkScanResult {
    early_exited: bool,
    candidate_bugs: Vec<crate::api::BugInput>,
    dismissed_concerns: Vec<serde_json::Value>,
    tokens_in: usize,
    tokens_out: usize,
    tokens_cached: usize,
}

#[allow(clippy::too_many_arguments)]
async fn scan_single_chunk(
    chunk: ScanChunk,
    target_sha: &str,
    shared_tools: std::sync::Arc<crate::toolbox::ToolBox>,
    prompts_dir: &Path,
    provider: std::sync::Arc<dyn crate::ai::AiProvider>,
    maintainers: Option<std::sync::Arc<crate::maintainers::MaintainersIndex>>,
    notes_ctx: std::sync::Arc<ChunkNotesContext>,
    max_interactions: usize,
    temperature: f32,
    manual_stages: Option<Vec<String>>,
    custom_prompt: Option<String>,
    effort: Option<crate::ai::EffortMode>,
    min_severity: crate::db::SeverityFloor,
    progress: Option<&ScanProgressCallback<'_>>,
) -> Result<ChunkScanResult> {
    let chunk_id = chunk.chunk_id();
    emit_scan_progress(
        progress,
        ScanProgressEvent::ChunkStarted {
            chunk_id: chunk_id.clone(),
            file_path: chunk.file_path.clone(),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
        },
    );

    // Header chunks that contain only declarations or trivial inline accessors with no
    // function calls and no risk signals are already covered via header prefetch on their
    // callers and can short-circuit deterministically without an LLM triage round-trip.
    if chunk.file_path.ends_with(".h")
        && manual_stages.as_ref().is_none_or(Vec::is_empty)
        && chunk
            .functions
            .iter()
            .all(|f| f.called_functions.is_empty() && f.risk_signals.is_empty())
    {
        emit_scan_progress(
            progress,
            ScanProgressEvent::ChunkFinished {
                chunk_id,
                findings_count: 0,
                early_exited: true,
            },
        );
        return Ok(ChunkScanResult {
            early_exited: true,
            candidate_bugs: Vec::new(),
            dismissed_concerns: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        });
    }

    let (maintainer_notes, code_notes) =
        if notes_ctx.maintainer_notes.is_empty() && notes_ctx.code_notes_set.is_none() {
            (String::new(), String::new())
        } else {
            let raw_source = chunk.raw_source_lines();
            let file_for_notes = chunk.file_path.clone();
            let ctx = notes_ctx.clone();
            tokio::task::spawn_blocking(move || {
                render_chunk_notes_blocking(&ctx, &file_for_notes, &raw_source)
            })
            .await
            .unwrap_or_default()
        };

    let maintainers_hint = maintainers.as_ref().and_then(|mindex| {
        let matched = mindex.match_files(std::slice::from_ref(&chunk.file_path));
        if matched.is_empty() {
            None
        } else {
            Some(matched.join(", "))
        }
    });

    let mut state = crate::workflows::linux_scan::LinuxScanState {
        chunk_id: chunk_id.clone(),
        file_path: chunk.file_path.clone(),
        target_sha: target_sha.to_string(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        numbered_source: chunk.numbered_source.clone(),
        chunk_functions_json: serde_json::to_string_pretty(&chunk.functions)
            .unwrap_or_else(|_| "[]".to_string()),
        file_function_index: chunk.file_function_index.clone(),
        intra_file_context: chunk.intra_file_context.clone(),
        file_invariants: chunk.file_invariants.clone(),
        contract_notes: chunk.contract_notes.clone(),
        external_callers: chunk.external_callers.clone(),
        prefetched_context: chunk.prefetched_headers.clone(),
        maintainers_hint,
        maintainer_notes,
        code_notes,
        kernel_threat_model: notes_ctx.kernel_threat_model.clone(),
        manual_stages,
        custom_prompt,
        effort,
        min_severity: Some(min_severity),
        ..Default::default()
    };

    let workflow = crate::workflows::linux_scan::build_linux_scan_workflow_with_effort(
        max_interactions,
        temperature,
        effort,
    );
    let env = crate::workflow::WorkflowEnv {
        provider,
        tools: shared_tools,
        base_dir: prompts_dir,
        context_tag: Some(format!("[scan:{chunk_id}] ")),
    };

    let chunk_id_for_cb = chunk_id.clone();
    let event_cb = move |event: crate::workflow::WorkflowEvent| {
        if let crate::workflow::WorkflowEvent::StageStarted { stage_name } = event {
            emit_scan_progress(
                progress,
                ScanProgressEvent::ChunkStageStarted {
                    chunk_id: chunk_id_for_cb.clone(),
                    stage: stage_name.to_string(),
                },
            );
        }
    };

    let outcome =
        crate::workflow::WorkflowEngine::execute(&workflow, &env, &mut state, Some(&event_cb))
            .await?;

    let tokens_in = outcome.tokens_in as usize;
    let tokens_out = outcome.tokens_out as usize;
    let tokens_cached = outcome.tokens_cached as usize;

    let early_exited = state.planned_stages.is_empty()
        && state.manual_stages.as_ref().is_none_or(Vec::is_empty)
        && state.all_concerns.is_empty();
    let candidate_bugs = state.to_candidate_bug_inputs(maintainers.as_deref());

    let dismissed = if !state.deduplicated_dismissed_concerns.is_empty() {
        state.deduplicated_dismissed_concerns
    } else {
        state.all_dismissed_concerns
    };

    emit_scan_progress(
        progress,
        ScanProgressEvent::ChunkFinished {
            chunk_id,
            findings_count: candidate_bugs.len(),
            early_exited,
        },
    );

    Ok(ChunkScanResult {
        early_exited,
        candidate_bugs,
        dismissed_concerns: dismissed,
        tokens_in,
        tokens_out,
        tokens_cached,
    })
}

#[cfg(feature = "server")]
struct TokenAccountingProvider {
    inner: std::sync::Arc<dyn crate::ai::AiProvider>,
    tokens_in: std::sync::atomic::AtomicUsize,
    tokens_out: std::sync::atomic::AtomicUsize,
    tokens_cached: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "server")]
#[async_trait::async_trait]
impl crate::ai::AiProvider for TokenAccountingProvider {
    async fn generate_content(
        &self,
        request: crate::ai::AiRequest,
    ) -> Result<crate::ai::AiResponse> {
        let resp = self.inner.generate_content(request).await?;
        if let Some(ref usage) = resp.usage {
            self.tokens_in
                .fetch_add(usage.prompt_tokens, std::sync::atomic::Ordering::Relaxed);
            self.tokens_out.fetch_add(
                usage.completion_tokens,
                std::sync::atomic::Ordering::Relaxed,
            );
            self.tokens_cached.fetch_add(
                usage.cached_tokens.unwrap_or(0),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        Ok(resp)
    }

    fn handles_active_budget(&self) -> bool {
        self.inner.handles_active_budget()
    }

    fn handles_dispatch_gate(&self) -> bool {
        self.inner.handles_dispatch_gate()
    }

    fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
        self.inner.get_capabilities()
    }

    async fn forget(&self, request: &crate::ai::AiRequest) {
        self.inner.forget(request).await;
    }

    fn cache_stats(&self) -> Option<crate::ai::CacheStats> {
        self.inner.cache_stats()
    }

    fn cache_identity(&self) -> String {
        self.inner.cache_identity()
    }
}

#[cfg(feature = "server")]
async fn open_submit_bugs_db(
    settings: &crate::settings::Settings,
) -> Result<std::sync::Arc<crate::db::Database>> {
    if settings.database.url.trim().is_empty() {
        bail!("database.url must be configured in Settings.toml when --submit-bugs is enabled");
    }
    let db = std::sync::Arc::new(
        crate::db::Database::new(&settings.database)
            .await
            .context("Failed to open database for --submit-bugs")?,
    );
    db.migrate()
        .await
        .context("Failed to run database migrations for --submit-bugs")?;
    db.ensure_project_stamp(crate::project::ProjectId::Linux)
        .await
        .context("Database project stamp mismatch for --submit-bugs")?;
    Ok(db)
}

#[cfg(feature = "server")]
#[allow(clippy::too_many_arguments)]
async fn submit_scan_candidates(
    db: std::sync::Arc<crate::db::Database>,
    provider: std::sync::Arc<dyn crate::ai::AiProvider>,
    options: &ScanOptions,
    settings: &crate::settings::Settings,
    target_sha: &str,
    candidate_bugs: Vec<crate::api::BugInput>,
    summary: &mut ScanSummary,
    progress: Option<&ScanProgressCallback<'_>>,
) {
    if candidate_bugs.is_empty() {
        return;
    }
    let accounting_provider = std::sync::Arc::new(TokenAccountingProvider {
        inner: provider.clone(),
        tokens_in: std::sync::atomic::AtomicUsize::new(0),
        tokens_out: std::sync::atomic::AtomicUsize::new(0),
        tokens_cached: std::sync::atomic::AtomicUsize::new(0),
    });
    let attributed_db = db.with_bug_actor(
        "sashiko",
        "sashiko:linux_scan",
        Some(provider.get_capabilities().model_name),
    );
    let bug_worker = std::sync::Arc::new(
        crate::worker::bug_worker::BugWorker::new(
            db,
            accounting_provider.clone(),
            options.repo_path.to_string_lossy().to_string(),
        )
        .with_project(crate::project::ProjectId::Linux)
        .with_settings(settings.linux_bug.clone())
        .with_effort(options.effort.or_else(|| settings.effective_effort()))
        .with_max_interactions(settings.ai.max_interactions)
        .with_min_severity(Some(options.min_severity)),
    );
    let attributed_db = std::sync::Arc::new(attributed_db);
    let verify_sem = std::sync::Arc::new(tokio::sync::Semaphore::new(options.concurrency.max(1)));
    let total = candidate_bugs.len();

    let min_severity = options.min_severity;
    let submit_futures = candidate_bugs
        .into_iter()
        .enumerate()
        .map(|(idx, mut input)| {
            let sem = verify_sem.clone();
            let worker = bug_worker.clone();
            let attr_db = attributed_db.clone();
            async move {
                let _permit = sem.acquire_owned().await;
                input.min_severity.get_or_insert(min_severity);
                let fallback_input = input.clone();
                emit_scan_progress(
                    progress,
                    ScanProgressEvent::CandidateVerifyStarted {
                        index: idx + 1,
                        total,
                        problem: fallback_input.problem.clone(),
                    },
                );
                let res = worker.submit_and_process_bug(&attr_db, input).await;
                (fallback_input, res)
            }
        });

    let outcomes = futures_util::future::join_all(submit_futures).await;

    for (fallback_input, res) in outcomes {
        match res {
            Ok(Some(crate::api::BugOutcome::NewlyDiscovered { bug: saved })) => {
                if options.min_severity.admits(saved.severity()) {
                    emit_scan_progress(
                        progress,
                        ScanProgressEvent::BugSubmitted {
                            canonical_title: saved.title.clone(),
                            bugid: saved.bugid.clone(),
                            is_duplicate: false,
                        },
                    );
                    let verified = VerifiedScanBug::from_db_bug(
                        &saved,
                        &fallback_input,
                        target_sha,
                        Some(saved.bugid.clone()),
                        None,
                    );
                    if !is_in_run_duplicate(&summary.findings, &verified) {
                        summary.findings.push(verified);
                    }
                } else {
                    let reason = format!(
                        "Calibrated severity {} is below the {} active-scan threshold",
                        saved.severity().as_str(),
                        options.min_severity
                    );
                    emit_scan_progress(
                        progress,
                        ScanProgressEvent::CandidateDiscarded {
                            problem: fallback_input.problem.clone(),
                            reason: reason.clone(),
                        },
                    );
                    summary.dismissed_concerns.push(serde_json::json!({
                        "origin": "linux_bug_verification",
                        "description": fallback_input.problem,
                        "reasoning": reason,
                    }));
                }
            }
            Ok(Some(crate::api::BugOutcome::Duplicate { existing_bug, .. })) => {
                if existing_bug.lifecycle_status == crate::db::BugLifecycleStatus::Open
                    && options.min_severity.admits(existing_bug.severity())
                {
                    emit_scan_progress(
                        progress,
                        ScanProgressEvent::BugSubmitted {
                            canonical_title: existing_bug.title.clone(),
                            bugid: existing_bug.bugid.clone(),
                            is_duplicate: true,
                        },
                    );
                    let verified = VerifiedScanBug::from_db_bug(
                        &existing_bug,
                        &fallback_input,
                        target_sha,
                        None,
                        Some(existing_bug.bugid.clone()),
                    );
                    if !is_in_run_duplicate(&summary.findings, &verified) {
                        summary.findings.push(verified);
                    }
                } else {
                    let reason = format!(
                        "Matched existing bug {} ({}, severity {})",
                        existing_bug.bugid,
                        existing_bug.lifecycle_status.as_str(),
                        existing_bug.severity().as_str()
                    );
                    emit_scan_progress(
                        progress,
                        ScanProgressEvent::CandidateDiscarded {
                            problem: fallback_input.problem.clone(),
                            reason: reason.clone(),
                        },
                    );
                    summary.dismissed_concerns.push(serde_json::json!({
                        "origin": "linux_bug_verification",
                        "description": fallback_input.problem,
                        "reasoning": reason,
                    }));
                }
            }
            Ok(Some(crate::api::BugOutcome::Discarded { reason, .. })) => {
                emit_scan_progress(
                    progress,
                    ScanProgressEvent::CandidateDiscarded {
                        problem: fallback_input.problem.clone(),
                        reason: reason.clone(),
                    },
                );
                summary.dismissed_concerns.push(serde_json::json!({
                    "origin": "linux_bug_verification",
                    "description": fallback_input.problem,
                    "reasoning": reason,
                }));
            }
            Ok(None) => {
                summary.candidates_failed += 1;
                let msg = "Lost bug deduplication lease before verification completed".to_string();
                tracing::warn!(
                    "Failed to finish verifying scan finding '{}': {}",
                    fallback_input.problem,
                    msg
                );
                emit_scan_progress(
                    progress,
                    ScanProgressEvent::CandidateFailed {
                        problem: fallback_input.problem,
                        error: msg,
                    },
                );
            }
            Err(e) => {
                summary.candidates_failed += 1;
                let msg = e.to_string();
                tracing::warn!(
                    "Failed to process and submit scan finding '{}' to bug repository: {}",
                    fallback_input.problem,
                    msg
                );
                emit_scan_progress(
                    progress,
                    ScanProgressEvent::CandidateFailed {
                        problem: fallback_input.problem,
                        error: msg,
                    },
                );
            }
        }
    }

    summary.tokens_in += accounting_provider
        .tokens_in
        .load(std::sync::atomic::Ordering::Relaxed);
    summary.tokens_out += accounting_provider
        .tokens_out
        .load(std::sync::atomic::Ordering::Relaxed);
    summary.tokens_cached += accounting_provider
        .tokens_cached
        .load(std::sync::atomic::Ordering::Relaxed);
}

fn merge_file_prefetched_headers(chunks: &[ScanChunk], max_chars: usize) -> String {
    let mut seen_headers = BTreeSet::new();
    let mut out = String::new();
    for chunk in chunks {
        for section in chunk.prefetched_headers.split("\n--- ") {
            let trimmed = section.trim();
            if trimmed.is_empty() || trimmed.starts_with("Source revision:") {
                continue;
            }
            let normalized = if trimmed.starts_with("--- ") {
                trimmed.to_string()
            } else {
                format!("--- {trimmed}")
            };
            let key = normalized.lines().next().unwrap_or("").to_string();
            if !key.is_empty() && seen_headers.insert(key) {
                if out.len() + normalized.len() + 2 > max_chars {
                    return out;
                }
                out.push_str(&normalized);
                out.push_str("\n\n");
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn verify_scan_candidates_standalone(
    provider: &dyn crate::ai::AiProvider,
    shared_tools: std::sync::Arc<crate::toolbox::ToolBox>,
    options: &ScanOptions,
    file_invariants_by_file: &BTreeMap<String, String>,
    candidate_bugs: Vec<crate::api::BugInput>,
    summary: &mut ScanSummary,
    progress: Option<&ScanProgressCallback<'_>>,
) {
    if candidate_bugs.is_empty() {
        return;
    }
    let verify_sem = std::sync::Arc::new(tokio::sync::Semaphore::new(options.concurrency.max(1)));
    let total = candidate_bugs.len();

    let min_severity = options.min_severity;
    let verify_futures = candidate_bugs.into_iter().enumerate().map(|(idx, input)| {
        let sem = verify_sem.clone();
        let tools = shared_tools.clone();
        let extra_ctx = input
            .source_files
            .iter()
            .filter_map(|f| file_invariants_by_file.get(f))
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("Synthesized File Invariants:\n{}", s.trim()))
            .collect::<Vec<_>>()
            .join("\n\n");
        let extra_context = if extra_ctx.is_empty() {
            None
        } else {
            Some(extra_ctx)
        };
        async move {
            let _permit = sem.acquire_owned().await;
            let problem = input.problem.clone();
            emit_scan_progress(
                progress,
                ScanProgressEvent::CandidateVerifyStarted {
                    index: idx + 1,
                    total,
                    problem: problem.clone(),
                },
            );
            let res = crate::workflows::linux_bug::run_standalone_bug_pipeline_with_options(
                provider,
                Some(tools),
                input,
                Some("[scan:linux_bug] "),
                crate::project::ProjectId::Linux,
                crate::workflows::linux_bug::StandaloneBugPipelineOptions {
                    min_severity,
                    fast_origin_tracing: true,
                    reuse_canonical_input: true,
                    extra_context,
                    max_session_turns: 6,
                },
            )
            .await;
            (problem, res)
        }
    });

    let outcomes = futures_util::future::join_all(verify_futures).await;

    for (problem, res) in outcomes {
        match res {
            Ok(crate::workflows::linux_bug::StandaloneBugOutcome::Verified(bug)) => {
                summary.tokens_in += bug.tokens_in;
                summary.tokens_out += bug.tokens_out;
                summary.tokens_cached += bug.tokens_cached;
                let verified = VerifiedScanBug::from_standalone(*bug);
                emit_scan_progress(
                    progress,
                    ScanProgressEvent::CandidateVerified {
                        canonical_title: verified.canonical_title.clone(),
                        severity: verified.severity.clone(),
                    },
                );
                if !is_in_run_duplicate(&summary.findings, &verified) {
                    summary.findings.push(verified);
                }
            }
            Ok(crate::workflows::linux_bug::StandaloneBugOutcome::Discarded {
                reason,
                tokens_in,
                tokens_out,
                tokens_cached,
            }) => {
                summary.tokens_in += tokens_in;
                summary.tokens_out += tokens_out;
                summary.tokens_cached += tokens_cached;
                tracing::info!(
                    "Scan candidate '{}' discarded by linux_bug pipeline: {}",
                    problem,
                    reason
                );
                emit_scan_progress(
                    progress,
                    ScanProgressEvent::CandidateDiscarded {
                        problem: problem.clone(),
                        reason: reason.clone(),
                    },
                );
                summary.dismissed_concerns.push(serde_json::json!({
                    "origin": "linux_bug_verification",
                    "description": problem,
                    "reasoning": reason,
                }));
            }
            Err(e) => {
                summary.candidates_failed += 1;
                if let Some(pf) =
                    e.downcast_ref::<crate::workflows::linux_bug::StandaloneBugPipelineFailure>()
                {
                    summary.tokens_in += pf.tokens_in;
                    summary.tokens_out += pf.tokens_out;
                    summary.tokens_cached += pf.tokens_cached;
                } else if let Some(sf) = e.downcast_ref::<crate::ai::session::SessionFailure>() {
                    summary.tokens_in += sf.usage.prompt_tokens;
                    summary.tokens_out += sf.usage.completion_tokens;
                    summary.tokens_cached += sf.usage.cached_tokens.unwrap_or(0);
                }
                let msg = e.to_string();
                tracing::warn!(
                    "Failed to run linux_bug pipeline for scan candidate '{}': {}",
                    problem,
                    msg
                );
                emit_scan_progress(
                    progress,
                    ScanProgressEvent::CandidateFailed {
                        problem,
                        error: msg,
                    },
                );
            }
        }
    }
}

/// Executes an active scan across the requested files/directories in `options`.
pub async fn run_scan(
    options: ScanOptions,
    settings: &crate::settings::Settings,
    progress: Option<&ScanProgressCallback<'_>>,
) -> Result<ScanSummary> {
    ensure!(
        options.project == crate::project::ProjectId::Linux,
        "sashiko scan currently supports only --project linux (got --project {})",
        options.project
    );

    #[cfg(not(feature = "server"))]
    if options.submit_bugs {
        bail!("--submit-bugs requires a sashiko binary built with the 'server' feature enabled");
    }

    #[cfg(feature = "server")]
    let submit_db = if options.submit_bugs {
        Some(open_submit_bugs_db(settings).await?)
    } else {
        None
    };

    let target_sha = resolve_scan_revision(&options.repo_path, options.git_ref.as_deref()).await?;
    let files = resolve_scan_targets(
        &options.repo_path,
        &target_sha,
        &options.paths,
        &options.exclude,
        options.max_files,
    )
    .await?;

    let max_chunks = options.max_chunks.filter(|&n| n > 0);
    let mut loaded_files = Vec::new();
    let mut chunks_accum = 0usize;
    for file in &files {
        let remaining = max_chunks.map(|limit| limit.saturating_sub(chunks_accum));
        if remaining == Some(0) {
            break;
        }
        let loaded =
            load_file_chunks_with_limit(&options.repo_path, &target_sha, file, remaining).await?;
        chunks_accum += loaded.chunks.len();
        loaded_files.push(loaded);
        if max_chunks.is_some_and(|limit| chunks_accum >= limit) {
            break;
        }
    }

    emit_scan_progress(
        progress,
        ScanProgressEvent::ResolvedTargets {
            target_sha: target_sha.clone(),
            files_count: files.len(),
            chunks_count: chunks_accum,
        },
    );

    let maintainers = load_or_init_maintainers(&options.repo_path).await;
    let notes_ctx = std::sync::Arc::new(
        preload_scan_notes(
            &options.repo_path,
            &options.prompts,
            &target_sha,
            settings.review.use_code_notes,
            settings.review.code_notes_scope,
        )
        .await,
    );
    let provider = build_scan_provider(settings, options.concurrency).await?;

    let mut shared_tb = crate::toolbox::ToolBox::new(options.repo_path.clone());
    shared_tb.set_virtual_head(target_sha.clone());
    shared_tb.set_active_patch_files(files.clone());
    let shared_tools = std::sync::Arc::new(shared_tb);

    let chunk_semaphore =
        std::sync::Arc::new(tokio::sync::Semaphore::new(options.concurrency.max(1)));
    let max_interactions = settings.ai.max_interactions;
    let temperature = settings.ai.temperature;
    let effort = options.effort.or_else(|| settings.effective_effort());

    let mut synthesis_tokens_in = 0usize;
    let mut synthesis_tokens_out = 0usize;
    let mut synthesis_tokens_cached = 0usize;
    let mut file_invariants_by_file: BTreeMap<String, String> = BTreeMap::new();

    let run_cartographer = options.stages.as_ref().is_none_or(|manual| {
        manual
            .iter()
            .any(|s| s.eq_ignore_ascii_case("contract-audit"))
    });
    if run_cartographer {
        let cartographer_futures = loaded_files.iter().enumerate().filter_map(|(f_idx, lf)| {
            if lf.file_path.ends_with(".h")
                || lf.raw_source.is_empty()
                || lf.raw_source.len() > MAX_WHOLE_FILE_SYNTHESIS_BYTES
            {
                return None;
            }
            let sem = chunk_semaphore.clone();
            let prov = provider.clone();
            let tools_for_cart = shared_tools.clone();
            let mindex = maintainers.clone();
            let notes = notes_ctx.clone();
            let file_path = lf.file_path.clone();
            let raw_source = lf.raw_source.clone();
            let merged_headers = merge_file_prefetched_headers(&lf.chunks, 20_000);
            let ext_callers = lf.whole_file_external_callers.clone();
            let target_sha_owned = target_sha.clone();
            Some(async move {
                let _permit = sem.acquire_owned().await.ok()?;
                let maintainers_hint = mindex.as_ref().and_then(|idx| {
                    let matched = idx.match_files(std::slice::from_ref(&file_path));
                    if matched.is_empty() {
                        None
                    } else {
                        Some(matched.join(", "))
                    }
                });
                let maint_notes = if notes.maintainer_notes.is_empty() {
                    String::new()
                } else {
                    let fp = file_path.clone();
                    let rs = raw_source.clone();
                    let ctx = notes.clone();
                    tokio::task::spawn_blocking(move || {
                        render_chunk_notes_blocking(&ctx, &fp, &rs).0
                    })
                    .await
                    .unwrap_or_default()
                };
                let lines: Vec<&str> = raw_source.lines().collect();
                let numbered_full = format_numbered_lines(&lines, 1, lines.len(), None);
                let res = crate::workflows::linux_scan::run_contract_cartographer_pass(
                    prov.as_ref(),
                    Some(tools_for_cart),
                    &file_path,
                    &target_sha_owned,
                    maintainers_hint.as_deref(),
                    &maint_notes,
                    &numbered_full,
                    &merged_headers,
                    &ext_callers,
                    temperature,
                    effort,
                )
                .await;
                Some((f_idx, file_path, res))
            })
        });

        for outcome in futures_util::future::join_all(cartographer_futures)
            .await
            .into_iter()
            .flatten()
        {
            let (f_idx, file_path, res) = outcome;
            match res {
                Ok(sr) => {
                    synthesis_tokens_in += sr.usage.prompt_tokens;
                    synthesis_tokens_out += sr.usage.completion_tokens;
                    synthesis_tokens_cached += sr.usage.cached_tokens.unwrap_or(0);
                    let contract_notes = sr.output.architecture_contract_notes.trim().to_string();
                    if !contract_notes.is_empty() {
                        for chunk in &mut loaded_files[f_idx].chunks {
                            chunk.contract_notes = contract_notes.clone();
                        }
                    }
                    let inv = sr.output.file_invariants.trim().to_string();
                    if !inv.is_empty() {
                        for chunk in &mut loaded_files[f_idx].chunks {
                            chunk.file_invariants = inv.clone();
                        }
                        file_invariants_by_file.insert(file_path, inv);
                    }
                }
                Err(e) => {
                    if let Some(sf) = e.downcast_ref::<crate::ai::session::SessionFailure>() {
                        synthesis_tokens_in += sf.usage.prompt_tokens;
                        synthesis_tokens_out += sf.usage.completion_tokens;
                        synthesis_tokens_cached += sf.usage.cached_tokens.unwrap_or(0);
                    }
                    tracing::warn!(
                        "Contract cartographer pass skipped for '{}': {}",
                        file_path,
                        e
                    );
                }
            }
        }
    }

    let mut all_chunks = Vec::with_capacity(chunks_accum);
    for mut lf in loaded_files {
        all_chunks.append(&mut lf.chunks);
    }

    let chunks_total = all_chunks.len();
    let chunk_futures = all_chunks.into_iter().map(|chunk| {
        let sem = chunk_semaphore.clone();
        let target_sha_ref = &target_sha;
        let tools_for_chunk = shared_tools.clone();
        let prompts_ref = options.prompts.as_path();
        let prov = provider.clone();
        let mindex = maintainers.clone();
        let notes = notes_ctx.clone();
        let manual_stages = options.stages.clone();
        let custom_prompt = options.custom_prompt.clone();
        async move {
            let _permit = sem
                .acquire_owned()
                .await
                .context("Scan chunk semaphore closed")?;
            let chunk_id = chunk.chunk_id();
            match scan_single_chunk(
                chunk,
                target_sha_ref,
                tools_for_chunk,
                prompts_ref,
                prov,
                mindex,
                notes,
                max_interactions,
                temperature,
                manual_stages,
                custom_prompt,
                effort,
                options.min_severity,
                progress,
            )
            .await
            {
                Ok(res) => Ok(res),
                Err(e) => {
                    emit_scan_progress(
                        progress,
                        ScanProgressEvent::ChunkFailed {
                            chunk_id,
                            error: e.to_string(),
                        },
                    );
                    Err(e)
                }
            }
        }
    });

    let results = futures_util::future::join_all(chunk_futures).await;

    let mut summary = ScanSummary {
        target_sha: target_sha.clone(),
        min_severity: Some(options.min_severity),
        files_scanned: files.len(),
        chunks_total,
        tokens_in: synthesis_tokens_in,
        tokens_out: synthesis_tokens_out,
        tokens_cached: synthesis_tokens_cached,
        ..Default::default()
    };

    let mut candidate_bugs: Vec<crate::api::BugInput> = Vec::new();
    for res in results {
        match res {
            Ok(chunk_res) => {
                if chunk_res.early_exited {
                    summary.chunks_early_exited += 1;
                } else {
                    summary.chunks_analyzed += 1;
                }
                summary.tokens_in += chunk_res.tokens_in;
                summary.tokens_out += chunk_res.tokens_out;
                summary.tokens_cached += chunk_res.tokens_cached;
                summary
                    .dismissed_concerns
                    .extend(chunk_res.dismissed_concerns);
                for candidate in chunk_res.candidate_bugs {
                    if !is_in_run_duplicate_input(&candidate_bugs, &candidate) {
                        candidate_bugs.push(candidate);
                    }
                }
            }
            Err(e) => {
                summary.chunks_failed += 1;
                if let Some(wf) = e.downcast_ref::<crate::workflow::WorkflowFailure>() {
                    summary.tokens_in += wf.outcome.tokens_in as usize;
                    summary.tokens_out += wf.outcome.tokens_out as usize;
                    summary.tokens_cached += wf.outcome.tokens_cached as usize;
                }
            }
        }
    }

    #[cfg(feature = "server")]
    if let Some(db) = submit_db {
        submit_scan_candidates(
            db,
            provider,
            &options,
            settings,
            &target_sha,
            candidate_bugs,
            &mut summary,
            progress,
        )
        .await;
        return Ok(summary);
    }

    verify_scan_candidates_standalone(
        provider.as_ref(),
        shared_tools,
        &options,
        &file_invariants_by_file,
        candidate_bugs,
        &mut summary,
        progress,
    )
    .await;

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_scan_path_valid_and_invalid() {
        assert_eq!(
            sanitize_scan_path(Path::new("./fs/smb/server/")).unwrap(),
            "fs/smb/server"
        );
        assert_eq!(sanitize_scan_path(Path::new(".")).unwrap(), ".");
        assert!(sanitize_scan_path(Path::new("")).is_err());
        assert!(sanitize_scan_path(Path::new("--option")).is_err());
        assert!(sanitize_scan_path(Path::new("fs/bad\npath.c")).is_err());
        assert!(sanitize_scan_path(Path::new("/etc/passwd")).is_err());
        assert!(sanitize_scan_path(Path::new("fs/../../etc")).is_err());
    }

    #[test]
    fn test_should_ignore_scan_file_respects_defaults_and_explicit_targets() {
        let explicit = vec!["fs/smb".to_string()];
        assert!(should_ignore_scan_file(
            "Documentation/filesystems/smb.c",
            &explicit,
            &[]
        ));
        assert!(should_ignore_scan_file(
            "tools/testing/selftests/net/test.c",
            &explicit,
            &[]
        ));
        assert!(should_ignore_scan_file(
            "fs/smb/server/ksmbd.mod.c",
            &explicit,
            &[]
        ));
        assert!(!should_ignore_scan_file(
            "fs/smb/server/smb2pdu.c",
            &explicit,
            &[]
        ));

        // Explicit targeting of a noisy path or .mod.c file allows it through.
        let explicit_selftest = vec!["tools/testing/selftests/net/test.c".to_string()];
        assert!(!should_ignore_scan_file(
            "tools/testing/selftests/net/test.c",
            &explicit_selftest,
            &[]
        ));
        let explicit_mod = vec!["fs/smb/server/ksmbd.mod.c".to_string()];
        assert!(!should_ignore_scan_file(
            "fs/smb/server/ksmbd.mod.c",
            &explicit_mod,
            &[]
        ));
    }

    #[test]
    fn test_extract_c_functions_includes_ifdef_blocks_and_risk_signals() {
        let src = r#"
struct my_dev {
    int count;
};

static void helper_free(struct my_dev *dev)
{
    kfree(dev);
}

#ifdef CONFIG_PM_SLEEP
static int my_dev_suspend(struct my_dev *dev, const char __user *ubuf)
{
    char buf[16];
    mutex_lock(&dev_lock);
    if (copy_from_user(buf, ubuf, sizeof(buf))) {
        helper_free(dev);
        goto err_unlock;
    }
    mutex_unlock(&dev_lock);
    return 0;
err_unlock:
    mutex_unlock(&dev_lock);
    return -EFAULT;
}
#endif
"#;
        let fns = extract_functions("drivers/misc/my_dev.c", src);
        assert_eq!(fns.len(), 2);
        assert_eq!(fns[0].name, "helper_free");
        assert_eq!(fns[0].risk_signals, vec!["alloc_free"]);

        assert_eq!(fns[1].name, "my_dev_suspend");
        assert!(fns[1].called_functions.contains(&"helper_free".to_string()));
        assert!(fns[1].risk_signals.contains(&"user_copy".to_string()));
        assert!(fns[1].risk_signals.contains(&"locking".to_string()));
        assert!(fns[1].risk_signals.contains(&"error_goto".to_string()));
    }

    #[test]
    fn test_chunk_source_file_single_and_multi_chunk() {
        let small_src = "static int foo(void)\n{\n\treturn 0;\n}\n";
        let chunks = chunk_source_file("net/core/foo.c", small_src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_index, 1);
        assert_eq!(chunks[0].total_chunks, 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 4);
        assert!(chunks[0].file_function_index.contains("foo()"));
        assert_eq!(chunks[0].raw_source_lines(), small_src);

        // Construct a large file > 800 lines with 4 functions of ~300 lines each.
        let mut large_src = String::from("struct shared_ctx {\n\tint state;\n};\n\n");
        for fn_idx in 1..=4 {
            large_src.push_str(&format!(
                "static int func_{fn_idx}(struct shared_ctx *ctx)\n{{\n"
            ));
            if fn_idx == 2 {
                large_src.push_str("\tfunc_1(ctx);\n");
            }
            for line_i in 0..290 {
                large_src.push_str(&format!("\tctx->state += {line_i};\n"));
            }
            large_src.push_str("\treturn 0;\n}\n\n");
        }

        let multi_chunks = chunk_source_file("drivers/net/large.c", &large_src);
        assert!(
            multi_chunks.len() >= 2,
            "Expected large file to be split into >= 2 chunks, got {}",
            multi_chunks.len()
        );
        // func_1 and func_2 have call affinity and together are ~600 lines, so they stay in chunk 1.
        let c1_names: Vec<&str> = multi_chunks[0]
            .functions
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(c1_names, vec!["func_1", "func_2"]);
        // Later chunks include the file-scope preamble and strip synthetic separator banners.
        assert!(
            multi_chunks[1]
                .numbered_source
                .contains("File-scope preamble")
        );
        assert!(multi_chunks[1].numbered_source.contains("see chunk lines"));
        let raw_c2 = multi_chunks[1].raw_source_lines();
        assert!(raw_c2.contains("struct shared_ctx"));
        assert!(!raw_c2.contains("File-scope preamble"));
        assert!(!raw_c2.contains("see chunk lines"));
    }

    #[test]
    fn test_build_function_windows_same_line_and_trailing_cap_and_rust_braces() {
        let fns = vec![
            FunctionSpan {
                name: "stub_a".to_string(),
                signature: "static inline void stub_a(void)".to_string(),
                start_line: 10,
                end_line: 10,
                ..Default::default()
            },
            FunctionSpan {
                name: "stub_b".to_string(),
                signature: "static inline void stub_b(void)".to_string(),
                start_line: 10,
                end_line: 10,
                ..Default::default()
            },
            FunctionSpan {
                name: "large_fn".to_string(),
                signature: "static int large_fn(void)".to_string(),
                start_line: 12,
                end_line: 650,
                ..Default::default()
            },
        ];
        // Total lines = 1800 (large trailing non-function section after line 650).
        let windows = build_function_windows(1800, &fns);
        for &(start, end) in &windows {
            assert!(
                start <= end,
                "window must not be inverted: ({start}, {end})"
            );
            assert!(
                end - start < MAX_CHUNK_LINES,
                "window ({start}, {end}) exceeds MAX_CHUNK_LINES"
            );
        }
        assert_eq!(windows.last().map(|&(_, end)| end), Some(1800));

        // Verify extract_rust_functions ignores braces inside strings, char literals, and comments.
        let rust_src = r##"
pub fn parse_braces(input: &str) -> usize {
    // Unmatched { in line comment
    /* Unmatched { in block comment */
    let open = '{';
    let raw = r#"unmatched { in raw string"#;
    let s = "unmatched { in normal string";
    input.len() + (open as usize) + raw.len() + s.len()
}

pub fn second_fn() -> u32 {
    42
}
"##;
        let rust_fns = extract_rust_functions(rust_src);
        assert_eq!(rust_fns.len(), 2);
        assert_eq!(rust_fns[0].name, "parse_braces");
        assert_eq!(rust_fns[0].start_line, 2);
        assert_eq!(rust_fns[0].end_line, 9);
        assert_eq!(rust_fns[1].name, "second_fn");
        assert_eq!(rust_fns[1].start_line, 11);
        assert_eq!(rust_fns[1].end_line, 13);
    }

    #[tokio::test]
    async fn test_resolve_targets_and_build_chunks_with_header_prefetch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path();

        let run_git = |args: &[&str]| {
            let out = crate::git_cmd::in_dir(repo).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        run_git(&["init"]);
        run_git(&["config", "user.name", "Test"]);
        run_git(&["config", "user.email", "test@example.com"]);

        std::fs::create_dir_all(repo.join("fs/smb/server"))?;
        std::fs::create_dir_all(repo.join("net/core"))?;
        std::fs::create_dir_all(repo.join("Documentation/filesystems"))?;

        std::fs::write(
            repo.join("fs/smb/server/smb_common.h"),
            "struct ksmbd_work {\n\tint compound_id;\n\tchar *buf;\n};\n",
        )?;
        std::fs::write(
            repo.join("fs/smb/server/smb2pdu.c"),
            "#include \"smb_common.h\"\n\nint smb2_handle(struct ksmbd_work *work)\n{\n\treturn work->compound_id;\n}\n",
        )?;
        std::fs::write(
            repo.join("net/core/dev.c"),
            "int net_dev_init(void) { return 0; }\n",
        )?;
        std::fs::write(
            repo.join("Documentation/filesystems/doc.c"),
            "int doc_example(void) { return 0; }\n",
        )?;

        run_git(&["add", "."]);
        run_git(&["commit", "-m", "initial"]);

        let sha = resolve_scan_revision(repo, Some("HEAD")).await?;
        assert_eq!(sha.len(), 40);

        // Passing "." alongside another valid directory must still scan the whole tree.
        let files = resolve_scan_targets(
            repo,
            &sha,
            &[PathBuf::from("."), PathBuf::from("fs/smb")],
            &[],
            10,
        )
        .await?;
        assert_eq!(
            files,
            vec![
                "fs/smb/server/smb2pdu.c".to_string(),
                "fs/smb/server/smb_common.h".to_string(),
                "net/core/dev.c".to_string(),
            ]
        );

        // Explicit missing path alongside a valid path must error loudly.
        let missing_err = resolve_scan_targets(
            repo,
            &sha,
            &[
                PathBuf::from("fs/smb/server/smb2pdu.c"),
                PathBuf::from("fs/smb/server/does_not_exist.c"),
            ],
            &[],
            10,
        )
        .await
        .unwrap_err();
        assert!(
            missing_err.to_string().contains("does not exist at commit"),
            "unexpected error: {missing_err}"
        );

        let chunks = build_scan_chunks_for_file(repo, &sha, "fs/smb/server/smb2pdu.c").await?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_id(), "fs/smb/server/smb2pdu.c:1-6");
        assert_eq!(chunks[0].functions.len(), 1);
        assert_eq!(chunks[0].functions[0].name, "smb2_handle");
        assert!(
            chunks[0].prefetched_headers.contains("struct ksmbd_work"),
            "Expected prefetched header definition for struct ksmbd_work, got: {}",
            chunks[0].prefetched_headers
        );
        Ok(())
    }

    #[test]
    fn test_verified_scan_bug_and_in_run_dedup() {
        let standalone = crate::workflows::linux_bug::StandaloneVerifiedBug {
            canonical_title: "smb: server: memory leak in parse_durable_handle_context()"
                .to_string(),
            canonical_description: "Trigger / Preconditions: x\nFailure Mechanism: y\nImpact: z"
                .to_string(),
            severity: crate::db::Severity::High,
            severity_explanation: "Leaks fp".to_string(),
            subsystems: vec![crate::db::AttributedSubsystem::from_maintainers(
                "SMB3 SERVER (KSMBD)",
            )],
            source_files: vec!["fs/smb/server/smb2pdu.c".to_string()],
            locations: Some(serde_json::json!([{
                "file": "fs/smb/server/smb2pdu.c",
                "line": 412,
                "function_or_symbol": "parse_durable_handle_context"
            }])),
            introduced_in_commit: Some(
                "1234567890ab (\"smb: server: add durable handle\") [v6.6]".to_string(),
            ),
            verified_on_sha: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        };

        let bug1 = VerifiedScanBug::from_standalone(standalone);
        assert_eq!(bug1.severity, "High");

        let bug_same_fn = VerifiedScanBug {
            canonical_title: "smb: server: fp memory leak in parse_durable_handle_context()"
                .to_string(),
            ..bug1.clone()
        };
        assert!(is_in_run_duplicate(
            std::slice::from_ref(&bug1),
            &bug_same_fn
        ));

        let bug_different = VerifiedScanBug {
            canonical_title: "smb: server: use-after-free in smb2_close()".to_string(),
            locations: serde_json::json!([{
                "file": "fs/smb/server/smb2pdu.c",
                "line": 900,
                "function_or_symbol": "smb2_close"
            }]),
            ..bug1.clone()
        };
        assert!(!is_in_run_duplicate(
            std::slice::from_ref(&bug1),
            &bug_different
        ));

        let input1 = crate::api::BugInput {
            problem: bug1.canonical_title.clone(),
            reasoning: bug1.canonical_description.clone(),
            locations: Some(bug1.locations.clone()),
            subsystems: Vec::new(),
            source_files: bug1.source_files.clone(),
            commit_sha: Some(bug1.verified_on_sha.clone()),
            patchset_id: None,
            patch_id: None,
            baseline_sha: Some(bug1.verified_on_sha.clone()),
            review_id: None,
            min_severity: None,
        };
        let input_same_fn = crate::api::BugInput {
            problem: bug_same_fn.canonical_title.clone(),
            ..input1.clone()
        };
        assert!(is_in_run_duplicate_input(
            std::slice::from_ref(&input1),
            &input_same_fn
        ));

        // Two distinct defects inside the exact same function must NOT be collapsed.
        let sve_bug_a = VerifiedScanBug {
            canonical_title:
                "arm64/ptrace: wrong task passed to task_user_regset_view in sve_set_common()"
                    .to_string(),
            locations: serde_json::json!([{
                "file": "arch/arm64/kernel/ptrace.c",
                "line": 915,
                "function_or_symbol": "sve_set_common"
            }]),
            ..bug1.clone()
        };
        let sve_bug_b = VerifiedScanBug {
            canonical_title: "arm64/ptrace: missing sve_free on fpsimd path in sve_set_common()"
                .to_string(),
            locations: serde_json::json!([{
                "file": "arch/arm64/kernel/ptrace.c",
                "line": 948,
                "function_or_symbol": "sve_set_common"
            }]),
            ..bug1.clone()
        };
        assert!(!is_in_run_duplicate(
            std::slice::from_ref(&sve_bug_a),
            &sve_bug_b
        ));

        let kho_a = crate::api::BugInput {
            problem: "kho: unchecked xa_erase return value in kho_vmalloc_unpreserve_chunk()"
                .to_string(),
            locations: Some(serde_json::json!([{
                "file": "kernel/kexec_handover.c",
                "line": 980,
                "function_or_symbol": "kho_vmalloc_unpreserve_chunk"
            }])),
            ..input1.clone()
        };
        let kho_b = crate::api::BugInput {
            problem: "kho: memory leak of chunk struct in kho_vmalloc_unpreserve_chunk()"
                .to_string(),
            locations: Some(serde_json::json!([{
                "file": "kernel/kexec_handover.c",
                "line": 992,
                "function_or_symbol": "kho_vmalloc_unpreserve_chunk"
            }])),
            ..input1.clone()
        };
        assert!(!is_in_run_duplicate_input(
            std::slice::from_ref(&kho_a),
            &kho_b
        ));

        let shared_alloc_loc = serde_json::json!({
            "file": "arch/arm64/kvm/nested.c",
            "line": 1215,
            "function_or_symbol": "kvm_vcpu_allocate_vncr_tlb"
        });
        let vncr_fp_cand = crate::api::BugInput {
            problem: "KVM: arm64: nv: NULL vncr_tlb dereference from kvm_vcpu_load_hw_mmu()"
                .to_string(),
            locations: Some(serde_json::json!([
                {"file": "arch/arm64/kvm/nested.c", "line": 786, "function_or_symbol": "kvm_vcpu_load_hw_mmu"},
                shared_alloc_loc.clone()
            ])),
            ..input1.clone()
        };
        let vncr_real_cand = crate::api::BugInput {
            problem: "KVM: arm64: nv: NULL vncr_tlb dereference in invalidate_vncr_va()"
                .to_string(),
            locations: Some(serde_json::json!([
                {"file": "arch/arm64/kvm/nested.c", "line": 929, "function_or_symbol": "invalidate_vncr_va"},
                shared_alloc_loc
            ])),
            ..input1
        };
        assert!(
            !is_in_run_duplicate_input(std::slice::from_ref(&vncr_fp_cand), &vncr_real_cand),
            "distinct target functions sharing a background allocator location must never collide"
        );
    }

    #[test]
    fn test_intra_file_context_extraction() {
        let mut src = String::from("static int early_helper(int v)\n{\n\treturn v + 1;\n}\n\n");
        src.push_str("static int early_caller(void)\n{\n");
        for i in 0..270 {
            src.push_str(&format!("\tint pad_{i} = {i};\n"));
        }
        src.push_str("\treturn late_target(42);\n}\n\n");
        src.push_str("#define MID_MAGIC_MASK 0x7f\n");
        src.push_str("struct mid_item {\n\tint flags;\n};\n\n");
        src.push_str("static const struct item_ops mid_ops = {\n\t.release = late_target,\n};\n\n");
        for i in 0..260 {
            src.push_str(&format!("static int filler_{i}(void) {{ return {i}; }}\n"));
        }
        src.push_str(
            "static int late_target(int x)\n{\n\tstruct mid_item item = { .flags = x & MID_MAGIC_MASK };\n\treturn early_helper(item.flags);\n}\n",
        );

        let chunks = chunk_source_file("drivers/misc/intra.c", &src);
        assert!(chunks.len() >= 2);
        let last_chunk = chunks.last().unwrap();
        assert!(
            last_chunk.functions.iter().any(|f| f.name == "late_target"),
            "last chunk should contain late_target"
        );
        assert!(last_chunk.intra_file_context.contains("MID_MAGIC_MASK"));
        assert!(last_chunk.intra_file_context.contains("struct mid_item"));
        assert!(last_chunk.intra_file_context.contains("early_helper"));
        assert!(last_chunk.intra_file_context.contains("early_caller"));
        assert!(last_chunk.intra_file_context.contains("mid_ops"));
    }

    #[tokio::test]
    async fn test_run_scan_rejects_non_linux_project() -> Result<()> {
        let settings = crate::settings::Settings::new()?;
        for project in [
            crate::project::ProjectId::Sashiko,
            crate::project::ProjectId::Gcc,
        ] {
            let options = ScanOptions {
                project,
                repo_path: PathBuf::from("."),
                paths: vec![PathBuf::from("src")],
                git_ref: None,
                concurrency: 1,
                max_files: 1,
                max_chunks: Some(1),
                exclude: Vec::new(),
                submit_bugs: false,
                min_severity: crate::db::SeverityFloor::Medium,
                prompts: PathBuf::from("projects/sashiko/prompts"),
                custom_prompt: None,
                stages: None,
                effort: None,
            };
            let err = run_scan(options, &settings, None).await.unwrap_err();
            assert!(
                err.to_string()
                    .contains("sashiko scan currently supports only --project linux"),
                "unexpected error for {project}: {err}"
            );
        }
        Ok(())
    }
}
