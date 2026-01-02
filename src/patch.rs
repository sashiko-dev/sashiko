use anyhow::{anyhow, Result};
use mail_parser::MessageParser;

#[derive(Debug)]
pub struct Patchset {
    pub message_id: String,
    pub subject: String,
    pub author: String,
    pub date: i64,
}

#[derive(Debug)]
pub struct Patch {
    pub message_id: String,
    pub body: String,
    pub diff: String,
}

pub fn parse_email(raw_email: &[u8]) -> Result<(Patchset, Option<Patch>)> {
    let message = MessageParser::default()
        .parse(raw_email)
        .ok_or_else(|| anyhow!("Failed to parse email"))?;

    let message_id = message
        .message_id()
        .ok_or_else(|| anyhow!("No Message-ID header"))?
        .to_string();

    let subject = message
        .subject()
        .unwrap_or("(no subject)")
        .to_string();

    let author = message
        .from()
        .and_then(|addr| addr.first())
        .map(|a| a.address().unwrap_or("unknown").to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let date = message
        .date()
        .map(|d| d.to_timestamp())
        .unwrap_or(0);

    let body = message
        .body_text(0)
        .unwrap_or_default()
        .to_string();

    // Simple heuristic: if body contains "diff --git", it's likely a patch
    let diff = if body.contains("diff --git") {
        // In a real implementation, we might want to extract just the diff part
        body.clone()
    } else {
        String::new()
    };

    let patchset = Patchset {
        message_id: message_id.clone(),
        subject,
        author,
        date,
    };

    let patch = if !diff.is_empty() {
        Some(Patch {
            message_id,
            body,
            diff,
        })
    } else {
        None
    };

    Ok((patchset, patch))
}