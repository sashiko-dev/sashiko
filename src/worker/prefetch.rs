#![allow(clippy::type_complexity)]

use anyhow::{Context, Result, anyhow, ensure};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tree_sitter::{Node, Parser, Point};

/// Parses a unified diff and returns a map of filename -> list of modified line ranges.
/// Line numbers are 0-based to align with Tree-sitter's Point API.
pub fn parse_diff_ranges(diff: &str) -> HashMap<String, Vec<(usize, usize)>> {
    let mut files = HashMap::new();
    let mut current_file = None;

    let chunk_header_re = Regex::new(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@").unwrap();
    for line in diff.lines() {
        if let Some(fname) = line.strip_prefix("+++ b/") {
            let fname = fname.to_string();
            current_file = Some(fname.clone());
            files.entry(fname).or_insert_with(Vec::new);
        } else if line.starts_with("+++ ") || line.starts_with("diff --git ") {
            // Deleted files have no post-image and must not inherit another file's hunks.
            current_file = None;
        } else if line.starts_with("@@")
            && let Some(fname) = &current_file
            && let Some(caps) = chunk_header_re.captures(line)
        {
            let start: usize = caps
                .get(1)
                .map(|m| m.as_str().parse().unwrap_or(1))
                .unwrap_or(1);
            let count: usize = caps
                .get(2)
                .map(|m| m.as_str().parse().unwrap_or(1))
                .unwrap_or(1);
            if count > 0 {
                // Convert to 0-based indices for tree-sitter
                let start_0 = start.saturating_sub(1);
                let end_0 = start_0 + count.saturating_sub(1);
                files.get_mut(fname).unwrap().push((start_0, end_0));
            }
        }
    }

    // Merge overlapping/adjacent ranges (within 10 lines)
    for ranges in files.values_mut() {
        ranges.sort_by_key(|r| r.0);
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for r in ranges.iter() {
            if let Some(last) = merged.last_mut() {
                if r.0 <= last.1 + 10 {
                    last.1 = std::cmp::max(last.1, r.1);
                } else {
                    merged.push(*r);
                }
            } else {
                merged.push(*r);
            }
        }
        *ranges = merged;
    }

    files
}

const MAX_PREFETCH_CHARS: usize = 200000;

type LineRangeMap = BTreeMap<PathBuf, BTreeSet<(usize, usize)>>;

fn add_range(map: &mut LineRangeMap, path: PathBuf, start: usize, end: usize) {
    map.entry(path).or_default().insert((start, end));
}

/// Immutable Git revision with a blob cache scoped to this repository and commit.
struct SourceSnapshot<'a> {
    repository: &'a Path,
    sha: String,
    blobs: HashMap<PathBuf, Arc<str>>,
}

