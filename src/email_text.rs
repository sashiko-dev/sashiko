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

//! Plain-text layout for the review emails Sashiko writes itself: the
//! summary header and the related pre-existing bug notes. Lines are wrapped
//! at 72 columns, the convention of kernel mailing lists.

/// Column review email text is wrapped at.
pub const WRAP_WIDTH: usize = 72;

/// Greedy word wrap of `text` at `WRAP_WIDTH`. The first line starts with
/// `first`, the others with `rest`; a word longer than the width keeps a
/// line to itself. Every line ends with a newline.
pub fn wrap(text: &str, first: &str, rest: &str) -> String {
    let mut out = String::new();
    let mut line = first.to_string();
    let mut prefix_len = first.len();
    for word in text.split_whitespace() {
        let empty = line.len() == prefix_len;
        if !empty && line.len() + 1 + word.len() > WRAP_WIDTH {
            out.push_str(&line);
            out.push('\n');
            line = rest.to_string();
            prefix_len = rest.len();
        } else if !empty {
            line.push(' ');
        }
        line.push_str(word);
    }
    out.push_str(&line);
    out.push('\n');
    out
}

/// A `- [Severity] problem` summary line, wrapped with a hanging indent.
pub fn bullet(severity: &str, problem: &str) -> String {
    wrap(&format!("[{severity}] {}", problem.trim()), "- ", "  ")
}

/// `"1 <singular>"` or `"<n> <plural>"`.
pub fn count(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// The opening sentence of a review email with `n` new findings.
pub fn findings_header(n: usize) -> String {
    wrap(
        &format!(
            "Thank you for your contribution! Sashiko AI review found {} to consider:",
            count(n, "potential issue", "potential issues")
        ),
        "",
        "",
    )
}

/// Re-wraps the prose of free text at `WRAP_WIDTH`: consecutive prose lines
/// form a paragraph, while quoted (`>`), indented and empty lines are kept
/// verbatim so quoted code is never reflowed. No trailing newline.
pub fn wrap_prose(text: &str) -> String {
    let mut out = String::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let flush = |paragraph: &mut Vec<&str>, out: &mut String| {
        if !paragraph.is_empty() {
            out.push_str(&wrap(&paragraph.join(" "), "", ""));
            paragraph.clear();
        }
    };
    for line in text.trim().lines() {
        let verbatim =
            line.trim().is_empty() || line.starts_with('>') || line.starts_with([' ', '\t']);
        if verbatim {
            flush(&mut paragraph, &mut out);
            out.push_str(line.trim_end());
            out.push('\n');
        } else {
            paragraph.push(line.trim());
        }
    }
    flush(&mut paragraph, &mut out);
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_fits_and_hangs() {
        let text = bullet(
            "High",
            "net: foo: out_unlock frees priv->buf twice on the foo_hw_init() error path",
        );
        assert_eq!(
            text,
            "- [High] net: foo: out_unlock frees priv->buf twice on the foo_hw_init()\n  error path\n"
        );
        let long_word = "x".repeat(80);
        assert_eq!(
            wrap(&format!("a {long_word} b"), "", ""),
            format!("a\n{long_word}\nb\n")
        );
    }

    #[test]
    fn test_findings_header_counts_and_wraps() {
        assert_eq!(
            findings_header(1),
            "Thank you for your contribution! Sashiko AI review found 1 potential\nissue to consider:\n"
        );
        assert!(findings_header(2).contains("2 potential\nissues to consider:"));
    }

    #[test]
    fn test_wrap_prose_keeps_quotes_and_indents() {
        let text = "one two\nthree\n\n> \tif (a_very_long_condition_that_should_never_be_rewrapped_by_us(x, y, z))\n    indented stays\nfour";
        assert_eq!(
            wrap_prose(text),
            "one two three\n\n> \tif (a_very_long_condition_that_should_never_be_rewrapped_by_us(x, y, z))\n    indented stays\nfour"
        );
    }
}
