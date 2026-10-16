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
}