impl<'a> SourceSnapshot<'a> {
    async fn new(repository: &'a Path, target_sha: &str) -> Result<Self> {
        ensure!(
            matches!(target_sha.len(), 40 | 64)
                && target_sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "Prefetch requires a full target commit SHA"
        );
        let output = crate::git_cmd::in_dir_async(repository)
            .args([
                "-c",
                "safe.bareRepository=all",
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{target_sha}^{{commit}}"),
            ])
            .kill_on_drop(true)
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "Cannot resolve prefetch commit {target_sha}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(Self {
            repository,
            sha: String::from_utf8(output.stdout)?.trim().to_string(),
            blobs: HashMap::new(),
        })
    }

    async fn read(&mut self, path: &Path) -> Result<Arc<str>> {
        if let Some(content) = self.blobs.get(path) {
            return Ok(content.clone());
        }
        let object = format!("{}:{}", self.sha, path.display());
        let output = crate::git_cmd::in_dir_async(self.repository)
            .args([
                "-c",
                "safe.bareRepository=all",
                "show",
                "--end-of-options",
                &object,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("Failed to read prefetch blob {object}"))?;
        ensure!(
            output.status.success(),
            "Cannot read prefetch blob {object}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let content: Arc<str> = String::from_utf8(output.stdout)
            .with_context(|| format!("Prefetch blob {object} is not UTF-8"))?
            .into();
        self.blobs.insert(path.to_path_buf(), content.clone());
        Ok(content)
    }
}

/// Gather target-commit source without depending on the physical checkout.
pub async fn prefetch_context(repository: &Path, target_sha: &str, diff: &str) -> Result<String> {
    let mut snapshot = SourceSnapshot::new(repository, target_sha).await?;
    let file_ranges = parse_diff_ranges(diff);
    let mut range_map: LineRangeMap = BTreeMap::new();
    let mut symbols_to_lookup = HashSet::new();
    let mut already_extracted = HashSet::new();
    let mut called_functions = HashSet::new();

    // Phase 1: modified code — find enclosing blocks, types, and called functions.
    for (file, ranges) in &file_ranges {
        if !is_prefetchable_source_file(file) {
            continue;
        }
        let file_path = PathBuf::from(file);
        let content = snapshot.read(&file_path).await?;
        for &(start, end) in ranges {
            let defs = overlapping_definitions(&content, start, end);
            if defs.is_empty() && !file.ends_with(".c") {
                let total_lines = content.lines().count();
                if total_lines > 0 {
                    let blk_start = start.saturating_sub(50);
                    let blk_end =
                        std::cmp::min(end.saturating_add(50), total_lines.saturating_sub(1));
                    add_range(&mut range_map, file_path.clone(), blk_start, blk_end);
                }
            } else {
                for (blk_start, blk_end) in defs {
                    add_range(&mut range_map, file_path.clone(), blk_start, blk_end);
                }
            }
            already_extracted.extend(extract_defined_names(&content, start, end));
            symbols_to_lookup.extend(extract_type_names(&content, start, end));
        }
        called_functions.extend(extract_called_functions(&content, ranges));
    }

    // Remove symbols whose definitions are already in context.
    for sym in &already_extracted {
        symbols_to_lookup.remove(sym);
    }

    // Drop opaque container types.
    let opaque = find_opaque_types(&symbols_to_lookup, &file_ranges, &snapshot);
    for sym in &opaque {
        symbols_to_lookup.remove(sym);
    }

    // Merge called functions *after* opaque filtering — find_opaque_types looks
    // for `struct X *var` declarations, so non-struct names (function calls) would
    // all be falsely classified as opaque and dropped.
    called_functions.retain(|f| !already_extracted.contains(f));
    symbols_to_lookup.extend(called_functions);

    // _ops structs are large vtables (e.g. net_device_ops) — not useful for review.
    symbols_to_lookup.retain(|s| !s.ends_with("_ops"));

    let mut symbols: Vec<String> = symbols_to_lookup.into_iter().collect();
    symbols.sort();
    symbols.truncate(50);

    // Phase 2: look up referenced symbol definitions via git grep + tree-sitter.
    let caller_dirs: HashSet<&str> = file_ranges
        .keys()
        .filter_map(|f| f.rsplit_once('/').map(|(dir, _)| dir))
        .collect();
    let resolved =
        resolve_symbol_definitions(repository, &mut snapshot, &symbols, &caller_dirs, None).await?;
    for (_, (path, start, end)) in resolved {
        add_range(&mut range_map, path, start, end);
    }

    render_range_map_with_budget(&range_map, &mut snapshot, &file_ranges, MAX_PREFETCH_CHARS).await
}

/// Prefetches external struct, enum, macro, and helper definitions referenced within
/// `start_line_1based..=end_line_1based` (1-based inclusive) of `file_path` at `target_sha`,
/// excluding definitions already located inside `file_path`.
pub async fn prefetch_external_definitions(
    repository: &Path,
    target_sha: &str,
    file_path: &str,
    start_line_1based: usize,
    end_line_1based: usize,
    max_chars: usize,
) -> Result<String> {
    let mut blocks = prefetch_external_definitions_for_chunks(
        repository,
        target_sha,
        file_path,
        &[(start_line_1based, end_line_1based)],
        max_chars,
    )
    .await?;
    Ok(blocks.pop().unwrap_or_default())
}

/// Prefetches external struct, enum, macro, and helper definitions for multiple
/// 1-based inclusive `(start_line, end_line)` chunks of `file_path` at `target_sha`,
/// sharing a single [`SourceSnapshot`] and batching `git grep` across all chunks.
pub async fn prefetch_external_definitions_for_chunks(
    repository: &Path,
    target_sha: &str,
    file_path: &str,
    chunk_ranges_1based: &[(usize, usize)],
    max_chars_per_chunk: usize,
) -> Result<Vec<String>> {
    if chunk_ranges_1based.is_empty() {
        return Ok(Vec::new());
    }
    if !file_path.ends_with(".c") && !file_path.ends_with(".h") {
        return Ok(vec![String::new(); chunk_ranges_1based.len()]);
    }

    let mut snapshot = SourceSnapshot::new(repository, target_sha).await?;
    let path_buf = PathBuf::from(file_path);
    let content = snapshot.read(&path_buf).await?;
    let total_lines = content.lines().count();
    if total_lines == 0 {
        return Ok(vec![String::new(); chunk_ranges_1based.len()]);
    }

    let already_extracted = extract_defined_names(&content, 0, total_lines.saturating_sub(1));
    let mut per_chunk_symbols: Vec<Vec<String>> = Vec::with_capacity(chunk_ranges_1based.len());
    let mut per_chunk_ranges: Vec<HashMap<String, Vec<(usize, usize)>>> =
        Vec::with_capacity(chunk_ranges_1based.len());
    let mut union_symbols: BTreeSet<String> = BTreeSet::new();

    for &(start_1, end_1) in chunk_ranges_1based {
        let start_0 = start_1.saturating_sub(1);
        let end_0 = end_1.saturating_sub(1).min(total_lines.saturating_sub(1));

        let mut file_ranges = HashMap::new();
        file_ranges.insert(file_path.to_string(), vec![(start_0, end_0)]);

        let mut symbols_to_lookup = extract_type_names(&content, start_0, end_0);
        let mut called_functions = extract_called_functions(&content, &[(start_0, end_0)]);

        for sym in &already_extracted {
            symbols_to_lookup.remove(sym);
        }

        let opaque = find_opaque_types(&symbols_to_lookup, &file_ranges, &snapshot);
        for sym in &opaque {
            symbols_to_lookup.remove(sym);
        }

        called_functions.retain(|f| !already_extracted.contains(f));
        symbols_to_lookup.extend(called_functions);
        symbols_to_lookup.retain(|s| !s.ends_with("_ops"));

        let mut chunk_syms: Vec<String> = symbols_to_lookup.into_iter().collect();
        chunk_syms.sort();
        chunk_syms.truncate(40);
        for sym in &chunk_syms {
            union_symbols.insert(sym.clone());
        }
        per_chunk_symbols.push(chunk_syms);
        per_chunk_ranges.push(file_ranges);
    }

    let mut caller_dirs = HashSet::new();
    if let Some((dir, _)) = file_path.rsplit_once('/') {
        caller_dirs.insert(dir);
    }
    let all_symbols: Vec<String> = union_symbols.into_iter().collect();
    let resolved = resolve_symbol_definitions(
        repository,
        &mut snapshot,
        &all_symbols,
        &caller_dirs,
        Some(&path_buf),
    )
    .await?;

    let mut outputs = Vec::with_capacity(chunk_ranges_1based.len());
    for (chunk_syms, file_ranges) in per_chunk_symbols
        .into_iter()
        .zip(per_chunk_ranges.into_iter())
    {
        let mut range_map: LineRangeMap = BTreeMap::new();
        for sym in &chunk_syms {
            if let Some((def_path, start, end)) = resolved.get(sym) {
                add_range(&mut range_map, def_path.clone(), *start, *end);
            }
        }
        outputs.push(
            render_range_map_with_budget(
                &range_map,
                &mut snapshot,
                &file_ranges,
                max_chars_per_chunk,
            )
            .await?,
        );
    }

    Ok(outputs)
}

async fn resolve_symbol_definitions(
    repository: &Path,
    snapshot: &mut SourceSnapshot<'_>,
    symbols: &[String],
    caller_dirs: &HashSet<&str>,
    exclude_file: Option<&Path>,
) -> Result<HashMap<String, (PathBuf, usize, usize)>> {
    let mut resolved = HashMap::new();
    if symbols.is_empty() {
        return Ok(resolved);
    }

    const GREP_SYMBOL_BATCH_SIZE: usize = 50;
    let mut candidates: HashMap<String, (Vec<(PathBuf, u64)>, Vec<(PathBuf, u64)>)> =
        HashMap::new();
    let revision_prefix = format!("{}:", snapshot.sha);

    for batch in symbols.chunks(GREP_SYMBOL_BATCH_SIZE) {
        let regex_pattern = format!(
            "^((struct|class|enum|union)\\s+(?:GTY\\s*\\(\\(.*?\\)\\)\\s+)?({0})\\b|#define\\s+({0})\\b|([a-zA-Z_][a-zA-Z0-9_ \\t*:<>]+\\s+)?({0})\\s*\\()",
            batch.join("|")
        );

        let mut cmd = crate::git_cmd::in_dir_async(repository);
        cmd.args(["-c", "safe.bareRepository=all"])
            .arg("grep")
            .arg("--full-name")
            .arg("--no-color")
            .arg("-z")
            .arg("-n")
            .arg("-I")
            .arg("-P")
            .arg("-e")
            .arg(&regex_pattern)
            .arg(&snapshot.sha)
            .arg("--")
            .args(PREFETCH_GREP_PATHSPECS)
            .kill_on_drop(true);

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => return Err(anyhow!("Failed to run git grep: {}", e)),
        };

        if !output.status.success() && output.status.code() != Some(1) {
            // git grep returns exit status 1 if no matches are found, which is not a hard error.
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("git grep failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some(line) = line.strip_prefix(&revision_prefix)
                && let Some((path_str, rest)) = line.split_once('\0')
                && let Some((line_num_str, line_content)) = rest.split_once('\0')
                && let Ok(line_num) = line_num_str.parse::<u64>()
            {
                let path = PathBuf::from(path_str);
                if exclude_file.is_some_and(|ex| ex == path.as_path())
                    || !is_prefetchable_source_file(path_str)
                {
                    continue;
                }

                let rel = path_str;
                let is_priority = rel.starts_with("include/")
                    || caller_dirs
                        .iter()
                        .any(|d| rel.starts_with(d) && rel.as_bytes().get(d.len()) == Some(&b'/'));

                for sym in batch {
                    if line_matches_symbol(line_content, sym) {
                        let (general, priority) = candidates
                            .entry(sym.clone())
                            .or_insert_with(|| (Vec::new(), Vec::new()));

                        if is_priority {
                            if priority.len() < 32 {
                                priority.push((path.clone(), line_num));
                            }
                        } else if general.len() < 32 {
                            general.push((path.clone(), line_num));
                        }
                    }
                }
            }
        }
    }

    for (sym, (general, priority)) in candidates {
        let mut hits = priority;
        hits.extend(general);
        if let Some((path, start, end)) =
            best_definition_range(&sym, &hits, snapshot, caller_dirs).await?
        {
            resolved.insert(sym, (path, start, end));
        }
    }

    Ok(resolved)
}

/// Merge overlapping or adjacent ranges (within `gap` lines).
fn merge_ranges(ranges: &BTreeSet<(usize, usize)>, gap: usize) -> Vec<(usize, usize)> {
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for &(start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 + gap + 1 {
                last.1 = std::cmp::max(last.1, end);
            } else {
                merged.push((start, end));
            }
        } else {
            merged.push((start, end));
        }
    }
    merged
}

