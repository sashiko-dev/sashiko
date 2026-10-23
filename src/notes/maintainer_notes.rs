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

//! Maintainer notes: what a subsystem's maintainers want that the code
//! cannot tell (preferences, policies, conventions, review priorities).
//! Code notes are generated from the source; maintainer notes are written
//! by people, one file per subsystem under `maintainers/` in a project's
//! prompt directory:
//!
//! ```markdown
//! +++
//! title = "Networking Drivers"
//! paths = ["drivers/net/"]
//! symbols = ["ethtool_ops", "netdev_*"]
//! +++
//! # Networking Drivers: maintainer notes
//!
//! - One preference per bullet.
//! ```
//!
//! A review loads every note whose `paths` match a file the patch touches
//! or whose `symbols` match an identifier its diff touches. A path is a
//! directory prefix ending in `/`, an exact file, a glob, or `*` for every
//! patch; a symbol is an identifier, or a prefix ending in `*`.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::maintainers::CompiledPattern;

/// Directory of maintainer notes inside a project's prompt directory.
pub const MAINTAINER_NOTES_RELDIR: &str = "maintainers";

const DELIMITER: &str = "+++";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    title: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    symbols: Vec<String>,
}

/// One maintainer note.
#[derive(Debug, Clone)]
pub struct MaintainerNote {
    /// File name without `.md`.
    pub name: String,
    pub title: String,
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    /// The markdown after the front matter.
    pub body: String,
}

impl MaintainerNote {
    /// Parses a note from its file contents.
    pub fn parse(name: &str, text: &str) -> Result<Self> {
        let rest = text
            .strip_prefix(DELIMITER)
            .context("a maintainer note must start with a +++ front matter line")?;
        let (front, body) = rest
            .split_once(&format!("\n{DELIMITER}"))
            .context("the front matter has no closing +++ line")?;
        let front: FrontMatter = toml::from_str(front).context("invalid front matter")?;
        if front.title.trim().is_empty() {
            bail!("the front matter needs a title");
        }
        if front.paths.is_empty() && front.symbols.is_empty() {
            bail!("the front matter needs paths or symbols, or the note is never loaded");
        }
        let body = body.trim_start_matches(['\r', '\n']).trim_end().to_string();
        if body.is_empty() {
            bail!("the note has no body");
        }
        Ok(Self {
            name: name.to_string(),
            title: front.title,
            paths: front.paths,
            symbols: front.symbols,
            body,
        })
    }

    /// Whether the note applies to a patch touching `files` with a diff
    /// touching `identifiers`.
    pub fn applies(&self, files: &[String], identifiers: &HashSet<String>) -> bool {
        let path_match = self.paths.iter().any(|pattern| {
            pattern.trim() == "*" || {
                let compiled = CompiledPattern::compile(pattern);
                files.iter().any(|f| compiled.is_match(f))
            }
        });
        path_match
            || self
                .symbols
                .iter()
                .any(|symbol| match symbol.strip_suffix('*') {
                    Some(prefix) => identifiers.iter().any(|id| id.starts_with(prefix)),
                    None => identifiers.contains(symbol),
                })
    }
}

/// Every maintainer note in `prompts_dir`, sorted by name. Notes that fail
/// to parse are skipped with a warning, so one bad note does not cost a
/// review the others.
pub fn load_maintainer_notes(prompts_dir: &Path) -> Vec<MaintainerNote> {
    let dir = prompts_dir.join(MAINTAINER_NOTES_RELDIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".md"))
        else {
            continue;
        };
        let parsed = std::fs::read_to_string(&path)
            .context("unreadable")
            .and_then(|text| MaintainerNote::parse(name, &text));
        match parsed {
            Ok(note) => notes.push(note),
            Err(err) => tracing::warn!("skipping maintainer note {}: {err:#}", path.display()),
        }
    }
    notes.sort_by(|a, b| a.name.cmp(&b.name));
    notes
}

/// Every identifier on a diff's added and removed lines and in its hunk
/// headers. Unlike code-note focusing, short and common names count:
/// maintainer notes name subsystem prefixes such as `skb_` or `bpf`.
pub fn diff_identifiers_all(diff: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut word = String::new();
    for line in diff.lines() {
        let text = if line.starts_with("+++") || line.starts_with("---") {
            continue;
        } else if let Some(rest) = line.strip_prefix("@@") {
            rest.split_once("@@").map_or("", |(_, context)| context)
        } else if let Some(rest) = line.strip_prefix(['+', '-']) {
            rest
        } else {
            continue;
        };
        for c in text.chars().chain(std::iter::once(' ')) {
            if c.is_ascii_alphanumeric() || c == '_' {
                word.push(c);
            } else if !word.is_empty() {
                if !word.starts_with(|c: char| c.is_ascii_digit()) {
                    out.insert(std::mem::take(&mut word));
                } else {
                    word.clear();
                }
            }
        }
    }
    out
}

