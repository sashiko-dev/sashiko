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

pub fn parse_email(raw_email: &[u8]) -> Result<(PatchsetMetadata, Option<Patch>)> {
    let message = MessageParser::default()
        .parse(raw_email)
        .ok_or_else(|| anyhow!("Failed to parse email"))?;

    let message_id = message
        .message_id()
        .ok_or_else(|| anyhow!("No Message-ID header"))?
        .to_string();

    let subject = message.subject().unwrap_or("(no subject)").to_string();

    let author = message
        .from()
        .and_then(|addr| addr.first())
        .map(|a| {
            let name = a.name().unwrap_or_default().trim();
            let address = a.address().unwrap_or("unknown@localhost").trim();
            let n = if name.is_empty() { "Unknown" } else { name };
            let addr = if address.is_empty() {
                "unknown@localhost"
            } else {
                address
            };
            format!("{} <{}>", n, addr)
        })
        .unwrap_or_else(|| "Unknown <unknown@localhost>".to_string());

    let date = message.date().map(|d| d.to_timestamp()).unwrap_or(0);

    let to = message
        .to()
        .map(|addr| {
            addr.iter()
                .map(|a| a.address().unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let cc = message
        .cc()
        .map(|addr| {
            addr.iter()
                .map(|a| a.address().unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let in_reply_to = match message.in_reply_to() {
        HeaderValue::Text(t) => Some(t.to_string()),
        HeaderValue::TextList(l) => l.first().map(|s| s.to_string()),
        _ => None,
    };

    let references = match message.references() {
        HeaderValue::Text(t) => vec![t.to_string()],
        HeaderValue::TextList(l) => l.iter().map(|s| s.to_string()).collect(),
        _ => vec![],
    };

    let (index, total) = parse_subject_index(&subject);
    let version = parse_subject_version(&subject);

    let mut body = String::new();
    for i in 0..message.text_body_count() {
        if let Some(text) = message.body_text(i) {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&text);
        }
    }

    let diff = if body.contains("diff --git")
        || (body.contains("--- ") && body.contains("+++ ") && body.contains("@@ -"))
    {
        body.clone()
    } else {
        String::new()
    };

    // Detection logic
    let subject_lower = subject.to_lowercase();
    let subject_clean = subject_lower.trim();
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
        || subject_lower.contains("(was:");
    let has_patch_tag = subject_clean.contains("patch") || subject_clean.contains("rfc");
    let has_diff = !diff.is_empty();

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

fn parse_subject_index(subject: &str) -> (u32, u32) {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    // Match [ ... M/N ... ] but strictly require PATCH, RFC, RESEND or vN before the M/N
    let re_brackets = RE_BRACKETS.get_or_init(|| {
        Regex::new(r"(?i)\[.*?\b(?:PATCH|RFC|RESEND|v\d+)\b.*?(\d+)/(\d+).*?\]").unwrap()
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
    let re_loose =
        RE_LOOSE.get_or_init(|| Regex::new(r"(?i)\b(?:PATCH|RFC|RESEND)\s+(\d+)/(\d+)\b").unwrap());

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

pub fn get_subject_prefixes(subject: &str) -> Vec<String> {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    let re = RE_BRACKETS.get_or_init(|| Regex::new(r"\[(.*?)\]").unwrap());

    let mut prefixes = Vec::new();

    for cap in re.captures_iter(subject) {
        if let Some(content) = cap.get(1) {
            // Split by whitespace or non-word characters.
            // "PATCH net-next 1/2" -> "PATCH", "net-next", "1/2"
            // "PATCH,net-next,1/2" -> "PATCH", "net-next", "1/2"
            let tokens: Vec<&str> = content
                .as_str()
                .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '.' && c != '_')
                .filter(|s| !s.is_empty())
                .collect();

            for token in tokens {
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
        }
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
    let prefixes = ["re:", "fwd:", "aw:", "forwarded:", "回复:", "回复："];

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

pub fn extract_email(author: &str) -> String {
    if let Some(start) = author.find('<')
        && let Some(end) = author.find('>')
        && end > start
    {
        return author[start + 1..end].trim().to_string();
    }
    author.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(meta.author, "Test User <test@example.com>");

        let raw_no_name =
            b"Message-ID: <456>\r\nFrom: test2@example.com\r\nSubject: Test\r\n\r\nBody";
        let (meta2, _) = parse_email(raw_no_name).unwrap();
        assert_eq!(meta2.author, "Unknown <test2@example.com>");
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
}