/// Render the collected line ranges into the final prefetch context string.
/// Modified files are rendered first (higher priority when nearing budget).
async fn render_range_map_with_budget(
    range_map: &LineRangeMap,
    snapshot: &mut SourceSnapshot<'_>,
    modified_files: &HashMap<String, Vec<(usize, usize)>>,
    max_chars: usize,
) -> Result<String> {
    let mut output = String::new();
    let mut current_chars = 0;

    let modified_paths: HashSet<PathBuf> = modified_files.keys().map(PathBuf::from).collect();

    // Render modified files first, then definition-only files.
    let mut ordered_files: Vec<&PathBuf> = range_map.keys().collect();
    ordered_files.sort_by_key(|p| if modified_paths.contains(*p) { 0 } else { 1 });

    for file_path in ordered_files {
        let Some(ranges) = range_map.get(file_path) else {
            continue;
        };
        let content = snapshot.read(file_path).await?;
        let lines: Vec<&str> = content.lines().collect();
        let relative = file_path.to_string_lossy();

        let merged = merge_ranges(ranges, 3);

        for &(start, end) in &merged {
            let clamped_end = std::cmp::min(end, lines.len().saturating_sub(1));

            let names = extract_defined_names(&content, start, clamped_end);
            if output.is_empty() {
                output.push_str(&format!("Source revision: {}\n\n", snapshot.sha));
                current_chars = output.len();
            }
            let header = if names.len() == 1 {
                let name = names.into_iter().next().unwrap();
                format!("--- {}:{} ({}) ---\n", relative, start + 1, name)
            } else {
                format!("--- {}:{} ---\n", relative, start + 1)
            };

            let block: String = if clamped_end >= start && start < lines.len() {
                lines[start..=clamped_end].join("\n")
            } else {
                String::new()
            };

            if current_chars + header.len() + block.len() + 1 > max_chars {
                output.push_str("\n... (Context prefetch limits reached)\n");
                return Ok(output);
            }

            output.push_str(&header);
            output.push_str(&block);
            output.push('\n');
            current_chars += header.len() + block.len() + 1;
        }
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Tree-sitter helpers
// ---------------------------------------------------------------------------

/// Collect line ranges of all top-level definitions that overlap a diff range.
/// Returns complete, parseable definitions (functions, structs, enums, etc.)
/// rather than walking up to a single enclosing block.
fn overlapping_definitions(
    source_code: &str,
    start_line: usize,
    end_line: usize,
) -> Vec<(usize, usize)> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return vec![];
    }
    let Some(tree) = parser.parse(source_code, None) else {
        return vec![];
    };

    let target_kinds = [
        "function_definition",
        "struct_specifier",
        "enum_specifier",
        "union_specifier",
        "declaration",
        "type_definition",
        "preproc_def",
        "preproc_function_def",
    ];

    // Iterate root children (top-down) rather than walking up from the diff point.
    // Walking up finds only one enclosing block and misses sibling definitions
    // that also overlap the diff range.
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut ranges = Vec::new();
    for child in root.children(&mut cursor) {
        if child.end_position().row < start_line || child.start_position().row > end_line {
            continue;
        }
        if !target_kinds.contains(&child.kind()) {
            continue;
        }
        let blk_start = child.start_position().row;
        let blk_end = child.end_position().row;
        let line_count = blk_end.saturating_sub(blk_start);
        if line_count > 200 {
            let center = (start_line + end_line) / 2;
            ranges.push((
                center.saturating_sub(100),
                std::cmp::min(center + 100, blk_end),
            ));
        } else {
            ranges.push((blk_start, blk_end));
        }
    }
    ranges
}

