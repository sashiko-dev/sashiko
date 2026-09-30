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
use crate::toolbox::framework::LlmTool;
use crate::toolbox::utils::{glob_to_regex, validate_path};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;

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

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdout"))?;
        let mut reader = tokio::io::BufReader::new(stdout).lines();

        let regex = glob_to_regex(pattern)?;
        let mut matched_files = Vec::new();
        let mut total_found = 0;
        let mut is_truncated = false;

        while let Some(line) = reader.next_line().await? {
            if regex.is_match(&line) {
                total_found += 1;
                if matched_files.len() < 1000 {
                    matched_files.push(line);
                } else {
                    is_truncated = true;
                    let _ = child.kill().await;
                    break;
                }
            }
        }

        let status = child.wait().await?;
        if !is_truncated && !status.success() {
            let mut stderr_str = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = stderr.read_to_string(&mut stderr_str).await;
            }
            return Err(anyhow!("git ls-tree failed: {}", stderr_str.trim()));
        }

        let truncated_files = matched_files.join("\n");

        Ok(json!({
            "content": truncated_files,
            "truncated": is_truncated,
            "metadata": {
                "total_items": total_found,
                "returned_items": if is_truncated { 1000 } else { total_found }
            },
            "next_page_hint": if is_truncated {
                Some("More than 1000 files matched. Please use a narrower path or pattern prefix to restrict search.".to_string())
            } else {
                None
            },
            "files": truncated_files,
            "total_found": total_found,
            "message": if is_truncated { Some("Output truncated to 1000 files.") } else { None }
        }))
    }
}
