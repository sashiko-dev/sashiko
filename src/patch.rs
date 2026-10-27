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

use anyhow::{Result, anyhow};
use mail_parser::{HeaderValue, MessageParser};
use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug)]
#[allow(dead_code)]
pub struct PatchsetMetadata {
    pub message_id: String,
    pub subject: String,
    pub author: String,
    pub date: i64,
    pub received_date: Option<i64>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub index: u32,
    pub total: u32,
    pub to: String,
    pub cc: String,
    pub is_patch_or_cover: bool,
    pub version: Option<u32>,
    pub body: String,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Patch {
    pub message_id: String,
    pub body: String,
    pub diff: String,
    pub part_index: u32,
}

pub fn extract_received_date(raw_email: &[u8]) -> Option<i64> {
    let header_end = raw_email
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(raw_email.len());
    let headers_bytes = &raw_email[..header_end];
    let headers_str = String::from_utf8_lossy(headers_bytes);

    let mut current_header = String::new();
    let mut in_received = false;

    for line in headers_str.lines() {
        if line.starts_with("Received:") {
            if in_received {
                break;
            }
            in_received = true;
            current_header.push_str(line);
        } else if in_received && (line.starts_with(' ') || line.starts_with('\t')) {
            current_header.push_str(line);
        } else if in_received {
            break;
        }
    }

    if current_header.is_empty() {
        return None;
    }

    if let Some(semi_idx) = current_header.rfind(';') {
        let date_str = current_header[semi_idx + 1..].trim();
        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(date_str) {
            return Some(dt.timestamp());
        }
    }

    None
}

pub fn has_unquoted_diff(text: &str) -> bool {
    let mut has_minus = false;
    let mut has_plus = false;
    let mut has_hunk = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("diff --git ") {
            return true;
        }
        if line.starts_with("--- ") {
            has_minus = true;
        } else if line.starts_with("+++ ") {
            has_plus = true;
        } else if line.starts_with("@@ -") {
            has_hunk = true;
        }
        if has_minus && has_plus && has_hunk {
            return true;
        }
    }
    false
}

/// Returns true if `line` is a `git format-patch` mbox separator line of the
/// form `From <hex-sha> <ctime-timestamp>` (e.g.
/// `From 0123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001`).
fn is_format_patch_mbox_from(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("From ") else {
        return false;
    };
    let Some((sha, tail)) = rest.split_once(' ') else {
        return false;
    };
    if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let b = tail.trim_end().as_bytes();
    let len = b.len();
    len >= 20 && b[len - 8] == b':' && b[len - 11] == b':' && b[len - 5] == b' '
}

/// Strips only a leading `git format-patch` `From <sha> <timestamp>` mbox
/// separator line when the patch appears at the very start of the message body,
/// leaving any subsequent `From:`, `Date:`, and `Subject:` lines at the top of
/// the body for `git mailinfo` (`git am`) to consume as in-body overrides.
fn strip_leading_mbox_from_line(text: &str) -> &str {
    let Some((first, rest)) = text.split_once('\n') else {
        return text;
    };
    if is_format_patch_mbox_from(first.trim_end_matches('\r')) {
        rest
    } else {
        text
    }
}

/// Strips a leading `git format-patch` mbox/RFC header block (`From <sha> ...`,
/// `From:`, `Date:`, `Subject:`, etc.) from an attached patch so `git am` does
/// not split on an embedded `From ` separator line when the attachment is
/// appended to the email body.
fn strip_format_patch_headers(text: &str) -> &str {
    let first_line = text
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end_matches('\r');
    let starts_with_mbox_from = is_format_patch_mbox_from(first_line);
    let starts_with_header = ["from:", "date:", "subject:"]
        .iter()
        .any(|p| first_line.to_ascii_lowercase().starts_with(p));
    if !starts_with_mbox_from && !starts_with_header {
        return text;
    }

    let mut offset = 0;
    let mut line_idx = 0;
    let mut saw_patch_header = false;

    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        offset += raw_line.len();
        if line.is_empty() {
            if saw_patch_header && line_idx > 0 {
                return &text[offset..];
            }
            return text;
        }
        if line_idx == 0 && starts_with_mbox_from {
            line_idx += 1;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            line_idx += 1;
            continue;
        }
        if let Some((name, _)) = line.split_once(':')
            && !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_graphic() && !c.is_ascii_whitespace())
        {
            let lower = name.to_ascii_lowercase();
            if matches!(lower.as_str(), "from" | "date" | "subject") {
                saw_patch_header = true;
            }
            line_idx += 1;
            continue;
        }
        return text;
    }

    text
}

fn is_ping_subject(subject: &str) -> bool {
    let lower = subject.trim().to_ascii_lowercase();
    if lower.starts_with("ping:") || lower.starts_with("ping ") || lower.starts_with("ping^") {
        return true;
    }
    subject_prefix_tokens(subject).any(|tok| tok.to_ascii_lowercase().starts_with("ping"))
}

pub fn is_promotable_reply_patch_subject(subject: &str) -> bool {
    let lower = subject.trim().to_lowercase();
    let starts_with_reply = lower.starts_with("re:")
        || lower.starts_with("aw:")
        || lower.starts_with("回复:")
        || lower.starts_with("回复：")
        || lower.starts_with("答复:")
        || lower.starts_with("答复：");
    if !starts_with_reply {
        return false;
    }
    if lower.contains("fwd:")
        || lower.contains("forwarded:")
        || lower.contains("wg:")
        || lower.contains("[reproducer]")
        || lower.contains("(was ")
        || lower.contains("(was:")
    {
        return false;
    }
    let has_patch_tag = subject_prefix_tokens(subject)
        .any(|tok| tok.eq_ignore_ascii_case("patch") || tok.eq_ignore_ascii_case("rfc"));
    has_patch_tag && !clean_subject(subject).is_empty()
}

pub fn promote_reply_patch_subject(subject: &str, version: u32) -> String {
    let mut stripped = subject.trim();
    let prefixes = ["re:", "aw:", "回复:", "回复：", "答复:", "答复："];
    loop {
        let lower = stripped.to_lowercase();
        let mut matched = false;
        for prefix in &prefixes {
            if lower.starts_with(prefix)
                && let Some(rest) = stripped.get(prefix.len()..)
            {
                stripped = rest.trim();
                matched = true;
                break;
            }
        }
        if !matched {
            break;
        }
    }

    if version <= 1 || parse_subject_version(stripped) == Some(version) {
        return stripped.to_string();
    }

    static RE_BRACKET_VER: OnceLock<Regex> = OnceLock::new();
    let re_ver =
        RE_BRACKET_VER.get_or_init(|| Regex::new(r"(?i)\[([^\]]*?)\bv\d+\b([^\]]*?)\]").unwrap());
    if let Some(caps) = re_ver.captures(stripped)
        && let Some(m) = caps.get(0)
    {
        let mut out = String::with_capacity(stripped.len() + 4);
        out.push_str(&stripped[..m.start()]);
        out.push_str(&format!("[{}v{}{}]", &caps[1], version, &caps[2]));
        out.push_str(&stripped[m.end()..]);
        return out;
    }

    static RE_BRACKET_TAG: OnceLock<Regex> = OnceLock::new();
    let re_tag = RE_BRACKET_TAG
        .get_or_init(|| Regex::new(r"(?i)\[([^\]]*?\b(?:PATCH|RFC)\b)([^\]]*?)\]").unwrap());
    if let Some(caps) = re_tag.captures(stripped)
        && let Some(m) = caps.get(0)
    {
        let mut out = String::with_capacity(stripped.len() + 8);
        out.push_str(&stripped[..m.start()]);
        out.push_str(&format!("[{} v{}{}]", &caps[1], version, &caps[2]));
        out.push_str(&stripped[m.end()..]);
        return out;
    }

    format!("[PATCH v{version}] {stripped}")
}

