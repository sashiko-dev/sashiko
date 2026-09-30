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

pub struct GitDiffTool;

#[async_trait]
impl LlmTool<SashikoToolContext> for GitDiffTool {
    fn name(&self) -> &'static str {
        "git_diff"
    }

    fn description(&self) -> &'static str {
        "Show changes between two commits or revisions."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "base_revision": { "type": "string", "description": "The baseline commit SHA or revision reference." },
                "target_revision": { "type": "string", "description": "The target commit SHA or revision reference to compare against." },
                "paths": {
                    "type": "array",
                    "description": "Optional relative file or directory paths to filter the diff (e.g. ['fs/', 'drivers/net/']).",
                    "items": { "type": "string" }
                }
            },
            "required": ["base_revision", "target_revision"]
        })
    }

    fn normalize_args(&self, args: &Value) -> Value {
        let mut normalized = args.clone();
        if let Some(obj) = normalized.as_object_mut()
            && !obj.contains_key("paths")
        {
            obj.insert("paths".to_string(), Value::Null);
        }
        normalized
    }

    async fn call(&self, args: Value, context: &SashikoToolContext) -> Result<Value> {
        let base_raw = args["base_revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing base_revision"))?;
        let base_virt = context.virtualize_ref(base_raw);
        let base = base_virt.as_str();
        let target_raw = args["target_revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing target_revision"))?;
        let target_virt = context.virtualize_ref(target_raw);
        let target = target_virt.as_str();

        if base.starts_with('-') || target.starts_with('-') {
            return Err(anyhow!("Invalid revision names"));
        }

        let mut cmd = crate::git_cmd::in_dir_async(&context.worktree_path);
        cmd.args(["diff", "--diff-algorithm=histogram", base, target]);

        if let Some(paths_val) = args["paths"].as_array() {
            cmd.arg("--");
            for p in paths_val {
                if let Some(p_str) = p.as_str() {
                    if p_str.starts_with('-') {
                        return Err(anyhow!("Invalid path parameter: {}", p_str));
                    }
                    validate_path(p_str, &context.worktree_path)?;
                    cmd.arg(p_str);
                }
            }
        }

        let output = capped_output(&mut cmd).await?;

        if !output.is_usable() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(anyhow!("git diff failed: {}", stderr));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        let total_diff_lines = content.lines().count();
        let res = Truncator::truncate_diff(&content, 10_000, "Diff");
        let truncated_diff = res.content;
        let is_truncated = res.truncated;
        let returned_diff_lines = truncated_diff.lines().count();

        Ok(json!({
            "content": truncated_diff,
            "truncated": is_truncated,
            "metadata": {
                "total_items": total_diff_lines,
                "returned_items": returned_diff_lines
            },
            "next_page_hint": if is_truncated {
                Some("This diff is too large and was truncated by dropping the middle. To see complete changes, filter by specific 'paths' (e.g., folders/files).".to_string())
            } else {
                None
            }
        }))
    }
}
