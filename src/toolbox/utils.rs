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

use crate::ai::truncator::Truncator;
use anyhow::{Context, Result, anyhow, ensure};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

fn clean_relative_path(relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(anyhow!("Invalid path: {}", relative));
    }

    let mut clean_rel = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(c) => clean_rel.push(c),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("Invalid path: {}", relative));
            }
        }
    }
    Ok(clean_rel)
}

/// Validates a relative path against a base path to prevent path traversal attacks.
///
/// Returns the canonicalized absolute path if it is safe and confined within `base`,
/// allowing non-existent trailing path components (such as files that only exist in
/// git history) while rejecting symlinks or dangling symlinks that escape `base`.
pub fn validate_path(relative: &str, base: &Path) -> Result<PathBuf> {
    let clean_rel = clean_relative_path(relative)?;

    let canonical_base = base
        .canonicalize()
        .context("Failed to canonicalize base directory")?;
    if !canonical_base.is_dir() {
        return Err(anyhow!(
            "Base path is not a directory: {:?}",
            canonical_base
        ));
    }

    let full_path = canonical_base.join(&clean_rel);
    let mut current = full_path.as_path();
    let mut missing_components = Vec::new();

    let canonical_full = loop {
        match current.canonicalize() {
            Ok(canon) => {
                if !canon.starts_with(&canonical_base) {
                    return Err(anyhow!(
                        "Access denied: Path escapes root directory: {:?}",
                        canon
                    ));
                }
                let mut resolved = canon;
                for comp in missing_components.into_iter().rev() {
                    resolved.push(comp);
                }
                break resolved;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                if current.symlink_metadata().is_ok() {
                    return Err(anyhow!(
                        "Dangling symlink not allowed in path: {:?}",
                        current
                    ));
                }
                if let (Some(parent), Some(name)) = (current.parent(), current.file_name()) {
                    missing_components.push(name.to_os_string());
                    current = parent;
                } else {
                    return Err(anyhow!("No parent directory for path: {:?}", full_path));
                }
            }
            Err(e) => return Err(anyhow!("Failed to canonicalize path: {}", e)),
        }
    };

    if !canonical_full.starts_with(&canonical_base) {
        return Err(anyhow!("Path traversal detected: {:?}", canonical_full));
    }

    Ok(canonical_full)
}

/// Converts a simple glob pattern (supporting `*` and `?`) into a compiled Regex.
pub fn glob_to_regex(glob: &str) -> Result<regex::Regex> {
    let mut regex_str = String::new();
    regex_str.push('^');
    for c in glob.chars() {
        match c {
            '*' => regex_str.push_str(".*"),
            '?' => regex_str.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\' => {
                regex_str.push('\\');
                regex_str.push(c);
            }
            _ => regex_str.push(c),
        }
    }
    regex_str.push('$');
    regex::Regex::new(&regex_str).map_err(|e| anyhow!("Invalid glob converted to regex: {}", e))
}

fn get_grep_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^([a-zA-Z0-9_./-]+)(:|-)([0-9]+)(:|-)(.*)$").unwrap())
}

