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
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use tree_sitter::{Node, Parser};

/// Files up to this line count are audited as a single chunk without splitting.
pub const SINGLE_CHUNK_MAX_LINES: usize = 800;

/// Soft target line count when grouping consecutive functions into a chunk.
pub const TARGET_CHUNK_LINES: usize = 600;

/// Hard upper bound when extending a chunk to keep mutually-calling functions together.
pub const MAX_CHUNK_LINES: usize = 900;

/// Line overlap when splitting a single giant function that exceeds `MAX_CHUNK_LINES`.
pub const GIANT_FUNCTION_OVERLAP_LINES: usize = 50;

/// Maximum character budget for pre-fetched external header definitions per chunk.
pub const MAX_HEADER_PREFETCH_CHARS: usize = 30_000;

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FunctionSpan {
    pub name: String,
    pub signature: String,
    /// 1-based inclusive start line.
    pub start_line: usize,
    /// 1-based inclusive end line.
    pub end_line: usize,
    /// Intra-file or external functions directly called by this function.
    pub called_functions: Vec<String>,
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

/// Resolves `--rev` (or the default mainline/HEAD revision) to a full commit SHA.
pub async fn resolve_scan_revision(repo_path: &Path, rev: Option<&str>) -> Result<String> {
    if let Some(user_rev) = rev.map(str::trim).filter(|s| !s.is_empty()) {
        ensure!(
            !user_rev.starts_with('-'),
            "Revision '{}' must not start with '-'",
            user_rev
        );
        let output = crate::git_cmd::in_dir_async(repo_path)
            .args([
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

    #[cfg(feature = "server")]
    if let Some(sha) = crate::workflows::linux_bug::resolve_linus_sha(repo_path).await {
        return Ok(sha);
    }

    for candidate in ["origin/master", "origin/main", "master", "main", "HEAD"] {
        let output = crate::git_cmd::in_dir_async(repo_path)
            .args([
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

    let mut cmd = crate::git_cmd::in_dir_async(repo_path);
    cmd.args(["ls-tree", "-r", "-z", "--name-only", target_sha, "--"]);
    for sp in &sanitized_paths {
        if sp != "." {
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
    let explicitly_requested = explicit_targets.iter().any(|t| {
        t == file || (t != "." && file.starts_with(&format!("{t}/")) && is_noisy_prefix(t))
    });

    if !explicitly_requested && is_noisy_prefix(file) {
        return true;
    }

    for pat in ignore_patterns {
        let p = pat.trim();
        if p.is_empty() {
            continue;
        }
        if explicit_targets.iter().any(|t| t == file) {
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
    let spec = format!("{target_sha}:{file_path}");
    let output = crate::git_cmd::in_dir_async(repo_path)
        .args(["show", &spec])
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

    for chunk in &mut chunks {
        if let Ok(prefetched) = crate::worker::prefetch::prefetch_external_definitions(
            repo_path,
            target_sha,
            file_path,
            chunk.start_line,
            chunk.end_line,
            MAX_HEADER_PREFETCH_CHARS,
        )
        .await
        {
            chunk.prefetched_headers = prefetched;
        }
    }

    Ok(chunks)
}

/// Deterministically partitions `source` into locality-aware `ScanChunk`s.
pub fn chunk_source_file(file_path: &str, source: &str) -> Vec<ScanChunk> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let total_lines = lines.len();
    let functions = extract_functions(file_path, source);
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
            ScanChunk {
                file_path: file_path.to_string(),
                chunk_index: idx + 1,
                total_chunks,
                start_line,
                end_line,
                numbered_source: format_numbered_lines(&lines, start_line, end_line, preamble),
                file_function_index: file_function_index.clone(),
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
            "// ---< lines {}..{} elided; seechunk lines {}..{} below >---\n",
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
    for f in functions {
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
        risk_signals,
    })
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
                "mod_timer",
                "del_timer_sync",
                "timer_delete_sync",
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
                        // Character escape literal like '\'' or '\x7f'
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
    pub repo_path: PathBuf,
    pub paths: Vec<PathBuf>,
    pub git_ref: Option<String>,
    pub concurrency: usize,
    pub max_files: usize,
    pub max_chunks: Option<usize>,
    pub exclude: Vec<String>,
    pub submit_bugs: bool,
    pub prompts: PathBuf,
    pub custom_prompt: Option<String>,
    pub stages: Option<Vec<String>>,
    pub effort: Option<crate::ai::EffortMode>,
}

/// A verified `High` or `Critical` defect discovered by `sashiko scan`
/// and processed through the `linux_bug` verification, origin-tracing,
/// and severity calibration pipeline.
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
    pub files_scanned: usize,
    pub chunks_total: usize,
    pub chunks_analyzed: usize,
    pub chunks_early_exited: usize,
    pub chunks_failed: usize,
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

fn is_title_and_locations_duplicate(
    existing_title: &str,
    existing_locations: &serde_json::Value,
    cand_title_norm: &str,
    cand_tokens: &BTreeSet<String>,
    cand_syms: &BTreeSet<(String, String)>,
) -> bool {
    if existing_title.trim().to_ascii_lowercase() == cand_title_norm {
        return true;
    }
    let existing_syms = extract_location_symbols(existing_locations);
    let shares_file_and_symbol = !cand_syms.is_empty() && !cand_syms.is_disjoint(&existing_syms);
    if shares_file_and_symbol && !cand_tokens.is_empty() {
        let existing_tokens = normalize_title_tokens(existing_title);
        let intersection = cand_tokens.intersection(&existing_tokens).count();
        let union = cand_tokens.union(&existing_tokens).count();
        if union > 0 && (intersection as f64 / union as f64) >= 0.45 {
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
    let cand_tokens = normalize_title_tokens(&candidate.problem);
    let empty_locs = serde_json::json!([]);
    let cand_locs = candidate.locations.as_ref().unwrap_or(&empty_locs);
    let cand_syms = extract_location_symbols(cand_locs);

    existing.iter().any(|bug| {
        let existing_locs = bug.locations.as_ref().unwrap_or(&empty_locs);
        is_title_and_locations_duplicate(
            &bug.problem,
            existing_locs,
            &cand_title_norm,
            &cand_tokens,
            &cand_syms,
        )
    })
}

/// Checks whether `candidate` is an in-run duplicate of any finding in `existing`.
pub fn is_in_run_duplicate(existing: &[VerifiedScanBug], candidate: &VerifiedScanBug) -> bool {
    let cand_title_norm = candidate.canonical_title.trim().to_ascii_lowercase();
    let cand_tokens = normalize_title_tokens(&candidate.canonical_title);
    let cand_syms = extract_location_symbols(&candidate.locations);

    existing.iter().any(|bug| {
        is_title_and_locations_duplicate(
            &bug.canonical_title,
            &bug.locations,
            &cand_title_norm,
            &cand_tokens,
            &cand_syms,
        )
    })
}

struct ChunkScanResult {
    chunk: ScanChunk,
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
    repo_path: &Path,
    prompts_dir: &Path,
    provider: std::sync::Arc<dyn crate::ai::AiProvider>,
    maintainers: Option<std::sync::Arc<crate::maintainers::MaintainersIndex>>,
    max_interactions: usize,
    temperature: f32,
    manual_stages: Option<Vec<String>>,
    custom_prompt: Option<String>,
    effort: Option<crate::ai::EffortMode>,
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

    let mut tb =
        crate::toolbox::ToolBox::new(repo_path.to_path_buf(), Some(prompts_dir.to_path_buf()));
    tb.set_virtual_head(target_sha.to_string());
    tb.set_active_patch_files(vec![chunk.file_path.clone()]);
    let tools = std::sync::Arc::new(tb);

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
        prefetched_context: chunk.prefetched_headers.clone(),
        maintainers_hint,
        manual_stages,
        custom_prompt,
        effort,
        ..Default::default()
    };

    let workflow = crate::workflows::linux_scan::build_linux_scan_workflow_with_effort(
        max_interactions,
        temperature,
        effort,
    );
    let env = crate::workflow::WorkflowEnv {
        provider,
        tools,
        base_dir: prompts_dir,
        context_tag: Some(format!("[scan:{}] ", chunk_id)),
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

    let early_exited =
        state.planned_stages.is_empty() && state.manual_stages.as_ref().is_none_or(Vec::is_empty);
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
        chunk,
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

/// Executes an active scan across the requested files/directories in `options`.
pub async fn run_scan(
    options: ScanOptions,
    settings: &crate::settings::Settings,
    progress: Option<&ScanProgressCallback<'_>>,
) -> Result<ScanSummary> {
    #[cfg(not(feature = "server"))]
    if options.submit_bugs {
        bail!("--submit-bugs requires a sashiko binary built with the 'server' feature enabled");
    }

    #[cfg(feature = "server")]
    let submit_db = if options.submit_bugs {
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
        Some(db)
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

    let mut all_chunks = Vec::new();
    for file in &files {
        let mut file_chunks =
            build_scan_chunks_for_file(&options.repo_path, &target_sha, file).await?;
        all_chunks.append(&mut file_chunks);
        if let Some(limit) = options.max_chunks
            && all_chunks.len() >= limit
        {
            all_chunks.truncate(limit);
            break;
        }
    }

    emit_scan_progress(
        progress,
        ScanProgressEvent::ResolvedTargets {
            target_sha: target_sha.clone(),
            files_count: files.len(),
            chunks_count: all_chunks.len(),
        },
    );

    let maintainers = if let Some(idx) = crate::maintainers::get_global_maintainers() {
        Some(idx)
    } else {
        let repo_for_maintainers = options.repo_path.clone();
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
    };

    let raw_provider = crate::ai::create_provider_cached(&settings.ai, None)
        .await
        .context("Failed to create AI provider for scan")?;
    let logged_provider: std::sync::Arc<dyn crate::ai::AiProvider> = if settings.ai.log_turns {
        std::sync::Arc::new(crate::ai::logging_provider::LoggingProvider::new(
            raw_provider,
        ))
    } else {
        raw_provider
    };
    let llm_concurrency = crate::ai::concurrency_limited_provider::llm_permits(options.concurrency);
    let llm_semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(llm_concurrency));
    let limited_provider: std::sync::Arc<dyn crate::ai::AiProvider> = std::sync::Arc::new(
        crate::ai::concurrency_limited_provider::ConcurrencyLimitedProvider::new(
            logged_provider,
            llm_semaphore,
        ),
    );
    let quota = std::sync::Arc::new(crate::ai::quota::QuotaManager::new());
    let provider: std::sync::Arc<dyn crate::ai::AiProvider> = std::sync::Arc::new(
        crate::ai::backoff_provider::BackoffProvider::new(limited_provider, quota, None),
    );

    let chunk_semaphore =
        std::sync::Arc::new(tokio::sync::Semaphore::new(options.concurrency.max(1)));
    let max_interactions = settings.ai.max_interactions;
    let temperature = settings.ai.temperature;
    let effort = options.effort.or_else(|| settings.effective_effort());

    let chunks_total = all_chunks.len();
    let chunk_futures = all_chunks.into_iter().map(|chunk| {
        let sem = chunk_semaphore.clone();
        let target_sha_ref = &target_sha;
        let repo_ref = options.repo_path.as_path();
        let prompts_ref = options.prompts.as_path();
        let prov = provider.clone();
        let mindex = maintainers.clone();
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
                repo_ref,
                prompts_ref,
                prov,
                mindex,
                max_interactions,
                temperature,
                manual_stages,
                custom_prompt,
                effort,
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

    let results = futures::future::join_all(chunk_futures).await;

    let mut summary = ScanSummary {
        target_sha: target_sha.clone(),
        files_scanned: files.len(),
        chunks_total,
        ..Default::default()
    };

    let mut candidate_bugs: Vec<crate::api::BugInput> = Vec::new();
    for res in results {
        match res {
            Ok(chunk_res) => {
                let _ = &chunk_res.chunk;
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
        if !candidate_bugs.is_empty() {
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

            let bug_worker = crate::worker::bug_worker::BugWorker::new(
                db.clone(),
                accounting_provider.clone(),
                options.repo_path.to_string_lossy().to_string(),
            )
            .with_prompts_dir(options.prompts.clone())
            .with_project(crate::project::ProjectId::Linux)
            .with_settings(settings.linux_bug.clone());

            for input in candidate_bugs {
                let fallback_input = input.clone();
                match bug_worker
                    .submit_and_process_bug(&attributed_db, input)
                    .await
                {
                    Ok(Some(crate::api::BugOutcome::NewlyDiscovered { bug: saved })) => {
                        if matches!(
                            saved.severity(),
                            crate::db::Severity::High | crate::db::Severity::Critical
                        ) {
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
                                &target_sha,
                                Some(saved.bugid.clone()),
                                None,
                            );
                            if !is_in_run_duplicate(&summary.findings, &verified) {
                                summary.findings.push(verified);
                            }
                        }
                    }
                    Ok(Some(crate::api::BugOutcome::Duplicate { existing_bug, .. })) => {
                        if existing_bug.lifecycle_status == crate::db::BugLifecycleStatus::Open
                            && matches!(
                                existing_bug.severity(),
                                crate::db::Severity::High | crate::db::Severity::Critical
                            )
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
                                &target_sha,
                                None,
                                Some(existing_bug.bugid.clone()),
                            );
                            if !is_in_run_duplicate(&summary.findings, &verified) {
                                summary.findings.push(verified);
                            }
                        }
                    }
                    Ok(Some(crate::api::BugOutcome::Discarded { reason, .. })) => {
                        summary.dismissed_concerns.push(serde_json::json!({
                            "origin": "linux_bug_verification",
                            "description": fallback_input.problem,
                            "reasoning": reason,
                        }));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            "Failed to process and submit scan finding '{}' to bug repository: {}",
                            fallback_input.problem,
                            e
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
        return Ok(summary);
    }

    let mut bug_tb =
        crate::toolbox::ToolBox::new(options.repo_path.clone(), Some(options.prompts.clone()));
    bug_tb.set_virtual_head(target_sha.clone());
    let bug_tools = std::sync::Arc::new(bug_tb);

    for input in candidate_bugs {
        let problem = input.problem.clone();
        match crate::workflows::linux_bug::run_standalone_bug_pipeline_for_project(
            provider.as_ref(),
            Some(bug_tools.clone()),
            input,
            Some("[scan:linux_bug] "),
            crate::project::ProjectId::Linux,
        )
        .await
        {
            Ok(crate::workflows::linux_bug::StandaloneBugOutcome::Verified(bug)) => {
                summary.tokens_in += bug.tokens_in;
                summary.tokens_out += bug.tokens_out;
                summary.tokens_cached += bug.tokens_cached;
                let verified = VerifiedScanBug::from_standalone(*bug);
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
                summary.dismissed_concerns.push(serde_json::json!({
                    "origin": "linux_bug_verification",
                    "description": problem,
                    "reasoning": reason,
                }));
            }
            Err(e) => {
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
                tracing::warn!(
                    "Failed to run linux_bug pipeline for scan candidate '{}': {}",
                    problem,
                    e
                );
            }
        }
    }

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
        assert!(!should_ignore_scan_file(
            "fs/smb/server/smb2pdu.c",
            &explicit,
            &[]
        ));

        // Explicit targeting of a noisy path allows it through.
        let explicit_selftest = vec!["tools/testing/selftests/net/test.c".to_string()];
        assert!(!should_ignore_scan_file(
            "tools/testing/selftests/net/test.c",
            &explicit_selftest,
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
        // Later chunks include the file-scope preamble.
        assert!(
            multi_chunks[1]
                .numbered_source
                .contains("File-scope preamble")
        );
    }

    #[test]
    fn test_build_function_windows_same_line_and_trailing_cap_and_rust_braces() {
        let fns = vec![
            FunctionSpan {
                name: "stub_a".to_string(),
                signature: "static inline void stub_a(void)".to_string(),
                start_line: 10,
                end_line: 10,
                called_functions: vec![],
                risk_signals: vec![],
            },
            FunctionSpan {
                name: "stub_b".to_string(),
                signature: "static inline void stub_b(void)".to_string(),
                start_line: 10,
                end_line: 10,
                called_functions: vec![],
                risk_signals: vec![],
            },
            FunctionSpan {
                name: "large_fn".to_string(),
                signature: "static int large_fn(void)".to_string(),
                start_line: 12,
                end_line: 650,
                called_functions: vec![],
                risk_signals: vec![],
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
            repo.join("Documentation/filesystems/doc.c"),
            "int doc_example(void) { return 0; }\n",
        )?;

        run_git(&["add", "."]);
        run_git(&["commit", "-m", "initial"]);

        let sha = resolve_scan_revision(repo, Some("HEAD")).await?;
        assert_eq!(sha.len(), 40);

        let files = resolve_scan_targets(repo, &sha, &[PathBuf::from(".")], &[], 10).await?;
        assert_eq!(
            files,
            vec![
                "fs/smb/server/smb2pdu.c".to_string(),
                "fs/smb/server/smb_common.h".to_string()
            ]
        );

        let chunks = build_scan_chunks_for_file(repo, &sha, "fs/smb/server/smb2pdu.c").await?;
        assert_eq!(chunks.len(), 1);
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
        };
        let input_same_fn = crate::api::BugInput {
            problem: bug_same_fn.canonical_title.clone(),
            ..input1.clone()
        };
        assert!(is_in_run_duplicate_input(
            std::slice::from_ref(&input1),
            &input_same_fn
        ));
    }
}
