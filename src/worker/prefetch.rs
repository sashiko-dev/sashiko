use anyhow::Result;
use regex::Regex;
use std::collections::{HashMap, HashSet};
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

/// Uses Tree-sitter to extract the highest-level meaningful enclosing block (like a function or struct)
/// for a given line range. Returns the source code of that block.
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use std::sync::{Arc, Mutex};

const MAX_PREFETCH_CHARS: usize = 200000;

pub async fn prefetch_context(worktree_path: &Path, diff: &str) -> Result<String> {
    let mut context_blocks = Vec::new();
    let mut current_chars = 0;
    let file_ranges = parse_diff_ranges(diff);
    let mut symbols_to_lookup = HashSet::new();

    for (file, ranges) in file_ranges {
        if !file.ends_with(".c") && !file.ends_with(".h") {
            continue;
        }
        let file_path = worktree_path.join(&file);
        if !file_path.exists() {
            continue;
        }

        if let Ok(content) = fs::read_to_string(&file_path).await {
            let mut extracted_blocks = HashSet::new();
            for (start, end) in ranges {
                if let Some(block) = extract_enclosing_block(&content, start, end) {
                    extracted_blocks.insert(block);
                }
                let ids = extract_type_names(&content, start, end);
                symbols_to_lookup.extend(ids);
            }
            for block in extracted_blocks {
                let block_str = format!("--- Extracted Context from {} ---\n{}\n", file, block);
                if current_chars + block_str.len() > MAX_PREFETCH_CHARS {
                    context_blocks.push("\n... (Context prefetch limits reached)\n".to_string());
                    return Ok(context_blocks.join("\n"));
                }
                current_chars += block_str.len();
                context_blocks.push(block_str);
            }
        }
    }

    let symbols: Vec<String> = symbols_to_lookup.into_iter().take(50).collect();
    if symbols.is_empty() {
        return Ok(context_blocks.join("\n"));
    }

    let regex_pattern = format!(
        "^((struct|enum|union)\\s+({0})\\b|#define\\s+({0})\\b|([a-zA-Z_][a-zA-Z0-9_ \\t*]+\\s+)?({0})\\s*\\()",
        symbols.join("|")
    );

    let matcher = match RegexMatcher::new(&regex_pattern) {
        Ok(m) => m,
        Err(_) => return Ok(context_blocks.join("\n")),
    };

    let search_path = worktree_path.to_path_buf();
    // Map of symbol -> list of (path, line_num) candidate hits.
    type CandidatesMap = HashMap<String, Vec<(PathBuf, u64)>>;
    let candidates: Arc<Mutex<CandidatesMap>> = Arc::new(Mutex::new(HashMap::new()));
    let candidates_clone = Arc::clone(&candidates);

    let _ = tokio::task::spawn_blocking(move || {
        let walker = WalkBuilder::new(&search_path)
            .hidden(false)
            .ignore(true)
            .git_ignore(true)
            .build_parallel();

        walker.run(|| {
            let matcher = matcher.clone();
            let candidates = Arc::clone(&candidates_clone);

            let symbols = symbols.clone();
            Box::new(move |result| {
                if let Ok(entry) = result {
                    if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                        return ignore::WalkState::Continue;
                    }

                    let path = entry.path().to_path_buf();
                    let path_str = path.to_string_lossy();
                    if !path_str.ends_with(".h") && !path_str.ends_with(".c") {
                        return ignore::WalkState::Continue;
                    }
                    if is_noisy_tree(&path_str) {
                        return ignore::WalkState::Continue;
                    }

                    let mut searcher = SearcherBuilder::new()
                        .binary_detection(BinaryDetection::quit(b'\x00'))
                        .line_number(true)
                        .build();

                    let _ = searcher.search_path(
                        &matcher,
                        &path,
                        grep_searcher::sinks::UTF8(|line_num, line| {
                            for sym in &symbols {
                                if line_matches_symbol(line, sym) {
                                    let mut defs = candidates.lock().unwrap();
                                    let entry = defs.entry(sym.clone()).or_default();
                                    if entry.len() < 32 {
                                        entry.push((path.clone(), line_num));
                                    }
                                }
                            }
                            Ok(true)
                        }),
                    );
                }
                ignore::WalkState::Continue
            })
        });
    })
    .await;

    let candidates_vec: Vec<(String, Vec<(PathBuf, u64)>)> = {
        let mut defs = candidates.lock().unwrap();
        defs.drain().collect()
    };

    for (sym, hits) in candidates_vec {
        if let Some((path, block)) = best_definition_block(&sym, &hits).await {
            let filename = path
                .strip_prefix(worktree_path)
                .unwrap_or(&path)
                .to_string_lossy();
            let def_str = format!(
                "--- Extracted Definition of {} from {} ---\n{}\n",
                sym, filename, block
            );

            if current_chars + def_str.len() > MAX_PREFETCH_CHARS {
                context_blocks.push("\n... (Definitions prefetch limits reached)\n".to_string());
                break;
            }
            current_chars += def_str.len();
            context_blocks.push(def_str);
        }
    }

    Ok(context_blocks.join("\n"))
}