/// Returns (block_text, symbol_name) for the first overlapping definition.
pub fn extract_enclosing_block(
    source_code: &str,
    start_line: usize,
    end_line: usize,
) -> Option<(String, Option<String>)> {
    let defs = overlapping_definitions(source_code, start_line, end_line);
    let &(blk_start, blk_end) = defs.first()?;
    let lines: Vec<&str> = source_code.lines().collect();
    let clamped_end = std::cmp::min(blk_end, lines.len().saturating_sub(1));
    let text = if clamped_end >= blk_start && blk_start < lines.len() {
        lines[blk_start..=clamped_end].join("\n")
    } else {
        return None;
    };

    let names = extract_defined_names(source_code, blk_start, clamped_end);
    let name = if names.len() == 1 {
        names.into_iter().next()
    } else {
        None
    };
    Some((text, name))
}

// ---------------------------------------------------------------------------
// Git grep + tree-sitter symbol lookup
// ---------------------------------------------------------------------------

const PREFETCH_GREP_PATHSPECS: &[&str] = &[
    "*.c", "*.h", "*.cc", "*.cpp", "*.cxx", "*.C", "*.hh", "*.hpp", "*.tcc", "*.inc", "*.def",
    "*.pd", "*.f90", "*.F90", "*.md",
];

// These directories contain userspace reimplementations of kernel primitives
// (e.g. tools/virtio/ringtest/ has a toy spin_lock) that shadow the real
// definitions and provide no signal for patch review.
fn is_prefetchable_source_file(file: &str) -> bool {
    if is_noisy_tree(file) {
        return false;
    }
    const SOURCE_EXTS: &[&str] = &[
        ".c", ".h", ".cc", ".cpp", ".cxx", ".C", ".hh", ".hpp", ".tcc", ".inc", ".def", ".pd",
        ".f90", ".F90",
    ];
    if SOURCE_EXTS.iter().any(|ext| file.ends_with(ext)) {
        return true;
    }
    // GCC RTL machine description files live under gcc/config/ or gcc/*.md.
    file.starts_with("gcc/") && file.ends_with(".md")
}

fn is_cpp_source_or_header(file: &str) -> bool {
    const CPP_EXTS: &[&str] = &[".cc", ".cpp", ".cxx", ".C", ".hh", ".hpp", ".tcc", ".h"];
    CPP_EXTS.iter().any(|ext| file.ends_with(ext))
}

fn is_c_or_cpp_impl_file(file: &str) -> bool {
    const IMPL_EXTS: &[&str] = &[".c", ".cc", ".cpp", ".cxx", ".C"];
    IMPL_EXTS.iter().any(|ext| file.ends_with(ext))
}

fn is_non_c_dsl_or_table_file(file: &str) -> bool {
    const DSL_EXTS: &[&str] = &[".inc", ".def", ".pd", ".f90", ".F90"];
    DSL_EXTS.iter().any(|ext| file.ends_with(ext))
        || (file.starts_with("gcc/") && file.ends_with(".md"))
}

fn is_noisy_tree(path_str: &str) -> bool {
    const NOISY_PREFIXES: &[&str] = &[
        "tools/",
        "samples/",
        "Documentation/",
        "scripts/",
        "LICENSES/",
        "testsuite/",
    ];
    NOISY_PREFIXES.iter().any(|p| path_str.starts_with(p))
        || path_str.contains("/testsuite/")
        || path_str.ends_with("ChangeLog")
        || path_str.contains("/ChangeLog")
}

