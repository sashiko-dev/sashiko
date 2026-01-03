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
            let name = a.name().unwrap_or_default();
            let address = a.address().unwrap_or("unknown");
            if name.is_empty() {
                address.to_string()
            } else {
                format!("{} <{}>", name, address)
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

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

    let body = message.body_text(0).unwrap_or_default().to_string();

    let diff = if body.contains("diff --git") || (body.contains("--- ") && body.contains("+++ ") && body.contains("@@ -")) {
        body.clone()
    } else {
        String::new()
    };

    // Detection logic
    let subject_lower = subject.to_lowercase();
    let is_reply = subject_lower.trim().starts_with("re:");
    let has_patch_tag = subject_lower.contains("patch") || subject_lower.contains("rfc");
    let has_diff = !diff.is_empty();

    // A message is part of a series if it's a cover letter (index 0) or has multiple parts (total > 1)
    let is_series_metadata = total > 1 || index == 0;

    // It is a patch or cover letter if:
    // 1. It is NOT a reply (Re: ...)
    // 2. AND (It contains a diff OR (It has [PATCH]/[RFC] tag AND looks like a series))
    // This ensures single patches [PATCH] must have a diff, and cover letters are always accepted.
    let is_patch_or_cover = !is_reply && (has_diff || (has_patch_tag && is_series_metadata));

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
    };

    let patch = if has_diff {
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
    static RE: OnceLock<Regex> = OnceLock::new();
    // Allow [Anything 1/2 Anything]
    let re = RE.get_or_init(|| Regex::new(r"\[.*?(\d+)/(\d+).*?\]").unwrap());

    if let Some(caps) = re.captures(subject) {
        let index = caps.get(1).map_or(1, |m| m.as_str().parse().unwrap_or(1));
        let total = caps.get(2).map_or(1, |m| m.as_str().parse().unwrap_or(1));
        (index, total)
    } else {
        (1, 1)
    }
}

pub fn parse_subject_version(subject: &str) -> Option<u32> {
    static RE_VER: OnceLock<Regex> = OnceLock::new();
    // Match version patterns like:
    // - [PATCH v2] ... (Standard)
    // - [PATCH V2] ... (Uppercase)
    // - [PATCHv2] ... (Attached)
    // - [RFC v2] ... (RFC)
    // - v3 PATCH ... (Start)
    // - ... v4] (End)
    // Avoid false positives like "dev" or "device".
    // Strategy:
    // Prefix: Start of string (^), non-alphanumeric char ([^a-z0-9]), or literal "PATCH" (for PATCHvN)
    // Body: v or V followed by digits
    // Suffix: End of string ($) or non-alphanumeric char
    let re = RE_VER.get_or_init(|| Regex::new(r"(?i)(?:^|[^a-z0-9]|PATCH)v(\d+)(?:[^a-z0-9]|$)").unwrap());

    if let Some(caps) = re.captures(subject) {
        caps.get(1).and_then(|m| m.as_str().parse().ok())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_author_parsing() {
        let raw =
            b"Message-ID: <123>\r\nFrom: Test User <test@example.com>\r\nSubject: Test\r\n\r\nBody";
        let (meta, _) = parse_email(raw).unwrap();
        assert_eq!(meta.author, "Test User <test@example.com>");

        let raw_no_name =
            b"Message-ID: <456>\r\nFrom: test2@example.com\r\nSubject: Test\r\n\r\nBody";
        let (meta2, _) = parse_email(raw_no_name).unwrap();
        assert_eq!(meta2.author, "test2@example.com");
    }

    #[test]
    fn test_reply_with_diff_is_not_patchset() {
        // A message that starts with Re: but contains diff --git
        // This simulates a reply quoting a patch or sending an inline fixup
        let raw = b"Message-ID: <123>\r\nSubject: Re: [PATCH] fix bug\r\n\r\n> diff --git a/file b/file\n> index...";
        let (meta, _) = parse_email(raw).unwrap();
        
        // This fails with current logic because has_diff is true
        assert!(!meta.is_patch_or_cover, "Reply with diff should NOT be a patchset");
    }

        #[test]
        fn test_normal_patch() {
            let raw = b"Message-ID: <456>\r\nSubject: [PATCH] fix bug\r\n\r\ndiff --git a/file b/file\nindex...";
            let (meta, _) = parse_email(raw).unwrap();
            assert!(meta.is_patch_or_cover);
        }
    
        #[test]
        fn test_single_patch_no_diff_ignored() {
            let raw = b"Message-ID: <nonpatch>\r\nSubject: [PATCH] discussion\r\n\r\nThis is not a patch";
            let (meta, _) = parse_email(raw).unwrap();
            assert!(!meta.is_patch_or_cover, "Single patch without diff should be ignored");
        }
    
        #[test]
        fn test_cover_letter() {        let raw = b"Message-ID: <789>\r\nSubject: [PATCH 0/5] fix bug\r\n\r\nCover letter body";
        let (meta, _) = parse_email(raw).unwrap();
        assert!(meta.is_patch_or_cover);
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
        assert_eq!(parse_subject_version("Subject with v2 inside"), Some(2));
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
}
