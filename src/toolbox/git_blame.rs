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
use crate::toolbox::command::capped_output;
use crate::toolbox::framework::LlmTool;
use crate::toolbox::utils::validate_path;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct GitBlameTool;

#[async_trait]
impl LlmTool<SashikoToolContext> for GitBlameTool {
    fn name(&self) -> &'static str {
        "git_blame"
    }

    fn description(&self) -> &'static str {
        "Show what revision and author last modified each line of a file."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "revision": { "type": "string", "description": "The Git commit SHA or reference to blame from." },
                "path": { "type": "string", "description": "Relative path to the file." },
                "start_line": { "type": "integer", "description": "1-based start line (optional)." },
                "end_line": { "type": "integer", "description": "1-based end line (optional)." }
            },
            "required": ["revision", "path"]
        })
    }

    fn normalize_args(&self, args: &Value) -> Value {
        let mut normalized = args.clone();
        if let Some(obj) = normalized.as_object_mut() {
            if !obj.contains_key("start_line") {
                obj.insert("start_line".to_string(), Value::Null);
            }
            if !obj.contains_key("end_line") {
                obj.insert("end_line".to_string(), Value::Null);
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
        // The revision sits before the "--", where git still reads options, so
        // a leading dash turns a model-chosen string into a flag. Patches come
        // from a public list and can steer the model, so this is reachable.
        if revision.starts_with('-') {
            return Err(anyhow!("Invalid revision name: {}", revision));
        }
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing path"))?;
        if path_str.starts_with('-') {
            return Err(anyhow!("Invalid path name: {}", path_str));
        }
        validate_path(path_str, &context.worktree_path)?;

        let start_line = args["start_line"].as_u64();
        let end_line = args["end_line"].as_u64();

        let mut cmd = crate::git_cmd::in_dir_async(&context.worktree_path);
        cmd.arg("blame");

        if let (Some(s), Some(e)) = (start_line, end_line) {
            cmd.arg(format!("-L{},{}", s, e));
        }

        cmd.arg(revision).arg("--").arg(path_str);

        let output = capped_output(&mut cmd).await?;
        if !output.is_usable() {
            return Err(anyhow!(
                "git blame failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        let total_blame_lines = content.lines().count();
        let res = Truncator::truncate_sequential(&content, 10_000);
        let truncated_content = res.content;
        let lines_kept = res.lines_kept;
        let is_truncated = res.truncated;

        let start = start_line.unwrap_or(1);
        let end_idx = if is_truncated && lines_kept > 0 {
            start + lines_kept as u64 - 1
        } else {
            start + total_blame_lines as u64 - 1
        };

        let returned_items = if is_truncated && lines_kept > 0 {
            lines_kept
        } else {
            total_blame_lines
        };

        Ok(json!({
            "content": truncated_content,
            "truncated": is_truncated,
            "metadata": {
                "total_items": total_blame_lines,
                "returned_items": returned_items,
                "start_index": start,
                "end_index": end_idx
            },
            "next_page_hint": if is_truncated {
                Some(format!("Only the first {} lines of blame are shown. To view the remaining blame lines, use start_line={}.", returned_items, start + returned_items as u64))
            } else {
                None
            }
        }))
    }
}
