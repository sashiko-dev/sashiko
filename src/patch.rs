use anyhow::{anyhow, Result};
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
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Patch {
    pub message_id: String,
    pub body: String,
    pub diff: String,
    pub part_index: u32,
}

#[allow(dead_code)]
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
        .map(|a| a.address().unwrap_or("unknown").to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let date = message.date().map(|d| d.to_timestamp()).unwrap_or(0);

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

    let body = message.body_text(0).unwrap_or_default().to_string();

    let diff = if body.contains("diff --git") {
        body.clone()
    } else {
        String::new()
    };

    let metadata = PatchsetMetadata {
        message_id: message_id.clone(),
        subject,
        author,
        date,
        in_reply_to,
        references,
        index,
        total,
    };

    let patch = if !diff.is_empty() {
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
    let re = RE.get_or_init(|| Regex::new(r"\[PATCH.*?(\d+)/(\d+)\]").unwrap());

    if let Some(caps) = re.captures(subject) {
        let index = caps.get(1).map_or(1, |m| m.as_str().parse().unwrap_or(1));
        let total = caps.get(2).map_or(1, |m| m.as_str().parse().unwrap_or(1));
        (index, total)
    } else {
        (1, 1)
    }
}