pub fn parse_email(raw_email: &[u8]) -> Result<(PatchsetMetadata, Option<Patch>)> {
    let received_date = extract_received_date(raw_email);

    let message = MessageParser::default()
        .with_mime_headers()
        .with_address_headers()
        .with_message_ids()
        .header_text(mail_parser::HeaderName::Subject)
        .header_date(mail_parser::HeaderName::Date)
        .header_address("X-Original-From")
        .parse(raw_email)
        .ok_or_else(|| anyhow!("Failed to parse email"))?;

    let message_id = clean_display_name(
        message
            .message_id()
            .ok_or_else(|| anyhow!("No Message-ID header"))?,
    );
    if message_id.is_empty() {
        return Err(anyhow!("Empty Message-ID header"));
    }

    let raw_subject = clean_display_name(message.subject().unwrap_or("(no subject)"));
    let subject = if raw_subject.is_empty() {
        "(no subject)".to_string()
    } else {
        raw_subject
    };

    let mut author = message
        .from()
        .and_then(|addr| addr.first())
        .map(|a| {
            let name = &clean_display_name(a.name().unwrap_or_default());
            let address = a.address().unwrap_or("unknown@localhost").trim();
            let addr = if address.is_empty() || !address.contains('@') {
                "unknown@localhost"
            } else {
                address
            };
            if name.is_empty() || name.to_lowercase() == "unknown" {
                addr.to_string()
            } else {
                format_mailbox(name, addr)
            }
        })
        .unwrap_or_else(|| "unknown@localhost".to_string());

    if let Some(first_addr) = message.header("X-Original-From").and_then(|h| match h {
        mail_parser::HeaderValue::Address(x_orig_addr) => x_orig_addr.first(),
        _ => None,
    }) {
        let name = &clean_display_name(first_addr.name().unwrap_or_default());
        let address = first_addr.address().unwrap_or("").trim();
        let author_email = extract_email(&author);
        if !address.is_empty() && address.contains('@') {
            if is_b4_alias(&author_email, address) {
                author = if name.is_empty() || name.to_lowercase() == "unknown" {
                    address.to_string()
                } else {
                    format_mailbox(name, address)
                };
            } else if author_email.to_lowercase().starts_with("devnull+") {
                tracing::warn!(
                    "Ignoring unverified X-Original-From header due to alias mismatch with From header"
                );
            }
        }
    }

    let date = message.date().map(|d| d.to_timestamp()).unwrap_or(0);

    let to = message
        .to()
        .map(|addr| {
            addr.iter()
                .filter_map(|a| {
                    let address = a.address().unwrap_or("").trim();
                    if address.is_empty() || !address.contains('@') {
                        None
                    } else {
                        let name = &clean_display_name(a.name().unwrap_or_default());
                        if name.is_empty() {
                            Some(address.to_string())
                        } else {
                            Some(format_mailbox(name, address))
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let cc = message
        .cc()
        .map(|addr| {
            addr.iter()
                .filter_map(|a| {
                    let address = a.address().unwrap_or("").trim();
                    if address.is_empty() || !address.contains('@') {
                        None
                    } else {
                        let name = &clean_display_name(a.name().unwrap_or_default());
                        if name.is_empty() {
                            Some(address.to_string())
                        } else {
                            Some(format_mailbox(name, address))
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let in_reply_to = match message.in_reply_to() {
        HeaderValue::Text(t) => Some(clean_display_name(t)).filter(|s| !s.is_empty()),
        HeaderValue::TextList(l) => l
            .iter()
            .map(|s| clean_display_name(s))
            .find(|s| !s.is_empty()),
        _ => None,
    };

    let references = match message.references() {
        HeaderValue::Text(t) => {
            let cleaned = clean_display_name(t);
            if cleaned.is_empty() {
                vec![]
            } else {
                vec![cleaned]
            }
        }
        HeaderValue::TextList(l) => l
            .iter()
            .map(|s| clean_display_name(s))
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    };

    let (index, total) = parse_subject_index(&subject);
    let version = parse_subject_version(&subject);

    let mut body = String::new();
    for i in 0..message.text_body_count() {
        if let Some(text) = message.body_text(i) {
            let normalized = text.replace("\r\n", "\n");
            let content = if has_unquoted_diff(&normalized) {
                if body.trim().is_empty() {
                    body.clear();
                    strip_leading_mbox_from_line(&normalized)
                } else {
                    strip_format_patch_headers(&normalized)
                }
            } else {
                normalized.as_str()
            };
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(content);
        }
    }

    for attachment in message.attachments() {
        if let Some(att_text) = attachment.text_contents()
            && has_unquoted_diff(att_text)
        {
            let normalized = att_text.replace("\r\n", "\n");
            let stripped = if body.trim().is_empty() {
                body.clear();
                strip_leading_mbox_from_line(&normalized)
            } else {
                strip_format_patch_headers(&normalized)
            };
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(stripped);
        }
    }

    let diff = if has_unquoted_diff(&body) {
        body.clone()
    } else {
        String::new()
    };

    // Detection logic
    let subject_lower = subject.to_lowercase();
    let subject_clean = subject_lower.trim();
    let has_diff = !diff.is_empty();
    let is_reply = subject_clean.starts_with("re:")
        || subject_clean.starts_with("fwd:")
        || subject_clean.starts_with("forwarded:")
        || subject_clean.starts_with("aw:") // German 'Antwort'
        || subject_clean.starts_with("wg:") // German 'Weitergeleitet'
        || subject_clean.starts_with("回复:") // Chinese 'Re'
        || subject_clean.starts_with("回复：") // Chinese 'Re' with full-width colon
        || subject_clean.starts_with("答复:") // Chinese 'Reply'
        || subject_clean.starts_with("答复：") // Chinese 'Reply'
        || subject_clean.starts_with("[reproducer]") // Reproducers
        || subject_lower.contains("(was ")
        || subject_lower.contains("(was:")
        || (!has_diff && is_ping_subject(&subject));
    let has_patch_tag = subject_clean.contains("patch") || subject_clean.contains("rfc");

    // A message is part of a series if it's a cover letter (index 0) or has multiple parts (total > 1)
    let is_series_metadata = total > 1 || index == 0;

    // It is a patch or cover letter if:
    // 1. It is NOT a reply (Re: ...)
    // 2. AND (It has [PATCH]/[RFC] tag OR it has a diff OR it looks like a series cover letter/part)
    let is_patch_or_cover = !is_reply && (has_patch_tag || has_diff || is_series_metadata);

    let metadata = PatchsetMetadata {
        message_id: message_id.clone(),
        subject,
        author,
        date,
        received_date,
        in_reply_to,
        references,
        index,
        total,
        to,
        cc,
        is_patch_or_cover,
        version,
        body: body.clone(),
    };

    let patch = if has_diff && index != 0 {
        Some(Patch {
            message_id,
            body,
            diff,
            part_index: index,
        })
    } else {
        None
    };

    Ok((metadata, patch))
}

/// Read the `M/N` part counter out of a patch subject, as `(index, total)`.
/// A subject that carries no counter reads as part 1 of 1.
pub fn parse_subject_index(subject: &str) -> (u32, u32) {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    // Match [ ... M/N ... ] but strictly require PATCH, RFC, RESEND, COMMIT or vN before the M/N
    let re_brackets = RE_BRACKETS.get_or_init(|| {
        Regex::new(r"(?i)\[.*?\b(?:PATCH|RFC|RESEND|COMMIT|v\d+)\b.*?(\d+)/(\d+).*?\]").unwrap()
    });

    if let Some(caps) = re_brackets.captures(subject)
        && let (Some(i), Some(t)) = (caps.get(1), caps.get(2))
    {
        let index = i.as_str().parse().unwrap_or(1);
        let total = t.as_str().parse().unwrap_or(1);
        return (index, total);
    }

    static RE_LOOSE: OnceLock<Regex> = OnceLock::new();
    // Match PATCH M/N or RFC M/N (case insensitive)
    let re_loose = RE_LOOSE
        .get_or_init(|| Regex::new(r"(?i)\b(?:PATCH|RFC|RESEND|COMMIT)\s+(\d+)/(\d+)\b").unwrap());

    if let Some(caps) = re_loose.captures(subject)
        && let (Some(i), Some(t)) = (caps.get(1), caps.get(2))
    {
        let index = i.as_str().parse().unwrap_or(1);
        let total = t.as_str().parse().unwrap_or(1);
        return (index, total);
    }

    // Check cleaned subject for "1/2" at start (Handles "[PATCH] 1/2")
    let cleaned = clean_subject(subject);
    static RE_START: OnceLock<Regex> = OnceLock::new();
    let re_start = RE_START.get_or_init(|| Regex::new(r"^\s*(\d+)/(\d+)\b").unwrap());
    if let Some(caps) = re_start.captures(&cleaned)
        && let (Some(i), Some(t)) = (caps.get(1), caps.get(2))
    {
        let index = i.as_str().parse().unwrap_or(1);
        let total = t.as_str().parse().unwrap_or(1);
        return (index, total);
    }

    (1, 1)
}

pub fn parse_subject_version(subject: &str) -> Option<u32> {
    static RE_VER: OnceLock<Regex> = OnceLock::new();
    // Strategy:
    // 1. Inside [...] blocks: find vN preceded by word boundary (e.g. [PATCH v2], [v2])
    // 2. Start of string: ^vN followed by word boundary
    // 3. Following PATCH: PATCH followed by non-word chars and vN (e.g. PATCHv2, [PATCH] v2)
    let re = RE_VER.get_or_init(|| {
        Regex::new(r"(?i)(?:\[[^\]]*?\bv(\d+)\b[^\]]*?\]|^\s*v(\d+)\b|PATCH\W*v(\d+)\b)").unwrap()
    });

    if let Some(caps) = re.captures(subject) {
        if let Some(m) = caps.get(1) {
            return m.as_str().parse().ok();
        }
        if let Some(m) = caps.get(2) {
            return m.as_str().parse().ok();
        }
        if let Some(m) = caps.get(3) {
            return m.as_str().parse().ok();
        }
    }
    None
}

/// Returns bracketed subject tokens in order, including standard patch tags.
pub(crate) fn subject_prefix_tokens(subject: &str) -> impl Iterator<Item = &str> {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    let re = RE_BRACKETS.get_or_init(|| Regex::new(r"\[(.*?)\]").unwrap());

    re.captures_iter(subject)
        .filter_map(|cap| cap.get(1))
        .flat_map(|content| {
            content
                .as_str()
                .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '.' && c != '_')
        })
        .filter(|token| !token.is_empty())
}

pub fn get_subject_prefixes(subject: &str) -> Vec<String> {
    let mut prefixes = Vec::new();

    for token in subject_prefix_tokens(subject) {
        let lower = token.to_lowercase();
        // Ignore standard tags
        if lower == "patch" || lower == "rfc" || lower == "resend" {
            continue;
        }
        // Ignore versions (v2, v10)
        if lower.starts_with('v') && lower[1..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // Ignore pure numbers (often part of 1/2 or just garbage)
        if token.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        prefixes.push(token.to_string());
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

pub fn clean_subject(subject: &str) -> String {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    let re = RE_BRACKETS.get_or_init(|| Regex::new(r"\[.*?\]").unwrap());

    // 1. Remove [...] blocks
    let no_brackets = re.replace_all(subject, "");

    // 2. Remove Re:, Fwd: prefixes (case insensitive)
    let mut cleaned = no_brackets.trim().to_string();
    let prefixes = [
        "re:",
        "fwd:",
        "aw:",
        "wg:",
        "forwarded:",
        "回复:",
        "回复：",
        "答复:",
        "答复：",
    ];

    let mut changed = true;
    while changed {
        changed = false;
        let lower = cleaned.to_lowercase();
        for prefix in &prefixes {
            if lower.starts_with(prefix)
                && let Some(rest) = cleaned.get(prefix.len()..)
            {
                cleaned = rest.trim().to_string();
                changed = true;
                break;
            }
        }
    }

    cleaned
}

/// Strips control characters from a display name. An RFC 2047 encoded word
/// can decode to CR/LF, which lettre cannot write into a mailbox header.
pub fn clean_display_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

fn format_mailbox(name: &str, address: &str) -> String {
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\" <{}>", escaped, address)
}

/// Splits a comma-separated recipient list into individual entries without
/// splitting on commas inside double-quoted display names.
pub fn split_address_list(list: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for ch in list.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if in_quotes && ch == '\\' {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
            continue;
        }
        if ch == ',' && !in_quotes {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                result.push(trimmed.to_string());
            }
            current.clear();
        } else {
            current.push(ch);
        }
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        result.push(trimmed.to_string());
    }

    result
}

pub fn extract_email(author: &str) -> String {
    if let Some(start) = author.find('<')
        && let Some(end) = author.find('>')
        && end > start
    {
        return author[start + 1..end].trim().to_string();
    }
    author.trim().to_string()
}

pub fn authors_match(a: &str, b: &str) -> bool {
    let email_a = extract_email(a);
    let email_b = extract_email(b);

    if email_a == email_b {
        return true;
    }

    is_b4_alias(&email_a, &email_b) || is_b4_alias(&email_b, &email_a)
}

fn is_b4_alias(alias: &str, real: &str) -> bool {
    let alias_lower = alias.to_lowercase();
    if !alias_lower.starts_with("devnull+") {
        return false;
    }
    let Some(at_idx) = alias_lower.rfind('@') else {
        return false;
    };
    if at_idx <= 8 {
        return false;
    }

    let encoded_part = &alias_lower[8..at_idx];
    let alias_domain = &alias_lower[at_idx + 1..];

    if alias_domain != "kernel.org" && alias_domain != "linux.dev" {
        return false;
    }

    let real_lower = real.to_lowercase();
    encoded_part == real_lower.replace('@', ".")
}

pub fn extract_changelog_from_body(body: &str) -> Option<String> {
    let lines = body.lines();
    let mut found_separator = false;
    let mut changelog = String::new();

    for line in lines {
        if !found_separator {
            let trimmed = line.trim();
            if trimmed == "---" {
                found_separator = true;
            }
        } else {
            if line.starts_with("diff --git ")
                || line.starts_with("--- a/")
                || line.starts_with("--- /dev/null")
            {
                break;
            }
            changelog.push_str(line);
            changelog.push('\n');
        }
    }

    if found_separator {
        let trimmed_changelog = changelog.trim().to_string();
        if !trimmed_changelog.is_empty() {
            return Some(trimmed_changelog);
        }
    }
    None
}

pub fn inject_changelog_into_git_show(git_show: &str, changelog: &str) -> String {
    if let Some(pos) = git_show.find("diff --git ") {
        let (header, diff) = git_show.split_at(pos);
        let mut result = header.to_string();
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("---\n");
        result.push_str(changelog);
        result.push('\n');
        if !changelog.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(diff);
        result
    } else {
        format!("{}\n---\n{}\n", git_show, changelog)
    }
}

const MAX_TRAILER_SCAN_LINES: usize = 2000;
const MAX_TRAILER_LINE_BYTES: usize = 2048;
const MAX_EXTRACTED_REF_IDS: usize = 128;

fn is_valid_b4_change_id_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Metadata extracted from a `b4 prep` empty cover-letter / tracking commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct B4TrackerCommit {
    pub change_id: Option<String>,
    pub edited_subject: Option<String>,
}

/// Detect and parse a `b4 prep` empty tracking commit (`--- b4-submit-tracking ---`).
/// Returns `None` if the commit has a non-empty diff or lacks the tracking marker.
/// Unedited `EDITME` / `*** SUBJECT HERE ***` subject placeholders
/// produce `edited_subject: None`.
pub fn parse_b4_submit_tracking(
    subject: &str,
    message: &str,
    diff: &str,
) -> Option<B4TrackerCommit> {
    if !diff.trim().is_empty() {
        return None;
    }
    if !message
        .lines()
        .take(MAX_TRAILER_SCAN_LINES)
        .any(|l| l.trim() == "--- b4-submit-tracking ---")
    {
        return None;
    }

    let change_id = parse_b4_change_id(message);

    let cleaned_subj = clean_subject(subject);
    let subj_trimmed = cleaned_subj.trim();
    let subj_upper = subj_trimmed.to_ascii_uppercase();
    let edited_subject = if subj_trimmed.is_empty()
        || subj_upper.starts_with("EDITME")
        || subj_trimmed == "*** SUBJECT HERE ***"
    {
        None
    } else {
        Some(subject.trim().to_string())
    };

    Some(B4TrackerCommit {
        change_id,
        edited_subject,
    })
}

/// Extract the `b4` `change-id` from a cover letter, patch body, or
/// `--- b4-submit-tracking ---` JSON block.
/// Stops scanning at the start of a unified diff and ignores quoted lines or
/// `prerequisite-change-id:` trailers.
pub fn parse_b4_change_id(body: &str) -> Option<String> {
    let mut in_b4_tracking_block = false;
    for line in body.lines().take(MAX_TRAILER_SCAN_LINES) {
        if line.starts_with("diff --git ")
            || line.starts_with("--- a/")
            || line.starts_with("--- /dev/null")
        {
            break;
        }
        let trimmed = line.trim();
        if trimmed == "--- b4-submit-tracking ---" {
            in_b4_tracking_block = true;
            continue;
        }
        if trimmed.len() > MAX_TRAILER_LINE_BYTES || trimmed.starts_with('>') {
            continue;
        }
        if in_b4_tracking_block {
            if trimmed.starts_with('#') {
                continue;
            }
            if let Some(pos) = trimmed.find("\"change-id\"") {
                let after_key = &trimmed[pos + "\"change-id\"".len()..];
                if let Some((_, rhs)) = after_key.split_once(':') {
                    let rhs = rhs.trim_start();
                    if let Some(rest) = rhs.strip_prefix('"')
                        && let Some(end_quote) = rest.find('"')
                    {
                        let token = &rest[..end_quote];
                        if is_valid_b4_change_id_token(token) {
                            return Some(token.to_string());
                        }
                    }
                }
            }
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("change-id") {
            continue;
        };
        let token = value.trim();
        if is_valid_b4_change_id_token(token) {
            return Some(token.to_string());
        }
    }
    None
}

fn normalize_candidate_msgid(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_matches(|c: char| {
            matches!(
                c,
                '<' | '>' | '(' | ')' | '[' | ']' | '"' | '\'' | ',' | '.'
            )
        })
        .trim_end_matches('/');
    if trimmed.is_empty() || trimmed.len() > MAX_TRAILER_LINE_BYTES || !trimmed.is_ascii() {
        return None;
    }
    let decoded = if trimmed.contains("%40") || trimmed.contains("%2B") || trimmed.contains("%2b") {
        trimmed
            .replace("%40", "@")
            .replace("%2B", "+")
            .replace("%2b", "+")
    } else {
        trimmed.to_string()
    };
    if decoded.is_empty()
        || decoded.len() > 998
        || decoded
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        || decoded.contains(['<', '>', ':'])
    {
        return None;
    }
    let (local, domain) = decoded.split_once('@')?;
    if local.is_empty()
        || local.starts_with('/')
        || local.ends_with('/')
        || domain.is_empty()
        || !domain.contains('.')
        || domain.contains('/')
    {
        return None;
    }
    Some(decoded)
}

fn lore_url_regex() -> &'static Regex {
    static RE_LORE_URL: OnceLock<Regex> = OnceLock::new();
    RE_LORE_URL.get_or_init(|| {
        Regex::new(
            r"(?i)https?://(?:lore\.kernel\.org|lkml\.kernel\.org|patchwork\.kernel\.org|patch\.msgid\.link)/(?:project/[a-z0-9._-]+/(?:patch|cover)/|(?:patch|cover)/|r/|all/|lkml/|[a-z0-9._-]+/)?([^\s?#<>()\[\]]+(?:@|%40)[^/\s?#<>()\[\]]+)",
        )
        .expect("static Lore URL regex must compile")
    })
}

/// Matches a line that starts with an explicit previous-version marker
/// immediately followed by a link to that version: "v1: <url>",
/// "[v1] <url>", "v1 link: <url>", "v7 (with test steps): <url>",
/// "v1 (<url>)", b4's "- Link to v1: <url>", "Link[v1]: <url>",
/// "Previous version: <url>", "Version 1: <url>" and
/// "Changes since v1 (<url>):".
///
/// The version token must be followed by whitespace or a separator, so
/// kernel versions such as "v6.8: <url>" never match, and the link must
/// follow the marker directly, so "v2 of the base series: <url>" or a
/// "Link:" trailer below a changelog heading do not count either.
fn version_link_line_regex() -> &'static Regex {
    static RE_VERSION_LINK_LINE: OnceLock<Regex> = OnceLock::new();
    RE_VERSION_LINK_LINE.get_or_init(|| {
        Regex::new(
            r"(?ix)
            ^(?:[-*+]\s*)?
            (?:
                (?:\[\s*v\d+\s*\]|v\d+)(?:\s+links?)?(?:\s*\([^()]*\))?
              | links?\s*(?:to\s+)?(?:\[\s*v\d+\s*\]|v\d+)
              | (?:previous|prior|earlier|last)\s+(?:version|revision|posting|iteration)
                (?:\s*\([^()]*\))?
              | version\s+\d+
              | changes\s+(?:since|from)\s+(?:\[\s*v\d+\s*\]|v\d+)
            )
            (?:
                (?:\s*[:=]|\s*-+>?|\s+at\b|\s)\s*[(<\[]?\s*
              | \s*[(\[]\s*
            )
            (?P<link>
                https?://(?:lore\.kernel\.org|lkml\.kernel\.org|patchwork\.kernel\.org|patch\.msgid\.link)/\S+
              | <[^<>\s]+@[^<>\s]+>
              | [^\s<>()\[\]]+@[^\s<>()\[\]]+
            )",
        )
        .expect("static version link line regex must compile")
    })
}

fn extract_msgid_or_lore_url(value: &str) -> Option<String> {
    if value.len() > MAX_TRAILER_LINE_BYTES {
        return None;
    }
    if let Some(msgid) = lore_url_regex()
        .captures(value)
        .and_then(|cap| cap.get(1))
        .and_then(|m| normalize_candidate_msgid(m.as_str()))
    {
        return Some(msgid);
    }
    normalize_candidate_msgid(value)
}

/// Extract the Message-IDs of earlier versions of the same patch or series
/// that the author links explicitly in a cover letter or patch body.
///
/// Only lines that start with a previous-version marker and carry the link
/// on the same line count (see `version_link_line_regex`). Threading
/// headers, unmarked Lore links (including any link below a changelog
/// heading that has no marker of its own), kernel-version prose such as
/// "since v6.8", quoted lines, and prerequisite trailers (`Based-on:`,
/// `prerequisite-message-id:`) never name a previous version.
pub fn extract_version_reference_msgids(body: &str) -> Vec<String> {
    let re_line = version_link_line_regex();
    let (prereq_msgids_vec, _) = extract_prerequisite_message_and_patch_ids(body);
    let prereq_msgids: std::collections::HashSet<String> = prereq_msgids_vec.into_iter().collect();

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in body.lines().take(MAX_TRAILER_SCAN_LINES) {
        if out.len() >= MAX_EXTRACTED_REF_IDS {
            break;
        }
        if line.starts_with("diff --git ")
            || line.starts_with("--- a/")
            || line.starts_with("--- /dev/null")
        {
            break;
        }
        let trimmed = line.trim();
        if trimmed.len() > MAX_TRAILER_LINE_BYTES || trimmed.starts_with('>') {
            continue;
        }
        let Some(link) = re_line
            .captures(trimmed)
            .and_then(|cap| cap.name("link"))
            .map(|m| m.as_str())
        else {
            continue;
        };
        if let Some(msgid) = extract_msgid_or_lore_url(link)
            && !prereq_msgids.contains(&msgid)
            && seen.insert(msgid.clone())
        {
            out.push(msgid);
        }
    }

    out
}

/// Extract prerequisite series Message-IDs (`Based-on:`, `prerequisite-message-id:`)
/// and prerequisite patch IDs (`prerequisite-patch-id:`) from a cover letter or
/// patch body so version discovery never mistakes a prerequisite series by the
/// same author for an earlier version of the current series.
pub fn extract_prerequisite_message_and_patch_ids(body: &str) -> (Vec<String>, Vec<String>) {
    let mut msgids = Vec::new();
    let mut seen_msgids = std::collections::HashSet::new();
    let mut patch_ids = Vec::new();
    let mut seen_patch_ids = std::collections::HashSet::new();

    for line in body.lines().take(MAX_TRAILER_SCAN_LINES) {
        if line.starts_with("diff --git ")
            || line.starts_with("--- a/")
            || line.starts_with("--- /dev/null")
        {
            break;
        }
        let trimmed = line.trim();
        if trimmed.len() > MAX_TRAILER_LINE_BYTES || trimmed.starts_with('>') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("based-on")
            || key.eq_ignore_ascii_case("prerequisite-message-id")
        {
            if msgids.len() < MAX_EXTRACTED_REF_IDS
                && let Some(msgid) = extract_msgid_or_lore_url(value)
                && seen_msgids.insert(msgid.clone())
            {
                msgids.push(msgid);
            }
        } else if key.eq_ignore_ascii_case("prerequisite-patch-id")
            && patch_ids.len() < MAX_EXTRACTED_REF_IDS
            && value.len() == 40
            && value.bytes().all(|b| b.is_ascii_hexdigit())
        {
            let lower = value.to_ascii_lowercase();
            if seen_patch_ids.insert(lower.clone()) {
                patch_ids.push(lower);
            }
        }
    }

    (msgids, patch_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_authors_match() {
        assert!(authors_match(
            "Real Author <author@example.com>",
            "Real Author <author@example.com>"
        ));
        assert!(authors_match(
            "devnull+author.example.com@kernel.org",
            "author@example.com"
        ));
        assert!(authors_match(
            "Real Author <author@example.com>",
            "Real Author via B4 Relay <devnull+author.example.com@kernel.org>"
        ));
        assert!(!authors_match("other@example.com", "author@example.com"));
        assert!(!authors_match(
            "devnull+author.example.com@spam.org",
            "author@example.com"
        ));
    }

    #[test]
    fn test_extract_email() {
        assert_eq!(
            extract_email("Name <email@example.com>"),
            "email@example.com"
        );
        assert_eq!(extract_email("email@example.com"), "email@example.com");
        assert_eq!(extract_email(" <email@example.com> "), "email@example.com");
        assert_eq!(
            extract_email("Name < email@example.com >"),
            "email@example.com"
        );
        assert_eq!(extract_email("Invalid < Format"), "Invalid < Format");
    }

    #[test]
    fn test_extract_received_date() {
        let email = b"Received: from mail.example.com ([192.0.2.1])\r\n \
by mail.example.org with ESMTPS ;\r\n \
Tue, 12 May 2026 00:12:30 +0000\r\n\
\r\n\
Body";
        let expected = chrono::DateTime::parse_from_rfc2822("Tue, 12 May 2026 00:12:30 +0000")
            .unwrap()
            .timestamp();
        assert_eq!(extract_received_date(email), Some(expected));

        let email_no_received = b"Subject: Test\r\n\
            \r\n\
            Body";
        assert_eq!(extract_received_date(email_no_received), None);

        let email_malformed_date = b"Received: from ... ; Invalid Date\r\n\
            \r\n\
            Body";
        assert_eq!(extract_received_date(email_malformed_date), None);
    }

    #[test]
    fn test_clean_subject() {
        assert_eq!(clean_subject("[PATCH] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("[PATCH v2] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("[PATCH 1/2] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("Re: [PATCH] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("[PATCH] Re: Fix bug"), "Fix bug"); // "[PATCH] " removed, then "Re: Fix bug" -> "Fix bug"
        assert_eq!(clean_subject("Subject only"), "Subject only");
        assert_eq!(
            clean_subject("[RFC] [PATCH v3] Complex subject"),
            "Complex subject"
        );
    }

    #[test]
    fn test_clean_subject_chinese() {
        assert_eq!(clean_subject("回复: [PATCH] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("回复：[PATCH] Fix bug"), "Fix bug");
        assert_eq!(clean_subject("回复：回复：[PATCH] Fix bug"), "Fix bug");
    }

    #[test]
    fn test_chinese_reply() {
        let raw =
            b"Message-ID: <reply>\r\nSubject: \xE5\x9B\x9E\xE5\xA4\x8D: [PATCH] fix\r\n\r\nBody";
        // \xE5\x9B\x9E\xE5\xA4\x8D is "回复" in UTF-8.
        let (meta, _) = parse_email(raw).unwrap();
        assert!(
            !meta.is_patch_or_cover,
            "Chinese reply should not be a patchset"
        );
    }

    #[test]
    fn test_author_parsing() {
        let raw =
            b"Message-ID: <123>\r\nFrom: Test User <test@example.com>\r\nSubject: Test\r\n\r\nBody";
        let (meta, _) = parse_email(raw).unwrap();
        assert_eq!(meta.author, "\"Test User\" <test@example.com>");

        let raw_no_name =
            b"Message-ID: <456>\r\nFrom: test2@example.com\r\nSubject: Test\r\n\r\nBody";
        let (meta2, _) = parse_email(raw_no_name).unwrap();
        assert_eq!(meta2.author, "test2@example.com");

        let raw_malformed =
            b"Message-ID: <789>\r\nFrom: \"fqr\" <user.email>\r\nSubject: Test\r\n\r\nBody";
        let (meta3, _) = parse_email(raw_malformed).unwrap();
        assert_eq!(meta3.author, "\"fqr\" <unknown@localhost>");
    }

    #[test]
    fn test_b4_relay_author_parsing() {
        let raw = b"Message-ID: <b4-relay-test>\r\n\
                    From: Real Author via B4 Relay <devnull+author.example.com@kernel.org>\r\n\
                    X-Original-From: Real Author <author@example.com>\r\n\
                    Subject: Test B4 Relay\r\n\r\nBody";
        let (meta, _) = parse_email(raw).unwrap();
        assert_eq!(meta.author, "\"Real Author\" <author@example.com>");
    }

    #[test]
    fn test_b4_relay_mismatch_fallback() {
        // Verification: If From is a B4 Relay alias, but X-Original-From is a completely
        // different address, Sashiko ignores the override and falls back to the From alias.
        let raw_mismatch = b"Message-ID: <mismatch-test-2>\r\n\
                             From: Real Author via B4 Relay <devnull+author.example.com@kernel.org>\r\n\
                             X-Original-From: Author Two <author2@example.com>\r\n\
                             Subject: Normal Patch\r\n\r\nBody";
        let (meta_mismatch, _) = parse_email(raw_mismatch).unwrap();
        assert_eq!(
            meta_mismatch.author,
            "\"Real Author via B4 Relay\" <devnull+author.example.com@kernel.org>"
        );
    }

    #[test]
    fn test_recipient_parsing() {
        let raw = b"Message-ID: <1>\r\nFrom: a@b.com\r\nTo: \"Valid User\" <valid@example.com>, invalid_no_at, <another@test.com>\r\nCc: Bad <bad>, \"Good\" <good@example.com>\r\nSubject: Test\r\n\r\nBody";
        let (meta, _) = parse_email(raw).unwrap();
        assert_eq!(
            meta.to,
            "\"Valid User\" <valid@example.com>, another@test.com"
        );
        assert_eq!(meta.cc, "\"Good\" <good@example.com>");
    }

    #[test]
    fn test_reply_with_diff_is_not_patchset() {
        // A message that starts with Re: but contains diff --git
        // This simulates a reply quoting a patch or sending an inline fixup
        let raw = b"Message-ID: <123>\r\nSubject: Re: [PATCH] fix bug\r\n\r\n> diff --git a/file b/file\n> index...";
        let (meta, _) = parse_email(raw).unwrap();

        // This fails with current logic because has_diff is true
        assert!(
            !meta.is_patch_or_cover,
            "Reply with diff should NOT be a patchset"
        );
    }

    #[test]
    fn test_diff_without_patch_tag_ignored() {
        let raw = b"Message-ID: <diffnopatch>\r\nSubject: Random fix\r\n\r\ndiff --git a/file b/file\nindex...";
        let (meta, _) = parse_email(raw).unwrap();
        assert!(
            meta.is_patch_or_cover,
            "Diff without [PATCH] tag should still be parsed as patch"
        );
    }

    #[test]
    fn test_normal_patch() {
        let raw = b"Message-ID: <456>\r\nSubject: [PATCH] fix bug\r\n\r\ndiff --git a/file b/file\nindex...";
        let (meta, _) = parse_email(raw).unwrap();
        assert!(meta.is_patch_or_cover);
    }

    #[test]
    fn test_single_patch_no_diff_ignored() {
        let raw =
            b"Message-ID: <nonpatch>\r\nSubject: [PATCH] discussion\r\n\r\nThis is not a patch";
        let (meta, _) = parse_email(raw).unwrap();
        assert!(
            meta.is_patch_or_cover,
            "Single patch without diff should still be parsed due to [PATCH] tag"
        );
    }

    #[test]
    fn test_cover_letter() {
        let raw = b"Message-ID: <789>\r\nSubject: [PATCH 0/5] fix bug\r\n\r\nCover letter body";
        let (meta, patch) = parse_email(raw).unwrap();
        assert!(meta.is_patch_or_cover);
        assert!(patch.is_none());
    }

    #[test]
    fn test_cover_letter_with_diff() {
        let raw = b"Message-ID: <cover_with_diff>\r\nSubject: [PATCH 0/5] fix bug\r\n\r\nExplanation:\ndiff --git a/file b/file\nindex...";
        let (meta, patch) = parse_email(raw).unwrap();
        assert!(meta.is_patch_or_cover);
        assert!(
            patch.is_none(),
            "Cover letter (index 0) with diff should NOT be a patch"
        );
    }

    #[test]
    fn test_pure_reply() {
        let raw = b"Message-ID: <abc>\r\nSubject: Re: [PATCH] fix bug\r\n\r\nLGTM";
        let (meta, _) = parse_email(raw).unwrap();
        assert!(!meta.is_patch_or_cover);
    }

    #[test]
    fn test_rfc_patch_parsing() {
        let subject = "[RFC PATCH 1/3] My RFC";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 3);
    }

    #[test]
    fn test_version_parsing() {
        assert_eq!(parse_subject_version("[PATCH v2] subject"), Some(2));
        assert_eq!(parse_subject_version("[PATCH v3 1/2] subject"), Some(3));
        assert_eq!(parse_subject_version("[PATCH] subject"), None); // v1 implicit
        assert_eq!(parse_subject_version("[RFC v4] subject"), Some(4));
        assert_eq!(parse_subject_version("[PATCH -v2] subject"), Some(2));
        assert_eq!(parse_subject_version("Subject with v2 inside"), None); // v2 ignored
        assert_eq!(parse_subject_version("Subject with devicetree"), None); // 'dev' should not match
        assert_eq!(parse_subject_version("[PATCH 0/10]"), None); // 0/10 is not version
        assert_eq!(parse_subject_version("[PATCH v12]"), Some(12));

        // New cases from analysis
        assert_eq!(parse_subject_version("[PATCH V2 13/13]"), Some(2)); // Uppercase V
        assert_eq!(parse_subject_version("[PATCH bpf-next v5 10/10]"), Some(5)); // Subsystem prefix
        assert_eq!(parse_subject_version("[PATCH RFC v2 8/8]"), Some(2)); // RFC + Version
        assert_eq!(parse_subject_version("[PATCHv5 2/2]"), Some(5)); // Attached version
        assert_eq!(parse_subject_version("[PATCH 00/33 v6]"), Some(6)); // Version at end
        assert_eq!(parse_subject_version("[v3 PATCH 1/1]"), Some(3)); // Version at start

        // Edge case: [PATCH] v3: ...
        assert_eq!(parse_subject_version("[PATCH] v3: subject"), Some(3));
    }

    #[test]
    fn test_complex_prefix_parsing() {
        let subject = "[PATCH v2 net-next 02/14] Something";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 2);
        assert_eq!(total, 14);
    }

    #[test]
    fn test_no_patch_prefix_parsing() {
        // Some lists might just use [RFC 1/2]
        let subject = "[RFC 1/2] Just RFC";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 2);
    }

    #[test]
    fn test_missed_cover_letter_parsing() {
        let subject = "[PATCH 6.18 000/430] 6.18.3-rc1 review";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 0);
        assert_eq!(total, 430);

        let raw = format!("Message-ID: <123>\r\nSubject: {}\r\n\r\nBody", subject);
        let (meta, _) = parse_email(raw.as_bytes()).unwrap();
        assert!(meta.is_patch_or_cover, "Should be detected as patch/cover");
    }

    #[test]
    fn test_forwarded_reply_is_not_patch() {
        // "Forwarded: Re: ..." should be treated as reply/skip if it has no diff,
        // or if it has diff but looks like a forwarded reply.
        // If it has diff, it might be a forwarded patch.
        // But if it starts with "Re:", it's usually a reply.
        // "Forwarded: Re:" -> effectively a reply.
        let subject = "Forwarded: Re: [syzbot] WARNING in cm109_urb_irq_callback";
        let raw = format!(
            "Message-ID: <456>\r\nSubject: {}\r\n\r\nDiff:\n--- a\n+++ b\n@@ -1 +1 @@",
            subject
        );
        let (meta, _) = parse_email(raw.as_bytes()).unwrap();

        // Current logic might think this is a patch because it has diff and doesn't start with "Re:" (starts with "Forwarded:")
        // We want to ensure it is handled correctly (either as patch if it IS a patch, or ignored if it's just a reply).
        // If it's "Forwarded: Re:", it's likely a discussion.
        // Let's assert what we expect. I expect it NOT to be a patchset root.
        assert!(
            !meta.is_patch_or_cover,
            "Forwarded Re: should not be a patchset"
        );
    }

    #[test]
    fn test_loose_patch_parsing() {
        // Case 1: PATCH prefix
        let subject = "PATCH 1/2: Subject";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 2);

        // Case 2: Start of string
        let subject = "1/2: Subject";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 2);

        // Case 3: Leading zeros
        let subject = "[PATCH 01/02] Subject";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 2);

        // Case 4: Regression test for false positive (508/512)
        let subject = "[PATCH v3] serial: 8250_pci: Fix broken RS485 for F81504/508/512";
        let (index, total) = parse_subject_index(subject);
        assert_eq!(index, 1);
        assert_eq!(total, 1); // Should be 1/1, NOT 508/512
    }

    #[test]
    fn test_subject_prefix_tokens_preserve_order() {
        let subject = "Re: [RESEND PATCH v2][STABLE V6.6-v6.1][net-next,bpf,net-next,1/2] \
                       fix stable USB 3.0";
        assert_eq!(
            subject_prefix_tokens(subject).collect::<Vec<_>>(),
            [
                "RESEND",
                "PATCH",
                "v2",
                "STABLE",
                "V6.6-v6.1",
                "net-next",
                "bpf",
                "net-next",
                "1",
                "2"
            ]
        );
        assert_eq!(
            get_subject_prefixes(subject),
            ["STABLE", "V6.6-v6.1", "bpf", "net-next"]
        );
        assert!(subject_prefix_tokens("STABLE v6.6").next().is_none());
        assert!(subject_prefix_tokens("[STABLE v6.6").next().is_none());
    }

    #[test]
    fn test_get_subject_prefixes() {
        assert_eq!(
            get_subject_prefixes("[PATCH net-next 1/2]"),
            vec!["net-next"]
        );
        assert_eq!(
            get_subject_prefixes("[PATCH v2 bpf-next]"),
            vec!["bpf-next"]
        );
        assert_eq!(get_subject_prefixes("[PATCH RFC]"), Vec::<String>::new());
        assert_eq!(get_subject_prefixes("[PATCH 00/10]"), Vec::<String>::new()); // numbers ignored
        assert_eq!(get_subject_prefixes("[PATCH 6.18]"), vec!["6.18"]);
        assert_eq!(
            get_subject_prefixes("[PATCH net-next v3 0/1]"),
            vec!["net-next"]
        );
        assert_eq!(
            get_subject_prefixes("[PATCH net-next,bpf 1/2]"),
            vec!["bpf", "net-next"]
        ); // sorted
        assert_eq!(get_subject_prefixes("[PATCH]"), Vec::<String>::new());
    }

    #[test]
    fn test_parse_email_author_quoting() {
        let raw = b"From: Thomas Richard (TI) <thomas.richard@bootlin.com>\r\nMessage-ID: <123>\r\nSubject: Test\r\n\r\nDiff:\n--- a\n+++ b\n@@ -1 +1 @@";
        let (meta, _) = parse_email(raw).unwrap();
        assert_eq!(
            meta.author,
            "\"Thomas Richard (TI)\" <thomas.richard@bootlin.com>"
        );
    }

    #[test]
    fn test_parse_email_strips_control_characters_from_names() {
        let raw = b"From: =?utf-8?q?Bob=0D=0AX?= <bob@example.com>\r\nTo: =?utf-8?q?Al=0Ace?= <alice@example.com>\r\nCc: =?utf-8?q?=0D=0A?= <carol@example.com>\r\nMessage-ID: <123>\r\nSubject: =?utf-8?q?Sub=0D=0Aject?=\r\n\r\nbody";
        let (meta, _) = parse_email(raw).unwrap();
        assert_eq!(meta.author, "\"BobX\" <bob@example.com>");
        assert_eq!(meta.to, "\"Alce\" <alice@example.com>");
        assert_eq!(meta.cc, "carol@example.com");
        assert_eq!(meta.subject, "Subject");
    }

    #[test]
    fn test_split_address_list_preserves_quoted_commas() {
        let raw = b"From: Author <author@example.com>\r\nTo: \"Tabba, Fuad\" <fuad@example.com>, \"Trail\\\\\" <trail@example.com>, list@vger.kernel.org\r\nCc: \"Doe, \\\"Jr,.,\\\" John\" <john@example.com>\r\nMessage-ID: <123>\r\nSubject: Test\r\n\r\nbody";
        let (meta, _) = parse_email(raw).unwrap();
        assert_eq!(
            split_address_list(&meta.to),
            vec![
                "\"Tabba, Fuad\" <fuad@example.com>".to_string(),
                "\"Trail\\\\\" <trail@example.com>".to_string(),
                "list@vger.kernel.org".to_string(),
            ]
        );
        assert_eq!(
            split_address_list(&meta.cc),
            vec!["\"Doe, \\\"Jr,.,\\\" John\" <john@example.com>".to_string()]
        );
    }

    #[test]
    fn test_extract_changelog_from_body() {
        let body_with_changelog = "This is the commit message.

Signed-off-by: Me <me@example.com>
---
Changes in v4:
- Rebase on v7.2-rc1, drop dependencies
- (A concurrency fix for mark_new_valid_map was sent independently)
  https://lore.kernel.org/linux-riscv/

diff --git a/file.c b/file.c
index ...";
        assert_eq!(
            extract_changelog_from_body(body_with_changelog),
            Some("Changes in v4:\n- Rebase on v7.2-rc1, drop dependencies\n- (A concurrency fix for mark_new_valid_map was sent independently)\n  https://lore.kernel.org/linux-riscv/".to_string())
        );

        let body_no_changelog = "This is the commit message.

Signed-off-by: Me <me@example.com>
---
diff --git a/file.c b/file.c
index ...";
        assert_eq!(extract_changelog_from_body(body_no_changelog), None);

        let body_no_separator = "This is the commit message.

Signed-off-by: Me <me@example.com>
diff --git a/file.c b/file.c
index ...";
        assert_eq!(extract_changelog_from_body(body_no_separator), None);

        let cover_letter = "This is cover letter.

---
changes:
- test

 fs/file.c | 2 +-";
        assert_eq!(
            extract_changelog_from_body(cover_letter),
            Some("changes:\n- test\n\n fs/file.c | 2 +-".to_string())
        );
    }

    #[test]
    fn test_inject_changelog_into_git_show() {
        let git_show = "commit 1234
Author: Me <me@example.com>
Date:   Mon Jul 6 2026

    This is commit message

diff --git a/file.c b/file.c";
        let changelog = "v2:
- test";
        let expected = "commit 1234
Author: Me <me@example.com>
Date:   Mon Jul 6 2026

    This is commit message

---
v2:
- test

diff --git a/file.c b/file.c";
        assert_eq!(
            inject_changelog_into_git_show(git_show, changelog),
            expected
        );
    }

    #[test]
    fn test_parse_b4_change_id() {
        let body = "Cover letter text\n\n---\nchange-id: 20261012-net-foo-teardown-8a9b1c2d3e4f\nbase-commit: 1234567890abcdef\n";
        assert_eq!(
            parse_b4_change_id(body),
            Some("20261012-net-foo-teardown-8a9b1c2d3e4f".to_string())
        );

        let tracking_json = "Cover prose\n\n--- b4-submit-tracking ---\n# This section is used by b4 prep for tracking purposes.\n{\n  \"series\": {\n    \"revision\": 1,\n    \"change-id\": \"20261012-net-foo-teardown-8a9b1c2d3e4f\",\n    \"prefixes\": []\n  }\n}\n";
        assert_eq!(
            parse_b4_change_id(tracking_json),
            Some("20261012-net-foo-teardown-8a9b1c2d3e4f".to_string())
        );

        let quoted = "> change-id: 20261012-quoted-111111\nprerequisite-change-id: 20261010-prereq-222222\ndiff --git a/foo.c b/foo.c\n+change-id: 20261012-in-diff-333333\n";
        assert_eq!(parse_b4_change_id(quoted), None);
    }

    #[test]
    fn test_parse_b4_submit_tracking() {
        let unedited_msg = "# Lines starting with # will be removed from the cover letter.\n\nSigned-off-by: Dev <dev@example.com>\n\n--- b4-submit-tracking ---\n# This section is used by b4 prep for tracking purposes.\n{\n  \"series\": {\n    \"revision\": 1,\n    \"change-id\": \"20261012-b4-prep-11223344\",\n    \"prefixes\": []\n  }\n}\n";
        let unedited =
            parse_b4_submit_tracking("EDITME: cover title for b4-prep-branch", unedited_msg, "")
                .expect("should detect unedited b4 tracker commit");
        assert_eq!(
            unedited.change_id.as_deref(),
            Some("20261012-b4-prep-11223344")
        );
        assert_eq!(unedited.edited_subject, None);

        let edited_msg = "# Comment line ignored\n># Architecture Overview\n\nExplain why patch 1 prepares the lock before patch 2 drops the old helper.\n\nSigned-off-by: Dev <dev@example.com>\n\n--- b4-submit-tracking ---\n{\"series\": {\"change-id\": \"20261012-b4-prep-11223344\", \"revision\": 2}}\n";
        let edited = parse_b4_submit_tracking("net: foo: refactor lock lifecycle", edited_msg, "")
            .expect("should detect edited b4 tracker commit");
        assert_eq!(
            edited.change_id.as_deref(),
            Some("20261012-b4-prep-11223344")
        );
        assert_eq!(
            edited.edited_subject.as_deref(),
            Some("net: foo: refactor lock lifecycle")
        );

        assert!(
            parse_b4_submit_tracking(
                "net: foo: real commit",
                edited_msg,
                "diff --git a/foo.c b/foo.c\n"
            )
            .is_none()
        );
    }

    #[test]
    fn test_extract_version_reference_msgids() {
        let body = "Fix race in foo_remove when allocating in vmalloc() and setting ptr -> value.\n\
Reported at https://lore.kernel.org/r/20260901-bug-report@kernel.org\n\
See discussion at https://lore.kernel.org/r/20260903-unrelated-thread@kernel.org\n\
Closes: https://lore.kernel.org/r/20260902-closes-bug@kernel.org\n\
Link: https://lore.kernel.org/r/20260904-link-trailer-discussion@kernel.org\n\
\n\
---\n\
Changes in v3:\n\
- Address locking feedback\n\
- Closes: https://lore.kernel.org/r/20260905-bulleted-closes@kernel.org\n\
- Link to v2: https://lore.kernel.org/r/20261010-foo-v2-0-aaaa@kernel.org\n\
- Link to v1: https://lore.kernel.org/netdev/20261001-foo-v1-1-bbbb%40kernel.org/\n\
Link: https://lore.kernel.org/r/20260930-changelog-link@kernel.org\n\
[1] https://lore.kernel.org/all/20260930-footnote@kernel.org/\n\
- Link to v0: <20260929-link-to-v0-msgid@kernel.org>\n\
v1: <20261001-explicit-v1@kernel.org>\n\
* v1: https://lore.kernel.org/bpf/20261001-star-v1@kernel.org/\n\
v2 link: https://lore.kernel.org/all/20261002-v2-link@kernel.org/\n\
v7 (with test steps): https://lore.kernel.org/all/20261003-v7-note@kernel.org/\n\
v1 (https://lore.kernel.org/all/20261004-paren-v1@kernel.org/)\n\
[v3] https://lore.kernel.org/all/20261005-bracket-v3@kernel.org/\n\
- Link[v1]: https://lore.kernel.org/lkml/20261006-link-bracket@kernel.org/\n\
- Link to v4: https://patch.msgid.link/20261007-msgid-link@kernel.org\n\
Previous version: https://lore.kernel.org/r/20261008-previous@kernel.org\n\
Changes since v5 (https://lore.kernel.org/r/20261009-changes-since@kernel.org):\n\
- Patchwork link: https://patchwork.kernel.org/project/netdev/patch/20261001-pw-v1@kernel.org/\n\
v1: https://lore.kernel.org/r/20260925-prereq-url@kernel.org\n\
Based-on: <20260925-prereq-series@kernel.org>\n\
Based-on: https://lore.kernel.org/r/20260925-prereq-url@kernel.org\n\
prerequisite-message-id: <20260920-b4-prereq@kernel.org>\n\
> v1: https://lore.kernel.org/r/20260918-quoted@kernel.org\n\
diff --git a/foo.c b/foo.c\n\
+v1: https://lore.kernel.org/r/should-ignore-diff@kernel.org\n";
        let refs = extract_version_reference_msgids(body);
        assert_eq!(
            refs,
            vec![
                "20261010-foo-v2-0-aaaa@kernel.org",
                "20261001-foo-v1-1-bbbb@kernel.org",
                "20260929-link-to-v0-msgid@kernel.org",
                "20261001-explicit-v1@kernel.org",
                "20261001-star-v1@kernel.org",
                "20261002-v2-link@kernel.org",
                "20261003-v7-note@kernel.org",
                "20261004-paren-v1@kernel.org",
                "20261005-bracket-v3@kernel.org",
                "20261006-link-bracket@kernel.org",
                "20261007-msgid-link@kernel.org",
                "20261008-previous@kernel.org",
                "20261009-changes-since@kernel.org",
            ]
        );
    }

    #[test]
    fn test_extract_version_reference_msgids_ignores_unmarked_links() {
        // A dependency link after a changelog heading names another series,
        // not the previous version of this one.
        let dependency_after_heading = "Changes since v2:\n\
- new patches 3 to 7 fix the range checks\n\
\n\
[1] https://lore.kernel.org/all/20260915070001.113559-3-hao.ge@linux.dev/\n\
Link: https://lore.kernel.org/r/20260915070001.113559-1-hao.ge@linux.dev\n\
This series depends on https://lore.kernel.org/r/20260901-dep@kernel.org\n";
        assert!(extract_version_reference_msgids(dependency_after_heading).is_empty());

        // Kernel-version prose is not a previous-version marker.
        let kernel_versions = "This regressed since v6.8 https://lore.kernel.org/r/20260101-regress@kernel.org\n\
It was fixed in v5.4: https://lore.kernel.org/r/20260102-fixed-in@kernel.org\n\
in v6.12 (https://lore.kernel.org/r/20260103-in-v6-12@kernel.org)\n\
v6.8: https://lore.kernel.org/r/20260104-v6-8-colon@kernel.org\n\
v5.4 (https://lore.kernel.org/r/20260105-v5-4-paren@kernel.org)\n\
Changes since v6.8: https://lore.kernel.org/r/20260106-changes-since-kver@kernel.org\n";
        assert!(extract_version_reference_msgids(kernel_versions).is_empty());

        // A version marker followed by prose names something else.
        let prose = "v2 of the base series: https://lore.kernel.org/r/20260107-base-v2@kernel.org\n\
v1 was discussed at https://lore.kernel.org/r/20260108-discussed@kernel.org\n";
        assert!(extract_version_reference_msgids(prose).is_empty());
    }

    #[test]
    fn test_extract_prerequisite_message_and_patch_ids() {
        let body = "Cover letter\n---\nBased-on: <20260925-prereq-series@kernel.org>\nBased-on: https://lore.kernel.org/r/20260925-prereq-url%40kernel.org/\nprerequisite-message-id: 20260920-b4-prereq@kernel.org\nprerequisite-patch-id: 0123456789abcdef0123456789abcdef01234567\n";
        let (msgids, patch_ids) = extract_prerequisite_message_and_patch_ids(body);
        assert_eq!(
            msgids,
            vec![
                "20260925-prereq-series@kernel.org",
                "20260925-prereq-url@kernel.org",
                "20260920-b4-prereq@kernel.org"
            ]
        );
        assert_eq!(patch_ids, vec!["0123456789abcdef0123456789abcdef01234567"]);
    }

    #[test]
    fn test_parse_email_decodes_quoted_printable_diff() {
        let raw = b"Message-ID: <qp-patch@example.com>\r\n\
From: Robin Dapp <rdapp.gcc@gmail.com>\r\n\
Subject: [PATCH] lra: Handle subregs in get_dependent_filter.\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=UTF-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Fix subreg handling.\r\n\
\r\n\
diff --git a/gcc/lra-constraints.cc b/gcc/lra-constraints.cc\r\n\
index 7b74ee2bef7..8685c4f97e4 100644\r\n\
--- a/gcc/lra-constraints.cc\r\n\
+++ b/gcc/lra-constraints.cc\r\n\
@@ -1,3 +1,3 @@\r\n\
-  int x =3D 0;\r\n\
+  int x =3D 1;\r\n";

        let (meta, patch) = parse_email(raw).unwrap();
        assert!(meta.is_patch_or_cover);
        let patch = patch.expect("expected decoded patch");
        assert!(patch.diff.contains("-  int x = 0;\n+  int x = 1;"));
        assert!(!patch.diff.contains("=3D"));
    }

    #[test]
    fn test_parse_email_extracts_attached_format_patch() {
        let raw = b"Message-ID: <attached-patch@example.com>\r\n\
From: Dev <dev@gcc.gnu.org>\r\n\
Subject: [PATCH] tree-vect: fix loop bound\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
\r\n\
--BOUNDARY\r\n\
Content-Type: text/plain; charset=UTF-8\r\n\
\r\n\
Bootstrapped and regtested on x86_64-pc-linux-gnu.\r\n\
--BOUNDARY\r\n\
Content-Type: text/x-patch; name=\"0001-fix.patch\"\r\n\
Content-Disposition: attachment; filename=\"0001-fix.patch\"\r\n\
\r\n\
From 0123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001\r\n\
From: Dev <dev@gcc.gnu.org>\r\n\
Date: Thu, 22 Oct 2026 10:00:00 +0000\r\n\
Subject: [PATCH] tree-vect: fix loop bound\r\n\
\r\n\
gcc/ChangeLog:\r\n\
\t* tree-vect-loop.cc: Fix bound.\r\n\
---\r\n\
diff --git a/gcc/tree-vect-loop.cc b/gcc/tree-vect-loop.cc\r\n\
index 1111111..2222222 100644\r\n\
--- a/gcc/tree-vect-loop.cc\r\n\
+++ b/gcc/tree-vect-loop.cc\r\n\
@@ -10,3 +10,3 @@\r\n\
-  return 0;\r\n\
+  return 1;\r\n\
--BOUNDARY--\r\n";

        let (meta, patch) = parse_email(raw).unwrap();
        assert!(meta.is_patch_or_cover);
        let patch = patch.expect("expected attached patch to be extracted");
        assert!(patch.diff.contains("diff --git a/gcc/tree-vect-loop.cc"));
        assert!(
            !patch.diff.contains("From 0123456789abcdef"),
            "format-patch envelope header should be stripped"
        );
    }

    #[test]
    fn test_quoted_diff_does_not_produce_patch() {
        let raw = b"Message-ID: <quoted-diff@example.com>\r\n\
From: Reviewer <reviewer@gcc.gnu.org>\r\n\
Subject: Re: [PATCH] fix bug\r\n\
\r\n\
On Thu, Oct 22, Author wrote:\r\n\
> diff --git a/file.c b/file.c\r\n\
> --- a/file.c\r\n\
> +++ b/file.c\r\n\
> @@ -1 +1 @@\r\n\
> -old\r\n\
> +new\r\n";

        let (meta, patch) = parse_email(raw).unwrap();
        assert!(!meta.is_patch_or_cover);
        assert!(patch.is_none());
    }

    #[test]
    fn test_diffless_ping_is_not_patchset() {
        for subject in [
            "Ping: [PATCH] c++: fix template",
            "[PING] [PATCH] c++: fix template",
            "[PING^2][PATCH] c++: fix template",
        ] {
            let raw =
                format!("Message-ID: <ping@example.com>\r\nSubject: {subject}\r\n\r\nPing!\r\n");
            let (meta, patch) = parse_email(raw.as_bytes()).unwrap();
            assert!(
                !meta.is_patch_or_cover,
                "diffless ping '{subject}' should not be treated as a patchset"
            );
            assert!(patch.is_none());
        }
    }

    #[test]
    fn test_commit_tag_subject_index() {
        assert_eq!(
            parse_subject_index("[gccrs COMMIT 10/12] gccrs: add check"),
            (10, 12)
        );
    }

    #[test]
    fn test_promote_reply_patch_subject() {
        assert!(is_promotable_reply_patch_subject(
            "Re: [PATCH] lra: Handle subregs in get_dependent_filter."
        ));
        assert!(!is_promotable_reply_patch_subject(
            "Forwarded: Re: [PATCH] lra: Handle subregs."
        ));
        assert!(!is_promotable_reply_patch_subject(
            "Re: [syzbot] bug report"
        ));
        assert!(!is_promotable_reply_patch_subject(
            "Re: [syzbot] bug in patch handler"
        ));
        assert_eq!(
            promote_reply_patch_subject(
                "Re: [PATCH] lra: Handle subregs in get_dependent_filter.",
                2
            ),
            "[PATCH v2] lra: Handle subregs in get_dependent_filter."
        );
        assert_eq!(
            promote_reply_patch_subject(
                "Re: [PATCH v2] lra: Handle subregs in get_dependent_filter.",
                3
            ),
            "[PATCH v3] lra: Handle subregs in get_dependent_filter."
        );
    }
}