/// Paths under these roots are test/sample/tools code that shadows real kernel
/// definitions (e.g. `tools/virtio/ringtest/` hosts a toy `spin_lock`). They
/// dominate first-match-wins lookups and contribute no signal for patch review.
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

/// Require the match to be a word-boundary hit against the symbol, not just a
/// substring occurrence. The regex matcher already filtered to definition-shaped
/// lines; this is the per-symbol disambiguation.
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
/// 0 means "not actually a definition" (forward decl, parameter name, etc.) —
/// the caller rejects those.
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
            // declarator is nested; find the function_declarator's identifier.
            let declared = function_name(node, source);
            if declared.as_deref() != Some(sym) {
                return 0;
            }
            if has_body { 90 } else { 0 }
        }
        "preproc_def" | "preproc_function_def" => {
            if names_symbol("name") {
                70
            } else {
                0
            }
        }
        "type_definition" => {
            // typedef struct foo { ... } sym; — score if the typedef name matches.
            if typedef_names_match(node, sym, source) {
                80
            } else {
                0
            }
        }
        _ => 0,
    }
    // Note: we deliberately do not score plain `declaration` nodes. They match
    // variable decls like `struct dentry *dentry;` and prototypes without
    // bodies — neither conveys the actual definition. If a symbol has only
    // declaration-form hits we'd rather omit it than pollute the prompt.
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
/// Returns (path, rendered block text).
async fn best_definition_block(sym: &str, hits: &[(PathBuf, u64)]) -> Option<(PathBuf, String)> {
    // Deduplicate paths — many hits may live in the same file.
    let mut seen = HashSet::new();
    let mut best: Option<(i32, PathBuf, String)> = None;

    for (path, _line) in hits {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(content) = fs::read_to_string(path).await else {
            continue;
        };
        let Some((score, block)) = score_best_in_file(&content, sym) else {
            continue;
        };
        if score == 0 {
            continue;
        }
        match &best {
            Some((best_score, _, _)) if *best_score >= score => {}
            _ => best = Some((score, path.clone(), block)),
        }
    }
    best.map(|(_, p, b)| (p, b))
}