fn line_matches_symbol(line: &str, sym: &str) -> bool {
    let bytes = line.as_bytes();
    let sym_bytes = sym.as_bytes();
    let mut i = 0;
    while let Some(pos) = line[i..].find(sym) {
        let start = i + pos;
        let end = start + sym_bytes.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        i = end;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Score a candidate definition block from tree-sitter. Higher is better.
/// 0 means "not actually a definition" (forward decl, parameter name, etc.).
fn score_definition_node(node: Node<'_>, sym: &str, source: &[u8]) -> i32 {
    let kind = node.kind();
    let names_symbol = |field: &str| {
        node.child_by_field_name(field)
            .and_then(|n| n.utf8_text(source).ok())
            .map(|t| t == sym)
            .unwrap_or(false)
    };
    let has_body = node.child_by_field_name("body").is_some();

    match kind {
        "struct_specifier" | "union_specifier" | "enum_specifier" => {
            if !names_symbol("name") {
                return 0;
            }
            if has_body { 100 } else { 0 }
        }
        "function_definition" => {
            let declared = function_name(node, source);
            if declared.as_deref() != Some(sym) {
                return 0;
            }
            if has_body { 90 } else { 0 }
        }
        "preproc_def" | "preproc_function_def" if names_symbol("name") => 70,
        "preproc_def" | "preproc_function_def" => 0,
        "type_definition" if typedef_names_match(node, sym, source) => 80,
        "type_definition" => 0,
        _ => 0,
    }
}

fn function_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cur = node.child_by_field_name("declarator")?;
    loop {
        match cur.kind() {
            "identifier" => return cur.utf8_text(source).ok().map(str::to_string),
            "function_declarator" | "pointer_declarator" | "parenthesized_declarator" => {
                cur = cur.child_by_field_name("declarator")?;
            }
            _ => return None,
        }
    }
}

fn typedef_names_match(node: Node<'_>, sym: &str, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_identifier" && child.utf8_text(source).ok() == Some(sym) {
            return true;
        }
    }
    false
}

/// Pick the highest-scoring definition across all Git grep candidates for `sym`.
/// Total score = definition kind score + proximity score.
async fn best_definition_range(
    sym: &str,
    hits: &[(PathBuf, u64)],
    snapshot: &mut SourceSnapshot<'_>,
    caller_dirs: &HashSet<&str>,
) -> Result<Option<(PathBuf, usize, usize)>> {
    let mut tree_sitter_checked = HashSet::new();
    let mut tree_sitter_matched = HashSet::new();
    let mut best: Option<(i32, PathBuf, usize, usize)> = None;

    for (path, line) in hits {
        if tree_sitter_matched.contains(path) {
            continue;
        }
        let content = snapshot.read(path).await?;
        let rel_path = path.to_string_lossy();
        let Ok(line_usize) = usize::try_from(*line) else {
            continue;
        };
        let line_0 = line_usize.saturating_sub(1);
        let ts_scored = if tree_sitter_checked.insert(path.clone()) {
            score_best_in_file_for_sym(&content, sym)
        } else {
            None
        };
        let (def_score, is_static, start, end) = if let Some(scored) = ts_scored {
            tree_sitter_matched.insert(path.clone());
            scored
        } else if let Some(cpp_scored) = cpp_fallback_definition_range(&rel_path, &content, line_0)
        {
            cpp_scored
        } else if is_non_c_dsl_or_table_file(&rel_path) {
            let total_lines = content.lines().count();
            if total_lines == 0 {
                continue;
            }
            let start = line_0.saturating_sub(5);
            let end = line_0.saturating_add(35).min(total_lines.saturating_sub(1));
            (60, false, start, end)
        } else {
            continue;
        };
        if def_score == 0 {
            continue;
        }
        let score = def_score + proximity_score(&rel_path, is_static, caller_dirs);
        match &best {
            Some((best_score, _, _, _)) if *best_score >= score => {}
            _ => best = Some((score, path.clone(), start, end)),
        }
    }
    Ok(best.map(|(_, p, s, e)| (p, s, e)))
}