/// Renders the review prompt block for `notes`.
pub fn render_maintainer_notes(notes: &[&MaintainerNote]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut block = String::from(
        "\n\n<maintainer_notes>\nThese notes record what the maintainers of the subsystems this patch touches expect beyond correctness: their preferences, policies, conventions and review priorities. Apply them when judging the patch; they do not describe how the code works.\n",
    );
    for note in notes {
        block.push_str(&format!(
            "\n<maintainer_note subsystem=\"{}\">\n{}\n</maintainer_note>\n",
            note.title.replace('"', "'"),
            note.body
        ));
    }
    block.push_str("</maintainer_notes>");
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "+++\ntitle = \"Networking Drivers\"\npaths = [\"drivers/net/\"]\nsymbols = [\"ethtool_ops\", \"netdev_*\"]\n+++\n# Networking Drivers: maintainer notes\n\n- Prefer standard stats.\n";

    #[test]
    fn test_parse_and_apply() {
        let note = MaintainerNote::parse("networking-drivers", NOTE).unwrap();
        assert_eq!(note.title, "Networking Drivers");
        assert!(note.body.starts_with("# Networking Drivers"));

        let none = HashSet::new();
        assert!(note.applies(&["drivers/net/e1000/main.c".to_string()], &none));
        assert!(!note.applies(&["fs/ext4/inode.c".to_string()], &none));
        let ids = HashSet::from(["netdev_priv".to_string()]);
        assert!(note.applies(&[], &ids));
        assert!(note.applies(&[], &HashSet::from(["ethtool_ops".to_string()])));
        assert!(!note.applies(&[], &HashSet::from(["ethtool_ops_x".to_string()])));

        let general = MaintainerNote::parse(
            "general",
            "+++\ntitle = \"General\"\npaths = [\"*\"]\n+++\n- Be kind.\n",
        )
        .unwrap();
        assert!(general.applies(&["any/file.c".to_string()], &none));
    }

    #[test]
    fn test_parse_rejects_bad_notes() {
        for (text, expected) in [
            ("# no front matter", "front matter line"),
            ("+++\ntitle = \"x\"\npaths = [\"a/\"]\n", "closing"),
            ("+++\ntitle = \"x\"\n+++\n- b\n", "paths or symbols"),
            (
                "+++\ntitle = \"x\"\npaths = [\"a/\"]\nextra = 1\n+++\n- b\n",
                "invalid",
            ),
            ("+++\ntitle = \"x\"\npaths = [\"a/\"]\n+++\n\n", "no body"),
        ] {
            let err = format!("{:#}", MaintainerNote::parse("n", text).unwrap_err());
            assert!(err.contains(expected), "expected {expected:?} in {err:?}");
        }
    }

    #[test]
    fn test_diff_identifiers_all_keeps_short_names() {
        let ids = diff_identifiers_all(
            "--- a/x.c\n+++ b/x.c\n@@ -1 +1 @@ int bpf_prog_run(void)\n-\tskb_put(skb, 4);\n+\tret = 0x10;\n context_line(ignored);\n",
        );
        for id in ["bpf_prog_run", "skb_put", "skb", "ret", "int"] {
            assert!(ids.contains(id), "missing {id}");
        }
        assert!(!ids.contains("context_line") && !ids.contains("0x10"));
    }

    #[test]
    fn test_render_block() {
        let note = MaintainerNote::parse("n", NOTE).unwrap();
        let block = render_maintainer_notes(&[&note]);
        assert!(block.contains("<maintainer_note subsystem=\"Networking Drivers\">"));
        assert!(block.contains("- Prefer standard stats."));
        assert_eq!(render_maintainer_notes(&[]), "");
    }

    #[test]
    fn test_shipped_maintainer_notes_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("projects");
        for project in std::fs::read_dir(&root).unwrap().flatten() {
            let dir = project.path().join("prompts").join(MAINTAINER_NOTES_RELDIR);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_stem().unwrap().to_string_lossy().to_string();
                let text = std::fs::read_to_string(&path).unwrap();
                MaintainerNote::parse(&name, &text)
                    .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
            }
        }
    }
}
