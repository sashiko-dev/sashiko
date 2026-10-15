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

//! The research agenda: version-agnostic questions the explorer answers
//! about each component. Agenda modules are markdown files under the
//! project's prompt directory (`notes/agenda/<module>.md`); each `##`
//! section is one topic, explored in its own stage, and each bullet is one
//! question with a stable ID:
//!
//! ```markdown
//! ## SYNC: Concurrency and Synchronization
//!
//! - SYNC-1: Which state is shared between concurrent contexts?
//! ```

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Directory of agenda modules inside a project's prompt directory.
pub const AGENDA_RELDIR: &str = "notes/agenda";

/// One question of a topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Question {
    pub id: String,
    pub text: String,
}

/// One topic, explored in its own stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Topic {
    pub id: String,
    pub title: String,
    pub module: String,
    pub questions: Vec<Question>,
}

/// The agenda a notes build works through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Agenda {
    /// Content hash of the modules (first 16 hex digits), recorded with
    /// every note so a change of agenda is visible in the manifest.
    pub version: String,
    pub topics: Vec<Topic>,
}

impl Agenda {
    /// Looks up a question by ID.
    pub fn question(&self, id: &str) -> Option<&Question> {
        self.topics
            .iter()
            .flat_map(|t| t.questions.iter())
            .find(|q| q.id == id)
    }
}

/// Loads and validates the agenda `modules` from `prompts_dir`.
pub fn load_agenda(prompts_dir: &Path, modules: &[String]) -> Result<Agenda> {
    if modules.is_empty() {
        bail!("agenda.modules must list at least one agenda module");
    }
    let mut hasher = Sha256::new();
    let mut topics = Vec::new();
    for module in modules {
        if !is_safe_module_name(module) {
            bail!("agenda module {module:?} must be a relative path without \"..\"");
        }
        let path = prompts_dir.join(AGENDA_RELDIR).join(format!("{module}.md"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read agenda module {}", path.display()))?;
        hasher.update(module.as_bytes());
        hasher.update(b"\0");
        hasher.update(text.as_bytes());
        hasher.update(b"\0");
        topics.extend(
            parse_agenda_module(module, &text)
                .with_context(|| format!("in agenda module {}", path.display()))?,
        );
    }
    check_unique_ids(&topics)?;
    let digest = hasher.finalize();
    let version = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Ok(Agenda { version, topics })
}

fn is_safe_module_name(module: &str) -> bool {
    !module.is_empty()
        && !module.starts_with('/')
        && !module.contains('\\')
        && module
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Parses one agenda module into topics.
fn parse_agenda_module(module: &str, text: &str) -> Result<Vec<Topic>> {
    let mut topics: Vec<Topic> = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if let Some(heading) = line.strip_prefix("## ") {
            let Some((id, title)) = heading.split_once(':') else {
                bail!(
                    "line {}: topic heading must look like \"## ID: Title\"",
                    lineno + 1
                );
            };
            let id = id.trim();
            if !is_valid_id(id) {
                bail!("line {}: invalid topic ID {id:?}", lineno + 1);
            }
            topics.push(Topic {
                id: id.to_string(),
                title: title.trim().to_string(),
                module: module.to_string(),
                questions: Vec::new(),
            });
        } else if let Some(item) = line.strip_prefix("- ") {
            let Some(topic) = topics.last_mut() else {
                // Bullets before the first topic are prose, not questions.
                continue;
            };
            let Some((id, question)) = item.split_once(':') else {
                bail!(
                    "line {}: question must look like \"- ID: question\"",
                    lineno + 1
                );
            };
            let id = id.trim();
            if !is_valid_id(id) || !id.starts_with(&format!("{}-", topic.id)) {
                bail!(
                    "line {}: question ID {id:?} must start with its topic ID {:?}",
                    lineno + 1,
                    topic.id
                );
            }
            topic.questions.push(Question {
                id: id.to_string(),
                text: question.trim().to_string(),
            });
        }
    }
    if topics.is_empty() {
        bail!("no topics (\"## ID: Title\" headings) found");
    }
    if let Some(empty) = topics.iter().find(|t| t.questions.is_empty()) {
        bail!("topic {} has no questions", empty.id);
    }
    Ok(topics)
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn check_unique_ids(topics: &[Topic]) -> Result<()> {
    let mut seen = HashSet::new();
    for topic in topics {
        if !seen.insert(topic.id.as_str()) {
            bail!("topic ID {} is defined twice", topic.id);
        }
        for question in &topic.questions {
            if !seen.insert(question.id.as_str()) {
                bail!("question ID {} is defined twice", question.id);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE: &str = "\
# Universal

Intro prose.
- not a question

## SYNC: Concurrency

- SYNC-1: Which state is shared?
- SYNC-2: What lock order exists?

## LIFE: Lifetimes

- LIFE-1: Who owns each resource?
";

    #[test]
    fn test_parse_module() {
        let topics = parse_agenda_module("universal", MODULE).unwrap();
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].id, "SYNC");
        assert_eq!(topics[0].title, "Concurrency");
        assert_eq!(topics[0].questions[1].id, "SYNC-2");
        assert_eq!(topics[1].questions[0].text, "Who owns each resource?");
    }

    #[test]
    fn test_parse_rejects_malformed_modules() {
        for (text, expected) in [
            ("## SYNC Concurrency\n- SYNC-1: q\n", "topic heading"),
            ("## sync: c\n- sync-1: q\n", "invalid topic ID"),
            ("## SYNC: c\n- LIFE-1: q\n", "must start with its topic ID"),
            ("## SYNC: c\n- SYNC-1 q\n", "question must look like"),
            ("## SYNC: c\n", "has no questions"),
            ("no topics\n", "no topics"),
        ] {
            let err = format!("{:#}", parse_agenda_module("m", text).unwrap_err());
            assert!(err.contains(expected), "expected {expected:?} in {err:?}");
        }
    }

    #[test]
    fn test_load_agenda_versions_and_checks_ids() {
        let dir = tempfile::tempdir().unwrap();
        let agenda_dir = dir.path().join(AGENDA_RELDIR);
        std::fs::create_dir_all(agenda_dir.join("lang")).unwrap();
        std::fs::write(agenda_dir.join("universal.md"), MODULE).unwrap();
        std::fs::write(
            agenda_dir.join("lang/rust.md"),
            "## RUST: Rust\n- RUST-1: Panics?\n",
        )
        .unwrap();
        std::fs::write(agenda_dir.join("dup.md"), "## SYNC: Again\n- SYNC-9: q\n").unwrap();

        let modules = vec!["universal".to_string(), "lang/rust".to_string()];
        let agenda = load_agenda(dir.path(), &modules).unwrap();
        assert_eq!(agenda.topics.len(), 3);
        assert_eq!(agenda.version.len(), 16);
        assert_eq!(agenda.question("RUST-1").unwrap().text, "Panics?");
        assert_eq!(agenda.topics[2].module, "lang/rust");

        let with_dup = vec!["universal".to_string(), "dup".to_string()];
        assert!(format!("{:#}", load_agenda(dir.path(), &with_dup).unwrap_err()).contains("twice"));
        assert!(load_agenda(dir.path(), &["../x".to_string()]).is_err());
        assert!(load_agenda(dir.path(), &[]).is_err());
    }
}
