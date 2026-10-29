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

use anyhow::Result;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The XDG data directory, where anything Sashiko keeps between runs lives:
/// the installed prompt bundle, and the response cache for a review with no
/// database beside it.
pub fn data_home() -> Result<PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data_home));
    }

    if let Some(home) = std::env::var_os("HOME") {
        return Ok(Path::new(&home).join(".local/share"));
    }

    Ok(std::env::current_dir()?.join(".local/share"))
}

static KEY_REGEX: OnceLock<Regex> = OnceLock::new();
static URL_CRED_REGEX: OnceLock<Regex> = OnceLock::new();
static URL_USER_CRED_REGEX: OnceLock<Regex> = OnceLock::new();

/// Returns the longest prefix of `input` that is valid UTF-8 and no longer
/// than `max_bytes` bytes.
///
/// This is useful when text must fit a byte-limited preview. Direct string
/// slicing can panic when the byte limit falls inside a multi-byte character.
pub fn utf8_prefix(input: &str, max_bytes: usize) -> &str {
    let mut end = input.len().min(max_bytes);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

/// Redacts sensitive information from a string.
///
/// Specifically targets:
/// - API keys in query parameters (e.g., `key=AIza...`)
/// - Credentials in URLs (e.g., `https://user:pass@host` or `https://token@host`)
pub fn redact_secret(s: &str) -> String {
    let key_re =
        KEY_REGEX.get_or_init(|| Regex::new(r"(?i)(key|token|secret)=([a-zA-Z0-9_\-]+)").unwrap());

    let url_cred_re =
        URL_CRED_REGEX.get_or_init(|| Regex::new(r"://([^/:\s@]*):([^/\s@]*)@").unwrap());
    let url_user_cred_re =
        URL_USER_CRED_REGEX.get_or_init(|| Regex::new(r"(?i)(https?://)([^/:\s@]+)@").unwrap());

    let redacted_params = key_re.replace_all(s, "$1=[REDACTED]");
    let redacted_url = url_cred_re.replace_all(&redacted_params, "://[REDACTED]:[REDACTED]@");
    let redacted_user_url = url_user_cred_re.replace_all(&redacted_url, "${1}[REDACTED]@");

    redacted_user_url.to_string()
}

/// Cleans a JSON string by escaping unescaped control characters inside string literals.
///
/// This is particularly useful for parsing LLM-generated JSON, which sometimes
/// contains literal newlines or tabs inside string values instead of the
/// correct escape sequences (`\n`, `\t`, etc.).
pub fn clean_json_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escape = false;

    for c in input.chars() {
        if in_string {
            if escape {
                out.push('\\');
                out.push(c);
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                out.push(c);
                in_string = false;
            } else if c == '\n' {
                out.push_str("\\n");
            } else if c == '\r' {
                out.push_str("\\r");
            } else if c == '\t' {
                out.push_str("\\t");
            } else if c < '\x20' {
                use std::fmt::Write;
                write!(&mut out, "\\u{:04x}", c as u32).unwrap();
            } else {
                out.push(c);
            }
        } else {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
        }
    }

    // If the string ended while still in an escape sequence
    if escape {
        out.push('\\');
    }

    out
}

/// Best-effort repairs for common LLM JSON malformations that strict
/// parsing rejects: unescaped double quotes inside string literals and
/// trailing commas before `}` or `]`.
///
/// A quote inside a string is treated as the terminator only when the next
/// non-whitespace character is a structural token (`,` `:` `}` `]`) or the
/// input ends; any other quote is escaped as content. A quote followed by a
/// comma is ambiguous: it genuinely separates list elements but also shows
/// up inside quoted prose, and closing prose early can leave behind valid
/// but semantically shifted JSON. The first syntax error localizes where
/// parsing derailed, so comma- or colon-preceded quotes at or after it are
/// escaped as prose while quotes in the region the parser already accepted
/// stay terminators; a pairs-only fallback is appended for inputs where the
/// prose reading does not parse. Callers re-parse each variant with a
/// strict parser and use the first that parses, keeping the original error
/// when none does. A quote directly followed by a closing brace or bracket
/// always terminates, so prose that embeds one right after a quote stays
/// unrecoverable.
pub fn repair_json_candidates(input: &str) -> Vec<String> {
    let error_offset = serde_json::from_str::<serde_json::Value>(input)
        .err()
        .map(|e| error_byte_offset(input, e.line(), e.column()));
    let mixed = repair_json_with(input, error_offset);
    let pairs = repair_json_with(input, None);
    if mixed == pairs {
        vec![mixed]
    } else {
        vec![mixed, pairs]
    }
}