/// Parse `content` once and find the highest-scoring definition of `sym`.
fn score_best_in_file(content: &str, sym: &str) -> Option<(i32, String)> {
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
    let start = node.start_byte();
    let end = node.end_byte();
    if end > content.len() {
        return None;
    }
    let lines_count = node
        .end_position()
        .row
        .saturating_sub(node.start_position().row);
    let block = if lines_count > 200 {
        format!(
            "// Block is too large ({} lines), truncated...\n{}",
            lines_count,
            truncate_to_window(content, node.start_position().row, node.end_position().row)
        )
    } else {
        content[start..end].to_string()
    };
    Some((score, block))
}
pub fn extract_enclosing_block(
    source_code: &str,
    start_line: usize,
    end_line: usize,
) -> Option<String> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_c::LANGUAGE.into()).ok()?;

    let tree = parser.parse(source_code, None)?;
    let root_node = tree.root_node();

    let start_point = Point::new(start_line, 0);
    let end_point = Point::new(end_line, usize::MAX);

    let mut current_node = root_node.descendant_for_point_range(start_point, end_point)?;

    let target_kinds = [
        "function_definition",
        "struct_specifier",
        "enum_specifier",
        "union_specifier",
        "declaration",
        "type_definition",
    ];

    let mut found_block = None;

    loop {
        if target_kinds.contains(&current_node.kind()) {
            found_block = Some(current_node);
            break;
        }
        if let Some(parent) = current_node.parent() {
            current_node = parent;
        } else {
            break;
        }
    }

    if let Some(node) = found_block {
        let start_byte = node.start_byte();
        let end_byte = node.end_byte();
        let lines_count = node
            .end_position()
            .row
            .saturating_sub(node.start_position().row);

        if start_byte < source_code.len() && end_byte <= source_code.len() {
            if lines_count > 200 {
                return Some(format!(
                    "// Block is too large ({} lines), truncated to 200 lines around the change...\n{}",
                    lines_count,
                    truncate_to_window(source_code, start_line, end_line)
                ));
            }
            return Some(source_code[start_byte..end_byte].to_string());
        }
    }

    Some(truncate_to_window(source_code, start_line, end_line))
}

fn truncate_to_window(source_code: &str, start_line: usize, end_line: usize) -> String {
    let lines: Vec<&str> = source_code.lines().collect();
    let start = start_line.saturating_sub(20);
    let end = std::cmp::min(lines.len().saturating_sub(1), end_line + 20);
    if start <= end && start < lines.len() {
        lines[start..=end].join("\n")
    } else {
        String::new()
    }
}

fn is_common_c_word(word: &str) -> bool {
    let common = [
        "int", "char", "void", "long", "short", "unsigned", "signed", "struct", "union", "enum",
        "typedef", "static", "const", "volatile", "if", "else", "for", "while", "do", "switch",
        "case", "default", "return", "break", "continue", "goto", "sizeof", "true", "false",
        "NULL", "inline", "extern", "register", "auto", "restrict", "u8", "u16", "u32", "u64",
        "s8", "s16", "s32", "s64", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "int8_t",
        "int16_t", "int32_t", "int64_t", "bool", "size_t", "ssize_t", "pid_t", "uid_t", "gid_t",
        "off_t", "ret", "err", "len", "size", "res", "tmp", "val", "ptr", "idx", "out",
    ];
    common.contains(&word)
}

/// Extracts C type names referenced within (and around) the modified line range.
///
/// Uses tree-sitter's `type_identifier` node-kind — which is semantically distinct
/// from `identifier` — so we pick up `fbnic_dev`, `fbnic_net`, `seq_file` and
/// skip variable/field/function names like `fbd`, `fbn`, `ret`.
///
/// Scopes collection to the **enclosing function/struct/typedef**, not just the
/// exact modified lines, so types declared in a function signature (`struct
/// fbnic_dev *fbd`) are captured even when the diff only touches the body.
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

    // Widen to the enclosing function/struct/typedef so signature types are in scope.
    let target_kinds = [
        "function_definition",
        "struct_specifier",
        "union_specifier",
        "enum_specifier",
        "type_definition",
    ];
    while !target_kinds.contains(&scope.kind()) {
        match scope.parent() {
            Some(p) => scope = p,
            None => break,
        }
    }

    fn walk(n: Node<'_>, src: &[u8], out: &mut HashSet<String>) {
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
            walk(child, src, out);
        }
    }
    walk(scope, source_code.as_bytes(), &mut types);
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
        let block_main = extract_enclosing_block(source_code, 4, 4).unwrap();
        assert!(block_main.starts_with("int main() {"));
        assert!(block_main.ends_with("return 0;\n}"));

        let block_struct = extract_enclosing_block(source_code, 10, 10).unwrap();
        assert!(block_struct.starts_with("struct MyStruct"));
    }
}
