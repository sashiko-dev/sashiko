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
use crate::toolbox::command::capped_output;
use crate::toolbox::framework::LlmTool;
use crate::toolbox::utils::{format_numbered_slice, validate_path};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct GitReadFilesTool;

#[async_trait]
impl LlmTool<SashikoToolContext> for GitReadFilesTool {
    fn name(&self) -> &'static str {
        "git_read_files"
    }

    fn description(&self) -> &'static str {
        "Read files at a Git revision."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "revision": { "type": "string", "description": "Git SHA or reference to read from." },
                "files": {
                    "type": "array",
                    "description": "List of files to read (max 10).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "File path." },
                            "start_line": { "type": "integer", "description": "Focus start line (optional)." },
                            "end_line": { "type": "integer", "description": "Focus end line (optional)." }
                        },
                        "required": ["path"]
                    }
                }
            },
            "required": ["revision", "files"]
        })
    }

    fn normalize_args(&self, args: &Value) -> Value {
        let mut normalized = args.clone();
        if let Some(obj) = normalized.as_object_mut()
            && let Some(files) = obj.get_mut("files").and_then(|f| f.as_array_mut())
        {
            for file in files {
                if let Some(f_obj) = file.as_object_mut() {
                    if !f_obj.contains_key("start_line") {
                        f_obj.insert("start_line".to_string(), Value::Null);
                    }
                    if !f_obj.contains_key("end_line") {
                        f_obj.insert("end_line".to_string(), Value::Null);
                    }
                }
            }
        }
        normalized
    }

    async fn call(&self, args: Value, context: &SashikoToolContext) -> Result<Value> {
        let revision = args["revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing revision"))?;
        let files = args["files"]
            .as_array()
            .ok_or_else(|| anyhow!("Missing files"))?;
        if files.len() > 10 {
            return Err(anyhow!(
                "Too many files requested. Maximum limit is 10 files per request."
            ));
        }

        let mut sections = Vec::with_capacity(files.len());

        for file_args in files {
            let path_str = file_args["path"].as_str().unwrap_or_default();
            if path_str.is_empty() {
                sections.push("=== <missing path>: ERROR: Missing path ===".to_string());
                continue;
            }

            let start_line = file_args["start_line"].as_u64().map(|v| v as usize);
            let end_line = file_args["end_line"].as_u64().map(|v| v as usize);

            match self
                .read_single_file(context, revision, path_str, start_line, end_line)
                .await
            {
                Ok(section) => sections.push(section),
                Err(e) => sections.push(format!("=== {path_str}: ERROR: {e} ===")),
            }
        }

        Ok(Value::String(sections.join("\n\n")))
    }
}

impl GitReadFilesTool {
    async fn read_single_file(
        &self,
        context: &SashikoToolContext,
        revision: &str,
        path_str: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<String> {
        let revision_virt = context.virtualize_ref(revision);
        let revision = revision_virt.as_str();
        // Revision and path are joined into a single argument, so a leading
        // dash on either end makes git read the whole thing as an option
        // rather than as a rev:path pair.
        if revision.starts_with('-') {
            return Err(anyhow!("Invalid revision name: {}", revision));
        }
        if path_str.starts_with('-') {
            return Err(anyhow!("Invalid path name: {}", path_str));
        }
        validate_path(path_str, &context.worktree_path)?;

        let mut cmd = crate::git_cmd::in_dir_async(&context.worktree_path);
        cmd.args(["show", &format!("{}:{}", revision, path_str)]);

        let output = capped_output(&mut cmd).await?;
        if !output.is_usable() {
            return Err(anyhow!(
                "git show failed to read file {} at {}: {}",
                path_str,
                revision,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        format_numbered_slice(
            path_str,
            "git_read_files",
            &content,
            start_line,
            end_line,
            20_000,
        )
    }
}