/// Fallback definition range scorer for C++ constructs (`class`, derived
/// `struct : Base`, templates, and `Class::method` definitions) that
/// `tree-sitter-c` cannot parse into valid C AST definition nodes.
///
/// Rejects forward declarations (`class Foo;`) and function prototypes
/// (`void foo(int);`) by requiring an opening `{` before any `;`.
fn cpp_fallback_definition_range(
    rel_path: &str,
    content: &str,
    line_0: usize,
) -> Option<(i32, bool, usize, usize)> {
    if !is_cpp_source_or_header(rel_path) {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    let hit_trimmed = lines.get(line_0)?.trim_start();
    let prev_trimmed = line_0
        .checked_sub(1)
        .and_then(|i| lines.get(i))
        .map(|l| l.trim_start())
        .unwrap_or("");

    // Shared `.h` headers are also used by plain C code, where `tree-sitter-c`
    // already scores all valid C definitions and returns `None` only on forward
    // declarations/prototypes or unparsed C++ syntax. Restrict `.h` fallback to
    // lines using C++-specific syntax or `GTY((...))` annotations.
    if rel_path.ends_with(".h") && !line_has_cpp_or_gty_syntax(hit_trimmed, prev_trimmed) {
        return None;
    }

    let (open_row, open_col) = find_open_brace_before_semicolon(&lines, line_0, 15)?;
    let max_end = (line_0 + 200).min(lines.len().saturating_sub(1));
    let end_row = find_matching_close_brace(&lines, open_row, open_col, max_end);
    let start_row = if line_0 > 0 && prev_trimmed.starts_with("template") {
        line_0 - 1
    } else {
        line_0
    };
    let is_type_def = ["class ", "struct ", "union ", "enum "]
        .iter()
        .any(|kw| hit_trimmed.starts_with(kw));
    let score = if is_type_def { 100 } else { 90 };
    let is_static = hit_trimmed.starts_with("static ");
    Some((score, is_static, start_row, end_row))
}

fn line_has_cpp_or_gty_syntax(hit_trimmed: &str, prev_trimmed: &str) -> bool {
    hit_trimmed.starts_with("class ")
        || hit_trimmed.starts_with("template")
        || prev_trimmed.starts_with("template")
        || hit_trimmed.contains("::")
        || hit_trimmed.contains("GTY")
        || ((hit_trimmed.starts_with("struct ") || hit_trimmed.contains(" struct "))
            && hit_trimmed.contains(':'))
}

/// Scans non-comment, non-literal code tokens across `lines[start_row..=end_row]`,
/// starting at `start_col` on `start_row`, invoking `on_byte(row, col, byte)` until
/// it returns `Some(T)`.
fn scan_code_bytes<T>(
    lines: &[&str],
    start_row: usize,
    start_col: usize,
    end_row: usize,
    mut on_byte: impl FnMut(usize, usize, u8) -> Option<T>,
) -> Option<T> {
    let mut in_block_comment = false;
    for (row, line_str) in lines.iter().enumerate().take(end_row + 1).skip(start_row) {
        let bytes = line_str.as_bytes();
        let mut col = if row == start_row { start_col } else { 0 };
        while col < bytes.len() {
            if in_block_comment {
                if col + 1 < bytes.len() && bytes[col] == b'*' && bytes[col + 1] == b'/' {
                    in_block_comment = false;
                    col += 2;
                } else {
                    col += 1;
                }
                continue;
            }
            if col + 1 < bytes.len() && bytes[col] == b'/' && bytes[col + 1] == b'/' {
                break;
            }
            if col + 1 < bytes.len() && bytes[col] == b'/' && bytes[col + 1] == b'*' {
                in_block_comment = true;
                col += 2;
                continue;
            }
            if bytes[col] == b'"' || bytes[col] == b'\'' {
                let quote = bytes[col];
                col += 1;
                while col < bytes.len() {
                    if bytes[col] == b'\\' {
                        col += 2;
                    } else if bytes[col] == quote {
                        col += 1;
                        break;
                    } else {
                        col += 1;
                    }
                }
                continue;
            }
            if let Some(done) = on_byte(row, col, bytes[col]) {
                return Some(done);
            }
            col += 1;
        }
    }
    None
}

fn find_open_brace_before_semicolon(
    lines: &[&str],
    line_0: usize,
    max_lines: usize,
) -> Option<(usize, usize)> {
    let end_row = (line_0 + max_lines).min(lines.len()).saturating_sub(1);
    scan_code_bytes(lines, line_0, 0, end_row, |row, col, b| match b {
        b';' => Some(None),
        b'{' => Some(Some((row, col))),
        _ => None,
    })
    .flatten()
}

fn find_matching_close_brace(
    lines: &[&str],
    open_row: usize,
    open_col: usize,
    max_end: usize,
) -> usize {
    let mut depth: i32 = 0;
    scan_code_bytes(lines, open_row, open_col, max_end, |row, _, b| match b {
        b'{' => {
            depth += 1;
            None
        }
        b'}' => {
            depth -= 1;
            (depth == 0).then_some(row)
        }
        _ => None,
    })
    .unwrap_or(max_end)
}

fn proximity_score(def_path: &str, is_static: bool, caller_dirs: &HashSet<&str>) -> i32 {
    // Static C/C++ implementation definitions outside caller directories are
    // almost certainly wrong-file matches (e.g. mkregtable.c reimplements list_add_tail).
    if is_static && is_c_or_cpp_impl_file(def_path) {
        let def_dir = def_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        if !caller_dirs.contains(def_dir) {
            return -200;
        }
    }

    let def_dir = def_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    if caller_dirs.contains(def_dir) {
        return 50;
    }

    if def_path.starts_with("include/") {
        return 40;
    }

    // Fall back to longest common path prefix with any caller directory.
    let best_common = caller_dirs
        .iter()
        .map(|cd| common_prefix_len(def_dir, cd))
        .max()
        .unwrap_or(0);

    best_common as i32
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

/// Parse `content` and find the highest-scoring definition of `sym`.
/// Returns (score, is_static, start_line, end_line).
fn score_best_in_file_for_sym(content: &str, sym: &str) -> Option<(i32, bool, usize, usize)> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_c::LANGUAGE.into()).ok()?;
    let tree = parser.parse(content, None)?;
    let source = content.as_bytes();

    let mut best: Option<(i32, Node)> = None;
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let score = score_definition_node(node, sym, source);
        if score > 0 {
            match &best {
                Some((b, _)) if *b >= score => {}
                _ => best = Some((score, node)),
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    let (score, node) = best?;
    let is_static = has_static_storage(node, source);
    let start = node.start_position().row;
    let end = node.end_position().row;
    let line_count = end.saturating_sub(start);
    if line_count > 200 {
        Some((score, is_static, start, std::cmp::min(start + 200, end)))
    } else {
        Some((score, is_static, start, end))
    }
}

fn has_static_storage(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "storage_class_specifier"
            && child.utf8_text(source).ok() == Some("static")
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Symbol extraction helpers
// ---------------------------------------------------------------------------

fn is_common_c_word(word: &str) -> bool {
    const COMMON: &[&str] = &[
        "int", "char", "void", "long", "short", "unsigned", "signed", "struct", "union", "enum",
        "typedef", "static", "const", "volatile", "if", "else", "for", "while", "do", "switch",
        "case", "default", "return", "break", "continue", "goto", "sizeof", "true", "false",
        "NULL", "inline", "extern", "register", "auto", "restrict", "u8", "u16", "u32", "u64",
        "s8", "s16", "s32", "s64", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "int8_t",
        "int16_t", "int32_t", "int64_t", "bool", "size_t", "ssize_t", "pid_t", "uid_t", "gid_t",
        "off_t", "ret", "err", "len", "size", "res", "tmp", "val", "ptr", "idx", "out",
    ];
    COMMON.contains(&word)
}

/// Identifies types that are only used as opaque containers in the modified files.
///
/// A type is "opaque" if, across all modified files:
///   - no variable of that type has any member accessed (`var->member`, `var.member`,
///     or `Type::member`), OR
///   - every accessed member name contains "priv"
fn find_opaque_types(
    types: &HashSet<String>,
    file_ranges: &HashMap<String, Vec<(usize, usize)>>,
    snapshot: &SourceSnapshot<'_>,
) -> HashSet<String> {
    if types.is_empty() {
        return HashSet::new();
    }

    let mut type_members: HashMap<&str, HashSet<String>> = HashMap::new();
    for t in types {
        type_members.insert(t, HashSet::new());
    }

    let ptr_decl_re = Regex::new(
        r"\b(?:(?:const|volatile)\s+)*(?:(?:struct|class|union)\s+)?([A-Za-z_]\w*)\s*(?:const\s*)?(?:\*+|&+)\s*([A-Za-z_]\w*)\b",
    )
    .unwrap();
    let val_decl_re = Regex::new(
        r"\b(?:(?:const|volatile)\s+)*(?:(?:struct|class|union)\s+)?([A-Za-z_]\w*)\s+([A-Za-z_]\w*)\s*[;=,)\[(]",
    )
    .unwrap();
    let scoped_re = Regex::new(r"\b([A-Za-z_]\w*)\s*::\s*(\w+)").unwrap();
    let member_access_re = Regex::new(r"\b([A-Za-z_]\w*)\s*(?:->|\.)\s*(\w+)").unwrap();

    for file in file_ranges.keys() {
        let Some(content) = snapshot.blobs.get(Path::new(file)) else {
            continue;
        };
        let content = content.as_ref();

        let mut var_to_types: HashMap<&str, HashSet<&str>> = HashMap::new();
        for decl_re in [&ptr_decl_re, &val_decl_re] {
            for cap in decl_re.captures_iter(content) {
                let Some(type_match) = cap.get(1) else {
                    continue;
                };
                let Some(var_match) = cap.get(2) else {
                    continue;
                };
                let type_name = type_match.as_str();
                let var_name = var_match.as_str();
                if type_members.contains_key(type_name) && !is_common_c_word(var_name) {
                    var_to_types.entry(var_name).or_default().insert(type_name);
                }
            }
        }

        for cap in scoped_re.captures_iter(content) {
            if let Some(members) = type_members.get_mut(&cap[1]) {
                members.insert(cap[2].to_string());
            }
        }

        if !var_to_types.is_empty() {
            for cap in member_access_re.captures_iter(content) {
                if let Some(bound_types) = var_to_types.get(&cap[1]) {
                    let member = &cap[2];
                    for &typ in bound_types {
                        if let Some(members) = type_members.get_mut(typ) {
                            members.insert(member.to_string());
                        }
                    }
                }
            }
        }
    }

    type_members
        .into_iter()
        .filter(|(_, members)| members.is_empty() || members.iter().all(|m| m.contains("priv")))
        .map(|(t, _)| t.to_string())
        .collect()
}

/// Collects the names of all function/struct/enum/union definitions that overlap
/// the given line range.
fn extract_defined_names(source_code: &str, start_line: usize, end_line: usize) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return names;
    }
    let Some(tree) = parser.parse(source_code, None) else {
        return names;
    };
    let source = source_code.as_bytes();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.end_position().row < start_line || child.start_position().row > end_line {
            continue;
        }
        let name = match child.kind() {
            "function_definition" => function_name(child, source),
            "struct_specifier" | "enum_specifier" | "union_specifier" => child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .map(str::to_string),
            _ => None,
        };
        if let Some(n) = name {
            names.insert(n);
        }
    }
    names
}

