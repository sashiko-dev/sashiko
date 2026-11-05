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
use crate::toolbox::utils::{glob_to_regex, validate_path};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct GitFindFilesTool;

#[async_trait]
impl LlmTool<SashikoToolContext> for GitFindFilesTool {
    fn name(&self) -> &'static str {
        "git_find_files"
    }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern in a specific Git revision."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "revision": { "type": "string", "description": "The Git commit SHA or reference to search in." },
                "pattern": { "type": "string", "description": "Glob pattern to match (e.g., '*.rs' or 'src/**/mod.rs')." },
                "path": { "type": "string", "description": "Optional relative path to restrict the search (e.g., 'drivers/net/')." }
            },
            "required": ["revision", "pattern"]
        })
    }

    fn normalize_args(&self, args: &Value) -> Value {
        let mut normalized = args.clone();
        if let Some(obj) = normalized.as_object_mut()
            && !obj.contains_key("path")
        {
            obj.insert("path".to_string(), Value::Null);
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

        if revision.starts_with('-') {
            return Err(anyhow!("Invalid revision"));
        }

        let regex = glob_to_regex(pattern)?;

        let mut cmd = crate::git_cmd::in_dir_async(&context.worktree_path);
        cmd.args(["ls-tree", "-r", "--name-only", revision]);

        if let Some(p) = path_str
            && p != "."
            && !p.is_empty()
        {
            if p.starts_with('-') {
                return Err(anyhow!("Invalid path parameter"));
            }
            validate_path(p, &context.worktree_path)?;
            cmd.arg("--").arg(p);
        }

        let mut matched_files = Vec::new();
        let mut total_found = 0;
        let mut is_truncated = false;

        let output = for_each_line(&mut cmd, |line| {
            if regex.is_match(line) {
                total_found += 1;
                if matched_files.len() < 1000 {
                    matched_files.push(line.to_string());
                } else {
                    is_truncated = true;
                }
            }
        })
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(anyhow!("git ls-tree failed: {}", stderr));
        }

        if total_found == 0 {
            return Ok(Value::String("No matching files found.".to_string()));
        }

        let body = matched_files.join("\n");
        let out = if is_truncated {
            format!(
                "Found {total_found} matching files (showing first 1000):\n{body}\n[Truncated: more than 1000 files matched. Use a narrower path or pattern.]"
            )
        } else {
            let noun = if total_found == 1 { "file" } else { "files" };
            format!("Found {total_found} matching {noun}:\n{body}")
        };

        Ok(Value::String(out))
    }
}