/// Maps a serde_json 1-based line/column error position onto a byte offset
/// in `input`, clamped to the line and input lengths.
fn error_byte_offset(input: &str, line: usize, column: usize) -> usize {
    let mut offset = 0;
    for (i, line_text) in input.split('\n').enumerate() {
        if i + 1 == line {
            return offset + column.saturating_sub(1).min(line_text.len());
        }
        offset += line_text.len() + 1;
    }
    offset
}

/// Renders one repair reading of `input`. A comma-preceded in-string quote
/// terminates the string only when it lies before `error_offset` (or when
/// the position is unknown). See [`repair_json_candidates`].
fn repair_json_with(input: &str, error_offset: Option<usize>) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escape = false;
    let mut byte_pos = 0;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if in_string {
            if escape {
                out.push('\\');
                out.push(c);
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                let terminates = if j >= chars.len() {
                    true
                } else {
                    match chars[j] {
                        '}' | ']' => true,
                        ',' | ':' => {
                            error_offset.is_none() || byte_pos < error_offset.unwrap_or(usize::MAX)
                        }
                        _ => false,
                    }
                };
                if terminates {
                    in_string = false;
                    out.push(c);
                } else {
                    out.push_str("\\\"");
                }
            } else {
                out.push(c);
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            // Strict JSON forbids a comma directly before } or ].
            if !matches!(chars.get(j), Some('}') | Some(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        byte_pos += c.len_utf8();
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utf8_prefix_preserves_character_boundaries() {
        assert_eq!(utf8_prefix("plain text", 5), "plain");
        assert_eq!(utf8_prefix("abc🙂def", 7), "abc🙂");
        assert_eq!(utf8_prefix("abc🙂def", 6), "abc");
        assert_eq!(utf8_prefix("🙂", 0), "");
        assert_eq!(utf8_prefix("short", 100), "short");
    }

    #[test]
    fn test_repair_json_candidates_pairs_reading_is_identity_on_valid_json() {
        let valid = r#"{"a": "x \"y\" z", "b": [1, 2], "c": {"d": true}}"#;
        assert_eq!(repair_json_candidates(valid), vec![valid.to_string()]);
    }

    #[test]
    fn test_repair_json_candidates_prefers_prose_over_shifted_pairs() {
        // The early-closed "say " string is where parsing derails, so the
        // comma- and colon-preceded quotes after the error must be read as
        // prose: terminating them instead would yield valid but shifted
        // JSON with a truncated value and a phantom key. The last inner
        // quote is consumed by the mandatory terminator, which is the best
        // reading an input missing its real closing quote allows.
        let broken = r#"{"a": "say "hi", "b": "ok"}"#;
        let candidates = repair_json_candidates(broken);
        let parsed = candidates
            .iter()
            .find_map(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .unwrap();
        assert_eq!(parsed["a"], "say \"hi\", \"b\": \"ok");
    }

    #[test]
    fn test_repair_json_candidates_escapes_unescaped_inner_quotes() {
        let broken = r#"{"description": "the flag turns "on" by default", "n": 3}"#;
        let repaired = repair_json_candidates(broken);
        let parsed = repaired
            .iter()
            .find_map(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .unwrap();
        assert_eq!(parsed["description"], "the flag turns \"on\" by default");
        assert_eq!(parsed["n"], 3);
    }

    #[test]
    fn test_repair_json_candidates_drops_trailing_commas() {
        let broken = "{\n  \"a\": [1, 2,],\n  \"b\": \"x\",\n}\n";
        let repaired = repair_json_candidates(broken);
        let parsed = repaired
            .iter()
            .find_map(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .unwrap();
        assert_eq!(parsed["a"], serde_json::json!([1, 2]));
        assert_eq!(parsed["b"], "x");
    }

    #[test]
    fn test_repair_json_candidates_terminates_list_elements_before_the_error() {
        // The trailing comma is the first syntax error, so the quotes the
        // parser already accepted as element separators stay terminators.
        let broken = r#"{"a": ["one", "two"], "c": [3,]}"#;
        let repaired = repair_json_candidates(broken);
        let parsed = repaired
            .iter()
            .find_map(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .unwrap();
        assert_eq!(parsed["a"], serde_json::json!(["one", "two"]));
        assert_eq!(parsed["c"], serde_json::json!([3]));
    }

    #[test]
    fn test_repair_json_candidates_recovers_embedded_key_colon_pairs() {
        // In the prose reading, colon-preceded quotes after the first
        // syntax error are content, so prose that itself embeds key-colon
        // pairs is recovered instead of truncated at the embedded colon.
        let broken = r#"{"a": "flag "b": 1"}"#;
        let candidates = repair_json_candidates(broken);
        let parsed = candidates
            .iter()
            .find_map(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .unwrap();
        assert_eq!(parsed["a"], "flag \"b\": 1");
    }

    #[test]
    fn test_redact_gemini_key() {
        let url = "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:generateContent?key=AIzaSyD-12345";
        let redacted = redact_secret(url);
        assert_eq!(
            redacted,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:generateContent?key=[REDACTED]"
        );
    }

    #[test]
    fn test_redact_git_credentials() {
        let url = "https://user:password123@github.com/torvalds/linux.git";
        let redacted = redact_secret(url);
        assert_eq!(
            redacted,
            "https://[REDACTED]:[REDACTED]@github.com/torvalds/linux.git"
        );

        let token_url = "https://ghp_secretToken123@github.com/sashiko-dev/sashiko.git";
        assert_eq!(
            redact_secret(token_url),
            "https://[REDACTED]@github.com/sashiko-dev/sashiko.git"
        );

        let empty_pass_url = "https://token_only:@github.com/sashiko-dev/sashiko.git";
        assert_eq!(
            redact_secret(empty_pass_url),
            "https://[REDACTED]:[REDACTED]@github.com/sashiko-dev/sashiko.git"
        );

        let ssh_url = "ssh://git@github.com/torvalds/linux.git";
        assert_eq!(redact_secret(ssh_url), ssh_url);
    }

    #[test]
    fn test_redact_mixed() {
        let s = "Error connecting to https://user:pass@host/api?key=secret_value";
        let redacted = redact_secret(s);
        assert_eq!(
            redacted,
            "Error connecting to https://[REDACTED]:[REDACTED]@host/api?key=[REDACTED]"
        );
    }

    #[test]
    fn test_no_secrets() {
        let s = "https://github.com/torvalds/linux.git";
        let redacted = redact_secret(s);
        assert_eq!(redacted, s);
    }

    #[test]
    fn test_clean_json_string() {
        let valid = r#"{"name": "test", "value": "a\nb"}"#;
        assert_eq!(clean_json_string(valid), valid);

        let invalid = "{\"name\": \"test\", \"value\": \"a\nb\"}";
        let fixed = r#"{"name": "test", "value": "a\nb"}"#;
        assert_eq!(clean_json_string(invalid), fixed);

        let invalid_tab = "{\"key\": \"val\tue\"}";
        let fixed_tab = r#"{"key": "val\tue"}"#;
        assert_eq!(clean_json_string(invalid_tab), fixed_tab);

        let structural = "{\n  \"key\": \"value\"\n}";
        assert_eq!(clean_json_string(structural), structural);
    }
}
