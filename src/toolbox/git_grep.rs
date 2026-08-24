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

use crate::ai::truncator::Truncator;
use crate::toolbox::SashikoToolContext;
use crate::toolbox::framework::LlmTool;
use crate::toolbox::utils::format_git_grep_output;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

pub struct GitGrepTool;

#[async_trait]
impl LlmTool<SashikoToolContext> for GitGrepTool {
    fn name(&self) -> &'static str {
        "git_grep"
    }

    fn description(&self) -> &'static str {
        "Search for a pattern in files using git grep at a specific Git revision. Returns matching lines with context."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "revision": { "type": "string", "description": "Git commit SHA or reference to search at." },
                "pattern": { "type": "string", "description": "Regex or literal pattern to search." },
                "path": { "type": "string", "description": "Relative paths or pathspecs to restrict search (optional). Highly recommended to scope to the modified subsystem directory (e.g. 'net/mptcp/') to avoid extremely expensive tree-wide searches." },
                "context_lines": { "type": "integer", "description": "Context lines to show. Default: 0." },
                "count_only": { "type": "boolean", "description": "If true, returns file names and match counts only. Recommended for cheap broad searches." },
                "is_literal": { "type": "boolean", "description": "If true, treats pattern as literal fixed string rather than PCRE regex." }
            },
            "required": ["revision", "pattern"]
        })
    }

    fn normalize_args(&self, args: &Value) -> Value {
        let mut normalized = args.clone();
        if let Some(obj) = normalized.as_object_mut() {
            if !obj.contains_key("path") {
                obj.insert("path".to_string(), Value::Null);
            }
            if !obj.contains_key("context_lines") {
                obj.insert("context_lines".to_string(), json!(0));
            }
            if !obj.contains_key("count_only") {
                obj.insert("count_only".to_string(), json!(false));
            }
            if !obj.contains_key("is_literal") {
                obj.insert("is_literal".to_string(), json!(false));
            }
        }
        normalized
    }

    async fn call(&self, args: Value, context: &SashikoToolContext) -> Result<Value> {
        let revision_raw = args["revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing revision"))?;
        let revision_virt = context.virtualize_ref(revision_raw);
        let revision = revision_virt.as_str();
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing pattern"))?;
        let path_str = args["path"].as_str();
        let context_lines = args["context_lines"].as_u64().unwrap_or(0) as usize;
        let count_only = args["count_only"].as_bool().unwrap_or(false);
        let is_literal = args["is_literal"].as_bool().unwrap_or(false);

        if revision.starts_with('-') {
            return Err(anyhow!("Invalid revision: {}", revision));
        }

        let mut cmd = Command::new("git");
        cmd.current_dir(&context.worktree_path).arg("grep");

        if count_only {
            cmd.arg("-c");
        } else {
            cmd.arg("-n").arg("-I").arg(format!("-C{}", context_lines));
        }

        if is_literal {
            cmd.arg("-F");
        } else {
            cmd.arg("-P");
        }

        cmd.arg("-e").arg(pattern).arg(revision);

        if let Some(p) = path_str
            && p != "."
            && !p.is_empty()
        {
            cmd.arg("--");
            for pathspec in p.split_whitespace() {
                cmd.arg(pathspec);
            }
        }

        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.is_empty() {
                return Ok(json!({
                    "content": "",
                    "truncated": false,
                    "metadata": { "total_items": 0, "returned_items": 0 },
                    "matches": [],
                    "message": "No matches found."
                }));
            }
            return Ok(json!({ "error": format!("git grep failed: {}", stderr) }));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        let formatted = if count_only {
            let prefix = format!("{}:", revision);
            content
                .lines()
                .map(|line| {
                    if line.starts_with(&prefix) {
                        &line[prefix.len()..]
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            let active_files = context.active_patch_files.read().unwrap();
            format_git_grep_output(&content, revision, &active_files)
        };

        let total_grep_lines = formatted.lines().count();
        let res = Truncator::truncate_sequential(&formatted, 10_000);
        let truncated_grep = res.content;
        let lines_kept = res.lines_kept;
        let is_truncated = res.truncated;

        let returned_items = if is_truncated && lines_kept > 0 {
            lines_kept
        } else {
            total_grep_lines
        };

        Ok(json!({
            "content": truncated_grep,
            "truncated": is_truncated,
            "metadata": {
                "total_items": total_grep_lines,
                "returned_items": returned_items
            },
            "next_page_hint": if is_truncated {
                Some("Grep matches were truncated. Narrow your search using a pathspec or a more specific regex pattern.".to_string())
            } else {
                None
            }
        }))
    }
}