/// Extracts function call names from modified lines using tree-sitter.
fn extract_called_functions(source_code: &str, diff_ranges: &[(usize, usize)]) -> HashSet<String> {
    let mut funcs = HashSet::new();
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return funcs;
    }
    let Some(tree) = parser.parse(source_code, None) else {
        return funcs;
    };
    let source = source_code.as_bytes();

    fn collect_calls(
        node: Node<'_>,
        source: &[u8],
        diff_ranges: &[(usize, usize)],
        out: &mut HashSet<String>,
    ) {
        if node.kind() == "call_expression" {
            let row = node.start_position().row;
            let in_diff = diff_ranges.iter().any(|&(s, e)| row >= s && row <= e);
            if in_diff && let Some(func) = node.child_by_field_name("function") {
                // Skip field_expression (e.g. obj->method) — only direct calls.
                if func.kind() == "identifier"
                    && let Ok(name) = func.utf8_text(source)
                    && name.len() >= 3
                    && !is_common_c_word(name)
                {
                    out.insert(name.to_string());
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_calls(child, source, diff_ranges, out);
        }
    }

    collect_calls(tree.root_node(), source, diff_ranges, &mut funcs);
    funcs
}

/// Extracts C type names referenced within (and around) the modified line range.
pub fn extract_type_names(
    source_code: &str,
    start_line: usize,
    end_line: usize,
) -> HashSet<String> {
    let mut types = HashSet::new();
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return types;
    }

    let Some(tree) = parser.parse(source_code, None) else {
        return types;
    };
    let root_node = tree.root_node();
    let start_point = Point::new(start_line, 0);
    let end_point = Point::new(end_line, usize::MAX);

    let Some(mut scope) = root_node.descendant_for_point_range(start_point, end_point) else {
        return types;
    };

    let target_kinds = [
        "function_definition",
        "struct_specifier",
        "union_specifier",
        "enum_specifier",
        "type_definition",
    ];
    // Walk up from the diff range to find the enclosing definition. If we hit
    // root (file-scope code), we restrict type extraction to just the diff lines
    // to avoid pulling types from unrelated functions in the same file.
    let hit_root = loop {
        if target_kinds.contains(&scope.kind()) {
            break false;
        }
        match scope.parent() {
            Some(p) => scope = p,
            None => break true,
        }
    };

    fn walk(n: Node<'_>, src: &[u8], out: &mut HashSet<String>, bounds: Option<(usize, usize)>) {
        if let Some((lo, hi)) = bounds
            && (n.end_position().row < lo || n.start_position().row > hi)
        {
            return;
        }
        if n.kind() == "type_identifier"
            && let Ok(text) = n.utf8_text(src)
        {
            let s = text.to_string();
            if s.len() >= 3 && !is_common_c_word(&s) {
                out.insert(s);
            }
        }
        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            walk(child, src, out, bounds);
        }
    }
    // Also restrict for struct/union scopes (huge headers like netdevice.h) and
    // when the parse tree has errors (scope is unreliable, fall back to range).
    let bounds = if hit_root
        || scope.kind() == "struct_specifier"
        || scope.kind() == "union_specifier"
        || scope.has_error()
    {
        Some((start_line, end_line))
    } else {
        None
    };
    walk(scope, source_code.as_bytes(), &mut types, bounds);
    types
}

