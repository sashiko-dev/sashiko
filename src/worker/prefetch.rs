#![allow(clippy::type_complexity)]

use anyhow::{Result, anyhow};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;
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

use tokio::process::Command;

const MAX_PREFETCH_CHARS: usize = 200000;

type LineRangeMap = BTreeMap<PathBuf, BTreeSet<(usize, usize)>>;

fn add_range(map: &mut LineRangeMap, path: PathBuf, start: usize, end: usize) {
    map.entry(path).or_default().insert((start, end));
}

/// Returns true if the file extension is a C/C++ source or header file.
pub fn is_c_like_source_file(file: &str) -> bool {
    file.ends_with(".c")
        || file.ends_with(".h")
        || file.ends_with(".cpp")
        || file.ends_with(".cc")
        || file.ends_with(".cxx")
        || file.ends_with(".hpp")
        || file.ends_with(".inc")
}

use crate::project::Project;

pub async fn prefetch_context(worktree_path: &Path, diff: &str, project: Project) -> Result<String> {
    let file_ranges = parse_diff_ranges(diff);
    let mut range_map: LineRangeMap = BTreeMap::new();
    let mut symbols_to_lookup = HashSet::new();
    let mut already_extracted = HashSet::new();
    let mut called_functions = HashSet::new();

    // Phase 1: modified code — find enclosing blocks, types, and called functions.
    for (file, ranges) in &file_ranges {
        if !is_c_like_source_file(file) {
            continue;
        }
        let file_path = worktree_path.join(file);
        if !file_path.exists() {
            continue;
        }

        if let Ok(content) = fs::read_to_string(&file_path).await {
            for &(start, end) in ranges {
                let defs = overlapping_definitions(&content, start, end);
                if defs.is_empty() {
                    let line_count = content.lines().count();
                    let ctx_start = start.saturating_sub(30);
                    let ctx_end = std::cmp::min(end + 30, line_count.saturating_sub(1));
                    add_range(&mut range_map, file_path.clone(), ctx_start, ctx_end);
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
    }

    // Remove symbols whose definitions are already in context.
    for sym in &already_extracted {
        symbols_to_lookup.remove(sym);
    }

    // Drop opaque container types.
    let opaque = find_opaque_types(&symbols_to_lookup, &file_ranges, worktree_path, project).await;
    for sym in &opaque {
        symbols_to_lookup.remove(sym);
    }

    // Merge called functions *after* opaque filtering — find_opaque_types looks
    // for `struct X *var` declarations, so non-struct names (function calls) would
    // all be falsely classified as opaque and dropped.
    called_functions.retain(|f| !already_extracted.contains(f));
    symbols_to_lookup.extend(called_functions);

    // _ops structs are large vtables (e.g. net_device_ops) — not useful for review.
    if project == Project::Kernel {
        symbols_to_lookup.retain(|s| !s.ends_with("_ops"));
    }

    let symbols: Vec<String> = symbols_to_lookup.into_iter().take(50).collect();

    // Phase 2: look up referenced symbol definitions via git grep + tree-sitter.
    if !symbols.is_empty() {
        let regex_pattern = if project == Project::Qemu {
            format!(
                r"^((struct|enum|union)\s+({0})\b|#define\s+({0})\b|([a-zA-Z_][a-zA-Z0-9_ \t*]+\s+)?({0})\s*\(|OBJECT_DECLARE_.*?\s*\(\s*({0})\b)",
                symbols.join("|")
            )
        } else {
            format!(
                r"^((struct|enum|union)\s+({0})\b|#define\s+({0})\b|([a-zA-Z_][a-zA-Z0-9_ \t*]+\s+)?({0})\s*\()",
                symbols.join("|")
            )
        };

        let caller_dirs: HashSet<&str> = file_ranges
            .keys()
            .filter_map(|f| f.rsplit_once('/').map(|(dir, _)| dir))
            .collect();

        let mut cmd = Command::new("git");
        cmd.current_dir(worktree_path)
            .arg("grep")
            .arg("-n")
            .arg("-I")
            .arg("-P")
            .arg("-e")
            .arg(&regex_pattern)
            .arg("--")
            .arg("*.c")
            .arg("*.h")
            .arg("*.cpp")
            .arg("*.cc")
            .arg("*.cxx")
            .arg("*.hpp")
            .arg("*.inc");

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => return Err(anyhow!("Failed to run git grep: {}", e)),
        };

        if !output.status.success() {
            // git grep returns exit status 1 if no matches are found, which is not a hard error.
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                return Err(anyhow!("git grep failed: {}", stderr));
            }
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut candidates: HashMap<String, (Vec<(PathBuf, u64)>, Vec<(PathBuf, u64)>)> =
            HashMap::new();

        for line in stdout.lines() {
            if let Some((path_str, rest)) = line.split_once(':')
                && let Some((line_num_str, line_content)) = rest.split_once(':')
                && let Ok(line_num) = line_num_str.parse::<u64>()
            {
                let abs_path = worktree_path.join(path_str);
                let abs_path_str = abs_path.to_string_lossy();
                if is_noisy_tree(&abs_path_str) {
                    continue;
                }

                let rel = path_str;
                let is_priority = rel.starts_with("include/")
                    || caller_dirs
                        .iter()
                        .any(|d| rel.starts_with(d) && rel.as_bytes().get(d.len()) == Some(&b'/'));

                for sym in &symbols {
                    if line_matches_symbol(line_content, sym) {
                        let (general, priority) = candidates
                            .entry(sym.clone())
                            .or_insert_with(|| (Vec::new(), Vec::new()));

                        if is_priority {
                            if priority.len() < 32 {
                                priority.push((abs_path.clone(), line_num));
                            }
                        } else if general.len() < 32 {
                            general.push((abs_path.clone(), line_num));
                        }
                    }
                }
            }
        }

        for (sym, (general, priority)) in candidates {
            let mut hits = priority;
            hits.extend(general);
            if let Some((path, start, end)) =
                best_definition_range(&sym, &hits, worktree_path, &caller_dirs).await
            {
                add_range(&mut range_map, path, start, end);
            }
        }
    }

    render_range_map(&range_map, worktree_path, &file_ranges).await
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
async fn render_range_map(
    range_map: &LineRangeMap,
    worktree_path: &Path,
    modified_files: &HashMap<String, Vec<(usize, usize)>>,
) -> Result<String> {
    let mut output = String::new();
    let mut current_chars = 0;

    let modified_paths: HashSet<PathBuf> = modified_files
        .keys()
        .map(|f| worktree_path.join(f))
        .collect();

    // Render modified files first, then definition-only files.
    let mut ordered_files: Vec<&PathBuf> = range_map.keys().collect();
    ordered_files.sort_by_key(|p| if modified_paths.contains(*p) { 0 } else { 1 });

    for file_path in ordered_files {
        let Some(ranges) = range_map.get(file_path) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(file_path).await else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        let relative = file_path
            .strip_prefix(worktree_path)
            .unwrap_or(file_path)
            .to_string_lossy();

        let merged = merge_ranges(ranges, 3);

        for &(start, end) in &merged {
            let clamped_end = std::cmp::min(end, lines.len().saturating_sub(1));

            let names = extract_defined_names(&content, start, clamped_end);
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

            if current_chars + header.len() + block.len() + 1 > MAX_PREFETCH_CHARS {
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
// Ripgrep + tree-sitter symbol lookup
// ---------------------------------------------------------------------------

// These directories contain userspace reimplementations of kernel primitives
// (e.g. tools/virtio/ringtest/ has a toy spin_lock) that shadow the real
// definitions and provide no signal for patch review.
fn is_noisy_tree(path_str: &str) -> bool {
    const NOISY_PREFIXES: &[&str] = &[
        "/tools/",
        "/samples/",
        "/Documentation/",
        "/scripts/",
        "/LICENSES/",
    ];
    NOISY_PREFIXES.iter().any(|p| path_str.contains(p))
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

/// Pick the highest-scoring definition across all ripgrep candidates for `sym`.
/// Total score = definition kind score + proximity score.
async fn best_definition_range(
    sym: &str,
    hits: &[(PathBuf, u64)],
    worktree_path: &Path,
    caller_dirs: &HashSet<&str>,
) -> Option<(PathBuf, usize, usize)> {
    let mut seen = HashSet::new();
    let mut best: Option<(i32, PathBuf, usize, usize)> = None;

    for (path, _line) in hits {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(content) = fs::read_to_string(path).await else {
            continue;
        };
        let Some((def_score, is_static, start, end)) = score_best_in_file_for_sym(&content, sym)
        else {
            continue;
        };
        if def_score == 0 {
            continue;
        }
        let rel_path = path
            .strip_prefix(worktree_path)
            .unwrap_or(path)
            .to_string_lossy();
        let score = def_score + proximity_score(&rel_path, is_static, caller_dirs);
        match &best {
            Some((best_score, _, _, _)) if *best_score >= score => {}
            _ => best = Some((score, path.clone(), start, end)),
        }
    }
    best.map(|(_, p, s, e)| (p, s, e))
}

fn proximity_score(def_path: &str, is_static: bool, caller_dirs: &HashSet<&str>) -> i32 {
    // Static definitions outside caller directories are almost certainly
    // wrong-file matches (e.g. mkregtable.c reimplements list_add_tail).
    if is_static
        && (def_path.ends_with(".c")
            || def_path.ends_with(".cpp")
            || def_path.ends_with(".cc")
            || def_path.ends_with(".cxx"))
    {
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
///   - no variable of that type is ever dereferenced (`var->member`), OR
///   - every dereferenced member name contains "priv"
async fn find_opaque_types(
    types: &HashSet<String>,
    file_ranges: &HashMap<String, Vec<(usize, usize)>>,
    worktree_path: &Path,
    project: Project,
) -> HashSet<String> {
    if types.is_empty() {
        return HashSet::new();
    }

    let mut type_members: HashMap<&str, HashSet<String>> = HashMap::new();
    for t in types {
        type_members.insert(t, HashSet::new());
    }

    let decl_re = Regex::new(r"struct\s+(\w+)\s+\*(\w+)").unwrap();

    for file in file_ranges.keys() {
        let file_path = worktree_path.join(file);
        let Ok(content) = fs::read_to_string(&file_path).await else {
            continue;
        };

        let mut var_to_type: Vec<(String, String)> = Vec::new();
        for cap in decl_re.captures_iter(&content) {
            let type_name = cap[1].to_string();
            let var_name = cap[2].to_string();
            if type_members.contains_key(type_name.as_str()) {
                var_to_type.push((var_name, type_name));
            }
        }

        for (var, typ) in &var_to_type {
            let pattern = format!(r"{}\s*->\s*(\w+)", regex::escape(var));
            if let Ok(re) = Regex::new(&pattern) {
                for cap in re.captures_iter(&content) {
                    let member = cap[1].to_string();
                    type_members.get_mut(typ.as_str()).unwrap().insert(member);
                }
            }
        }
    }

    type_members
        .into_iter()
        .filter(|(_, members)| {
            if members.is_empty() {
                return true;
            }
            if project == Project::Kernel {
                members.iter().all(|m| m.contains("priv"))
            } else {
                false // For QEMU/LLVM, don't drop types with members just because they look like ->priv
            }
        })
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
    fn test_is_c_like_source_file() {
        assert!(is_c_like_source_file("lib.c"));
        assert!(is_c_like_source_file("header.h"));
        assert!(is_c_like_source_file("code.cpp"));
        assert!(is_c_like_source_file("source.cc"));
        assert!(is_c_like_source_file("impl.cxx"));
        assert!(is_c_like_source_file("decl.hpp"));
        assert!(is_c_like_source_file("table.inc"));
        assert!(!is_c_like_source_file("script.py"));
        assert!(!is_c_like_source_file("doc.md"));
        assert!(!is_c_like_source_file("Makefile"));
    }
}
