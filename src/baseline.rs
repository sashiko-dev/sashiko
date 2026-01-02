use anyhow::Result;
use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, PartialEq)]
pub struct Baseline {
    pub repo_url: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
}

pub fn detect_baseline(subject: &str, body: &str) -> Result<Baseline> {
    // 1. Check for "base-commit: <hash>"
    static BASE_COMMIT_RE: OnceLock<Regex> = OnceLock::new();
    let base_commit_re = BASE_COMMIT_RE.get_or_init(|| {
        Regex::new(r"(?m)^base-commit: ([0-9a-f]{40})").expect("Invalid regex")
    });

    let commit = base_commit_re
        .captures(body)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string());

    // 2. Heuristic from subject (e.g. [PATCH net-next])
    // This is simplified; mapping "net-next" to a URL requires a config/map.
    let branch = if subject.contains("net-next") {
        Some("net-next".to_string())
    } else if subject.contains("bpf-next") {
        Some("bpf-next".to_string())
    } else {
        None
    };

    Ok(Baseline {
        repo_url: None, // Logic to map branch/subsystem to URL would go here
        branch,
        commit,
    })
}