#[cfg(test)]
mod revision_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_diff_ranges() {
        let diff = r#"
--- a/file.c
+++ b/file.c
@@ -10,2 +10,4 @@
 context
+new line 1
+new line 2
 context
@@ -50,0 +52,1 @@
+new line 3
"#;
        let ranges = parse_diff_ranges(diff);
        assert_eq!(ranges.len(), 1);
        let file_ranges = ranges.get("file.c").unwrap();
        assert_eq!(file_ranges.len(), 2);
        assert_eq!(file_ranges[0], (9, 12)); // 0-based: 10->9, count 4 -> 9,10,11,12 -> end 12
        assert_eq!(file_ranges[1], (51, 51)); // 0-based: 52->51, count 1 -> 51
    }

    #[test]
    fn test_extract_enclosing_block() {
        let source_code = r#"#include <stdio.h>

int main() {
    int a = 1;
    // target line 4 (0-based)
    printf("hello");
    return 0;
}

struct MyStruct {
    int x;
};
"#;
        let (block_main, name_main) = extract_enclosing_block(source_code, 4, 4).unwrap();
        assert!(block_main.starts_with("int main() {"));
        assert!(block_main.ends_with("return 0;\n}"));
        assert_eq!(name_main.as_deref(), Some("main"));

        let (block_struct, name_struct) = extract_enclosing_block(source_code, 10, 10).unwrap();
        assert!(block_struct.starts_with("struct MyStruct"));
        assert_eq!(name_struct.as_deref(), Some("MyStruct"));
    }

    #[test]
    fn test_merge_ranges() {
        let mut ranges = BTreeSet::new();
        ranges.insert((10, 20));
        ranges.insert((22, 30)); // gap of 1 — merges with gap=3
        ranges.insert((50, 60)); // gap of 19 — does not merge
        let merged = merge_ranges(&ranges, 3);
        assert_eq!(merged, vec![(10, 30), (50, 60)]);
    }

    #[test]
    fn test_is_prefetchable_source_file() {
        assert!(is_prefetchable_source_file("mm/page_alloc.c"));
        assert!(is_prefetchable_source_file("gcc/tree-vect-stmts.cc"));
        assert!(is_prefetchable_source_file("gcc/match.pd"));
        assert!(is_prefetchable_source_file("gcc/config/i386/i386.md"));
        assert!(is_prefetchable_source_file("gcc/fortran/resolve.f90"));
        assert!(!is_prefetchable_source_file("README.md"));
        assert!(!is_prefetchable_source_file("Documentation/foo.md"));
        assert!(!is_prefetchable_source_file(
            "gcc/testsuite/gcc.dg/pr12345.c"
        ));
        assert!(!is_prefetchable_source_file("gcc/ChangeLog"));

        assert!(PREFETCH_GREP_PATHSPECS.contains(&"*.pd"));
        assert!(PREFETCH_GREP_PATHSPECS.contains(&"*.f90"));
        assert!(PREFETCH_GREP_PATHSPECS.contains(&"*.F90"));
        assert!(PREFETCH_GREP_PATHSPECS.contains(&"*.md"));
        assert!(is_non_c_dsl_or_table_file("gcc/match.pd"));
        assert!(is_non_c_dsl_or_table_file("gcc/config/i386/i386.md"));
        assert!(is_non_c_dsl_or_table_file("gcc/fortran/resolve.f90"));
        assert!(!is_non_c_dsl_or_table_file("gcc/tree-vect-stmts.cc"));
    }

    #[test]
    fn test_find_opaque_types_single_pass_member_detection() {
        let types: HashSet<String> = [
            "OpaqueHandle",
            "PrivOnlyBox",
            "UsedStruct",
            "UsedByValue",
            "ScopedClass",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let mut file_ranges = HashMap::new();
        file_ranges.insert("gcc/foo.cc".to_string(), vec![(0, 20)]);

        let mut blobs = HashMap::new();
        blobs.insert(
            PathBuf::from("gcc/foo.cc"),
            Arc::<str>::from(
                r#"
void f(const struct OpaqueHandle *handle, struct PrivOnlyBox *box_ptr) {
    struct UsedStruct *item = get_item();
    struct UsedStruct *item = get_other_item();
    UsedByValue local_val = make_val();
    pass_along(handle);
    use_priv(box_ptr->priv_data);
    item->count += 1;
    local_val.field = ScopedClass::static_member;
}
"#,
            ),
        );
        let snapshot = SourceSnapshot {
            repository: Path::new("."),
            sha: "0".repeat(40),
            blobs,
        };

        let opaque = find_opaque_types(&types, &file_ranges, &snapshot);
        assert!(opaque.contains("OpaqueHandle"));
        assert!(opaque.contains("PrivOnlyBox"));
        assert!(!opaque.contains("UsedStruct"));
        assert!(!opaque.contains("UsedByValue"));
        assert!(!opaque.contains("ScopedClass"));
    }
}
