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

use crate::toolbox::SashikoToolContext;
use crate::toolbox::command::for_each_line;
use crate::toolbox::framework::LlmTool;
use crate::toolbox::utils::validate_path;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct GitLsTool;

#[async_trait]
impl LlmTool<SashikoToolContext> for GitLsTool {
    fn name(&self) -> &'static str {
        "git_ls"
    }

    fn description(&self) -> &'static str {
        "List files in a directory at a specific Git revision."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "revision": { "type": "string", "description": "The Git commit SHA or reference to list from." },
                "path": { "type": "string", "description": "Relative path to the directory (e.g., '.' or 'src/')." }
            },
            "required": ["revision", "path"]
        })
    }

    async fn call(&self, args: Value, context: &SashikoToolContext) -> Result<Value> {
        let revision_raw = args["revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing revision"))?;
        let revision_virt = context.virtualize_ref(revision_raw);
        let revision = revision_virt.as_str();
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing path"))?;

        if revision.starts_with('-') || path_str.starts_with('-') {
            return Err(anyhow!("Invalid revision or path name"));
        }

        let tree_spec = if path_str.is_empty() || path_str == "." {
            revision.to_string()
        } else {
            validate_path(path_str, &context.worktree_path)?;
            format!("{}:{}", revision, path_str)
        };

        let mut cmd = crate::git_cmd::in_dir_async(&context.worktree_path);
        cmd.args(["ls-tree", &tree_spec]);

        let mut entries = Vec::new();
        let mut total_entries = 0;

        let output = for_each_line(&mut cmd, |line| {
            let split_tab: Vec<&str> = line.split('\t').collect();
            if split_tab.len() >= 2 {
                let filename = split_tab[1];
                let metadata: Vec<&str> = split_tab[0].split_whitespace().collect();
                if metadata.len() >= 2 {
                    total_entries += 1;
                    if entries.len() < 1000 {
                        let formatted = match metadata[1] {
                            "tree" => format!("{filename}/"),
                            _ => filename.to_string(),
                        };
                        entries.push(formatted);
                    }
                }
            }
        })
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Ok(Value::String(format!(
                "Error: git ls-tree failed for {}: {}",
                tree_spec, stderr
            )));
        }

        if total_entries == 0 {
            return Ok(Value::String("Directory is empty.".to_string()));
        }

        let mut out = entries.join("\n");
        if total_entries > 1000 {
            out.push_str(&format!(
                "\n[Truncated: showing 1000 of {total_entries} entries. Call git_ls with a specific subdirectory path.]"
            ));
        }

        Ok(Value::String(out))
    }
}