/// Formats raw git grep output into a clean, grouped structure, sorting findings
/// by proximity to the files modified in the active patchset.
pub fn format_git_grep_output(stdout: &str, revision: &str, active_files: &[String]) -> String {
    let prefix = format!("{}:", revision);
    let re = get_grep_regex();

    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current_file: Option<String> = None;

    for line in stdout.lines() {
        if line == "--" {
            if let Some(ref cur) = current_file
                && let Some(list) = grouped.get_mut(cur)
            {
                list.push("  --".to_string());
            }
            continue;
        }

        let stripped = if line.starts_with(&prefix) {
            &line[prefix.len()..]
        } else {
            line
        };

        if let Some(caps) = re.captures(stripped) {
            let path = &caps[1];
            let sep1 = &caps[2];
            let line_num = &caps[3];
            let sep2 = &caps[4];
            let content = &caps[5];

            if sep1 == sep2 {
                let formatted_line = format!("  {}{}{}", line_num, sep1, content);
                let path_str = path.to_string();
                current_file = Some(path_str.clone());
                grouped.entry(path_str).or_default().push(formatted_line);
            } else if let Some(ref cur) = current_file {
                grouped
                    .entry(cur.clone())
                    .or_default()
                    .push(stripped.to_string());
            }
        } else if let Some(ref cur) = current_file {
            grouped
                .entry(cur.clone())
                .or_default()
                .push(stripped.to_string());
        }
    }

    // Proximity Ranking: sort matching files so that files closest to modified files appear first
    let mut blocks: Vec<(String, Vec<String>)> = grouped.into_iter().collect();
    blocks.sort_by_key(|(path, _)| (get_priority_score(path, active_files), path.clone()));

    let total_files = blocks.len();
    let total_matches: usize = blocks
        .iter()
        .map(|(_, lines)| lines.iter().filter(|l| l.trim() != "--").count())
        .sum();

    const MAX_SUMMARY_FILES: usize = 10;
    let file_summaries: Vec<String> = blocks
        .iter()
        .take(MAX_SUMMARY_FILES)
        .map(|(path, lines)| {
            let count = lines.iter().filter(|l| l.trim() != "--").count();
            format!(
                "{} ({} {})",
                path,
                count,
                if count == 1 { "match" } else { "matches" }
            )
        })
        .collect();

    let mut summary = file_summaries.join(", ");
    if total_files > MAX_SUMMARY_FILES {
        summary.push_str(&format!(
            ", ... and {} more files",
            total_files - MAX_SUMMARY_FILES
        ));
    }

    let mut result = String::new();
    if total_files > 0 {
        result.push_str(&format!(
            "Matches found across {} {} ({} total {}): {}\n\n",
            total_files,
            if total_files == 1 { "file" } else { "files" },
            total_matches,
            if total_matches == 1 {
                "match"
            } else {
                "matches"
            },
            summary
        ));
    }

    for (path, lines) in blocks {
        result.push_str(&format!("[file: {}]\n", path));
        for l in lines {
            result.push_str(&l);
            result.push('\n');
        }
        result.push('\n');
    }

    result.trim_end().to_string()
}

fn get_priority_score(path: &str, active_files: &[String]) -> u32 {
    if active_files.is_empty() {
        return 4;
    }

    // 1. Exact Match (highest priority)
    if active_files.iter().any(|f| f == path) {
        return 1;
    }

    // 2. Directory Prefix Match
    let path_parent = Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if !path_parent.is_empty() {
        for active_file in active_files {
            let active_parent = Path::new(active_file)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if !active_parent.is_empty() && path_parent == active_parent {
                return 2;
            }
        }
    }

    // 3. Include Directory Match
    if path.starts_with("include/") {
        return 3;
    }

    // 4. Default (lowest priority)
    4
}

/// Recursively converts integral floating-point JSON numbers (e.g. `4475.0`) into integer
/// JSON numbers (`4475`). Protobuf-to-JSON proxies represent all numbers as IEEE-754 doubles,
/// which serialize with a trailing `.0` and cause `serde_json::Value::as_u64()` to return `None`.
pub fn normalize_json_numbers(val: serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Number(n) => {
            if n.as_i64().is_some() || n.as_u64().is_some() {
                serde_json::Value::Number(n)
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f.fract() == 0.0 {
                    if f >= 0.0 && f <= u64::MAX as f64 {
                        serde_json::Value::Number(serde_json::Number::from(f as u64))
                    } else if f >= i64::MIN as f64 && f < 0.0 {
                        serde_json::Value::Number(serde_json::Number::from(f as i64))
                    } else {
                        serde_json::Value::Number(n)
                    }
                } else {
                    serde_json::Value::Number(n)
                }
            } else {
                serde_json::Value::Number(n)
            }
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(normalize_json_numbers).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, normalize_json_numbers(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Formats a slice of source text with 1-based line numbers, a section header,
/// and sequential token-budget truncation.
///
/// Line numbers are prepended prior to calling [`Truncator::truncate_sequential`]
/// so the byte budget accounts for the line-number prefixes and `lines_kept`
/// tracks the exact count of source lines retained.
pub fn format_numbered_slice(
    label: &str,
    tool_name: &str,
    content: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_tokens: usize,
) -> Result<String> {
    if let (Some(s), Some(e)) = (start_line, end_line) {
        ensure!(s <= e, "Invalid range: start_line ({s}) > end_line ({e})");
    }

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    if total_lines == 0 {
        return Ok(format!("=== {label} (empty, 0 lines) ==="));
    }

    // Empty files return early above, so 0-based start/end bounds are safe without clamp.
    let (start, end) = match (start_line, end_line) {
        (Some(s), Some(e)) => (s.max(1) - 1, e.min(total_lines)),
        (Some(s), None) => (s.max(1) - 1, total_lines),
        (None, Some(e)) => (0, e.min(total_lines)),
        (None, None) => (0, total_lines),
    };

    if start >= total_lines {
        return Ok(format!("=== {label} (0 lines shown of {total_lines}) ==="));
    }
    let end = end.clamp(start, total_lines);

    let numbered = lines[start..end]
        .iter()
        .enumerate()
        .map(|(idx, line)| format!("{}: {}", start + idx + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    let res = Truncator::truncate_sequential(&numbered, max_tokens);
    let start_idx = start + 1;
    let end_idx = if res.truncated {
        start + res.lines_kept.max(1)
    } else {
        end
    };

    let mut out = format!(
        "=== {label} (lines {start_idx}-{end_idx} of {total_lines}) ===\n{}",
        res.content.trim_end_matches('\n')
    );
    if res.truncated {
        out.push_str(&format!(
            "\n[Truncated: lines {start_idx}-{end_idx} of {total_lines} shown. Call {tool_name} with start_line={} to read more.]",
            end_idx + 1
        ));
    }
    Ok(out)
}

const SNIPPET_ELISIONS: &[&str] = &["...", "…", "// ...", "/* ... */", "--"];

enum ParsedSnippetLine<'a> {
    Passthrough(&'a str),
    Numbered {
        line_no: usize,
        sep: char,
        rest: &'a str,
    },
}

fn parse_numbered_tool_line(line: &str) -> Option<ParsedSnippetLine<'_>> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return Some(ParsedSnippetLine::Passthrough(""));
    }
    if SNIPPET_ELISIONS.contains(&trimmed) {
        return Some(ParsedSnippetLine::Passthrough(trimmed));
    }
    if trimmed.starts_with("... [") && trimmed.ends_with("] ...") {
        return Some(ParsedSnippetLine::Passthrough("..."));
    }

    let digit_len = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if !(1..=7).contains(&digit_len) {
        return None;
    }
    let line_no: usize = trimmed[..digit_len].parse().ok()?;
    if line_no == 0 {
        return None;
    }

    let after = &trimmed[digit_len..];
    if let Some(rest) = after.strip_prefix(':') {
        if rest.starts_with(':') {
            return None;
        }
        Some(ParsedSnippetLine::Numbered {
            line_no,
            sep: ':',
            rest,
        })
    } else if let Some(rest) = after.strip_prefix('-') {
        if rest.starts_with(['-', '>', '=']) {
            return None;
        }
        Some(ParsedSnippetLine::Numbered {
            line_no,
            sep: '-',
            rest,
        })
    } else if let Some(rest) = after.strip_prefix('=') {
        if rest.starts_with(['=', '>']) {
            return None;
        }
        Some(ParsedSnippetLine::Numbered {
            line_no,
            sep: '=',
            rest,
        })
    } else {
        None
    }
}

/// Strips 1-based tool line-number prefixes (from `git_read_files`, `git_show`,
/// or `git_grep`) when every non-empty, non-elision line carries a strictly
/// increasing line number.
pub fn strip_tool_line_numbers(raw: &str) -> Option<String> {
    let mut parsed = Vec::new();
    let mut prev_line_no: Option<usize> = None;
    let mut numbered_count = 0usize;
    let mut has_grep_context_sep = false;

    for line in raw.lines() {
        let item = parse_numbered_tool_line(line)?;
        if let ParsedSnippetLine::Numbered { line_no, sep, .. } = item {
            if prev_line_no.is_some_and(|prev| line_no <= prev) {
                return None;
            }
            prev_line_no = Some(line_no);
            numbered_count += 1;
            if sep != ':' {
                has_grep_context_sep = true;
            }
        }
        parsed.push(item);
    }

    if numbered_count == 0 {
        return None;
    }

    let stripped: Vec<&str> = parsed
        .into_iter()
        .map(|item| match item {
            ParsedSnippetLine::Passthrough(text) => text,
            ParsedSnippetLine::Numbered { sep, rest, .. } => {
                if sep == ':' && !has_grep_context_sep {
                    rest.strip_prefix(' ').unwrap_or(rest)
                } else {
                    rest
                }
            }
        })
        .collect();

    Some(stripped.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_git_grep_output_summary_header() {
        let stdout = "HEAD:fs/ext4/inline.c:1518:if (x)\nHEAD:fs/ext4/ext4.h:2489:static inline\nHEAD:fs/ext4/dir.c:91:if (y)\nHEAD:fs/ext4/dir.c:95:else";
        let active_files = vec!["fs/ext4/inline.c".to_string()];
        let formatted = format_git_grep_output(stdout, "HEAD", &active_files);
        assert!(formatted.starts_with("Matches found across 3 files (4 total matches): fs/ext4/inline.c (1 match), fs/ext4/dir.c (2 matches), fs/ext4/ext4.h (1 match)"));
        assert!(formatted.contains("[file: fs/ext4/inline.c]"));
    }

    #[test]
    fn test_format_git_grep_output_summary_header_truncation() {
        let mut lines = Vec::new();
        for i in 1..=15 {
            lines.push(format!("HEAD:file_{}.c:1:match", i));
        }
        let stdout = lines.join("\n");
        let active_files = Vec::new();
        let formatted = format_git_grep_output(&stdout, "HEAD", &active_files);
        assert!(formatted.starts_with("Matches found across 15 files (15 total matches):"));
        assert!(formatted.contains(", ... and 5 more files"));
    }

    #[test]
    fn test_normalize_json_numbers() {
        let input = serde_json::json!({
            "files": [
                {
                    "path": "mm/memcontrol.c",
                    "start_line": 4475.0,
                    "end_line": 4505.0
                }
            ],
            "limit": 5.0,
            "offset": -10.0,
            "temperature": 0.75
        });

        let normalized = normalize_json_numbers(input);
        let file = &normalized["files"][0];
        assert_eq!(file["start_line"].as_u64(), Some(4475));
        assert_eq!(file["end_line"].as_u64(), Some(4505));
        assert_eq!(normalized["limit"].as_u64(), Some(5));
        assert_eq!(normalized["offset"].as_i64(), Some(-10));
        assert_eq!(normalized["temperature"].as_f64(), Some(0.75));
        assert_eq!(normalized["temperature"].as_u64(), None);
    }

    #[test]
    fn test_glob_to_regex() {
        // Group A: * wildcard
        // * matches zero or more characters.
        let re = glob_to_regex("*.c").unwrap();
        assert!(re.is_match("foo.c"), "* matches one or more characters");
        assert!(re.is_match(".c"), "* matches zero characters");
        assert!(!re.is_match("foo.rs"), "* does not match wrong extension");

        // Group B: ? wildcard
        // ? matches exactly one character.
        let re = glob_to_regex("fo?.c").unwrap();
        assert!(re.is_match("foo.c"), "? matches exactly one character");
        assert!(!re.is_match("fooo.c"), "? does not match two characters");
        assert!(!re.is_match("fo.c"), "? does not match zero characters");

        // Group C: regex metacharacter escaping
        // . must be a literal dot, not the regex wildcard.
        let re = glob_to_regex("file.c").unwrap();
        assert!(re.is_match("file.c"), ". matches literal dot");
        assert!(!re.is_match("fileXc"), ". is not treated as regex wildcard");

        // + must be a literal plus, not a quantifier.
        let re = glob_to_regex("a+b").unwrap();
        assert!(re.is_match("a+b"), "+ matches literal plus");
        assert!(!re.is_match("ab"), "+ is not treated as regex quantifier");
        assert!(
            !re.is_match("aab"),
            "+ is not treated as one-or-more quantifier"
        );

        // ( and ) must be literal parens, not group delimiters.
        let re = glob_to_regex("a(b").unwrap();
        assert!(re.is_match("a(b"), "( matches literal open paren");
        assert!(!re.is_match("ab"), "( is not dropped as a group opener");

        let re = glob_to_regex("a)b").unwrap();
        assert!(re.is_match("a)b"), ") matches literal close paren");
        assert!(!re.is_match("ab"), ") is not dropped as a group closer");

        // [ must be escaped; an unescaped [ would produce an invalid regex.
        let re = glob_to_regex("a[b").unwrap();
        assert!(re.is_match("a[b"), "[ matches literal open bracket");
        assert!(
            !re.is_match("ab"),
            "[ is not treated as character class opener"
        );

        // ] must be a literal bracket.
        let re = glob_to_regex("a]b").unwrap();
        assert!(re.is_match("a]b"), "] matches literal close bracket");
        assert!(!re.is_match("ab"), "] is not dropped");

        // | must be a literal pipe, not alternation.
        let re = glob_to_regex("a|b").unwrap();
        assert!(re.is_match("a|b"), "| matches literal pipe");
        assert!(
            !re.is_match("a"),
            "| is not treated as alternation (left side)"
        );
        assert!(
            !re.is_match("b"),
            "| is not treated as alternation (right side)"
        );

        // ^ must be a literal caret, not a negation or extra anchor.
        let re = glob_to_regex("a^b").unwrap();
        assert!(re.is_match("a^b"), "^ matches literal caret");
        assert!(!re.is_match("ab"), "^ is not dropped");

        // $ must be a literal dollar sign, not an extra end anchor.
        let re = glob_to_regex("a$b").unwrap();
        assert!(re.is_match("a$b"), "$ matches literal dollar sign");
        assert!(!re.is_match("ab"), "$ is not treated as end anchor");

        // \ must be a literal backslash.
        let re = glob_to_regex(r"a\b").unwrap();
        assert!(re.is_match(r"a\b"), r"\ matches literal backslash");
        assert!(!re.is_match("ab"), r"\ is not dropped");

        // Group D: anchoring
        // The pattern must match the entire string, not a substring.
        let re = glob_to_regex("foo").unwrap();
        assert!(re.is_match("foo"), "exact match works");
        assert!(!re.is_match("barfoo"), "^ anchor rejects a leading prefix");
        assert!(!re.is_match("foobar"), "$ anchor rejects a trailing suffix");

        // Group E: empty pattern
        // An empty glob compiles to ^$ and matches only the empty string.
        let re = glob_to_regex("").unwrap();
        assert!(re.is_match(""), "empty pattern matches empty string");
        assert!(
            !re.is_match("a"),
            "empty pattern does not match non-empty string"
        );
    }

    #[test]
    fn test_validate_path() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("worktree");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let existing_file = base.join("existing.c");
        std::fs::write(&existing_file, "int main() {}\n").unwrap();
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "secret\n").unwrap();

        let canon_base = base.canonicalize().unwrap();

        // 1. Existing file inside base succeeds
        let resolved = validate_path("existing.c", &base).unwrap();
        assert_eq!(resolved, canon_base.join("existing.c"));

        // 2. Non-existent nested path (e.g. deleted in git history) succeeds
        let resolved_missing = validate_path("deleted_dir/sub/file.c", &base).unwrap();
        assert_eq!(resolved_missing, canon_base.join("deleted_dir/sub/file.c"));
        let resolved_dots = validate_path("v1..v2.txt", &base).unwrap();
        assert_eq!(resolved_dots, canon_base.join("v1..v2.txt"));

        // 3. Parent directory traversal and absolute paths are rejected
        assert!(validate_path("..", &base).is_err());
        assert!(validate_path("../outside/secret.txt", &base).is_err());
        assert!(validate_path("sub/../../outside", &base).is_err());
        assert!(validate_path("/etc/passwd", &base).is_err());

        // 4. Symlinks pointing outside base (and nested paths under them) are rejected
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let escape_dir_link = base.join("escape_dir");
            symlink(&outside, &escape_dir_link).unwrap();
            assert!(validate_path("escape_dir", &base).is_err());
            assert!(validate_path("escape_dir/secret.txt", &base).is_err());
            assert!(validate_path("escape_dir/nonexistent/file.c", &base).is_err());

            let dangling_link = base.join("dangling_link");
            symlink(outside.join("does_not_exist"), &dangling_link).unwrap();
            assert!(validate_path("dangling_link", &base).is_err());
            assert!(validate_path("dangling_link/sub/file.c", &base).is_err());
        }
    }

    #[test]
    fn test_format_numbered_slice_basic_and_edge_cases() {
        let src = "alpha\n\ngamma\ndelta\n";
        let out = format_numbered_slice("foo.c", "git_read_files", src, Some(1), Some(3), 20_000)
            .unwrap();
        assert_eq!(
            out,
            "=== foo.c (lines 1-3 of 4) ===\n1: alpha\n2: \n3: gamma"
        );

        let empty =
            format_numbered_slice("empty.c", "git_read_files", "", Some(1), Some(5), 20_000)
                .unwrap();
        assert_eq!(empty, "=== empty.c (empty, 0 lines) ===");

        let past_eof =
            format_numbered_slice("foo.c", "git_read_files", src, Some(10), Some(20), 20_000)
                .unwrap();
        assert_eq!(past_eof, "=== foo.c (0 lines shown of 4) ===");

        assert!(
            format_numbered_slice("foo.c", "git_read_files", src, Some(5), Some(2), 20_000)
                .is_err()
        );
    }

    #[test]
    fn test_strip_tool_line_numbers() {
        // Multi-line git_read_files / git_show output preserves source indentation and blank lines.
        let read_snip = "10: pub fn get(m: &Mutex<u8>) -> u8 {\n11:\n12:     let guard = m.lock().unwrap();\n...\n15:     *guard";
        assert_eq!(
            strip_tool_line_numbers(read_snip).as_deref(),
            Some(
                "pub fn get(m: &Mutex<u8>) -> u8 {\n\n    let guard = m.lock().unwrap();\n...\n    *guard"
            )
        );

        // git_grep output with '=', '-', and ':' prefixes preserves relative indentation.
        let grep_snip = "  100=static void shrink_node(pg_data_t *pgdat)\n  ... [50 lines of enclosing block omitted] ...\n  151-    int x = 1;\n  152:    target_call(pgdat);";
        assert_eq!(
            strip_tool_line_numbers(grep_snip).as_deref(),
            Some(
                "static void shrink_node(pg_data_t *pgdat)\n...\n    int x = 1;\n    target_call(pgdat);"
            )
        );

        // Non-increasing line numbers, 0-based tuple fields, and plain code return None.
        assert_eq!(
            strip_tool_line_numbers("10: first();\n10: duplicate();"),
            None
        );
        assert_eq!(strip_tool_line_numbers("12: later();\n9: earlier();"), None);
        assert_eq!(strip_tool_line_numbers("0: tuple_field"), None);
        assert_eq!(strip_tool_line_numbers("let guard = m.lock();"), None);
        assert_eq!(strip_tool_line_numbers("..."), None);
    }
}
