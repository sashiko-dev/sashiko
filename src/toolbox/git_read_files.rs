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
use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

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
        if let Some(obj) = normalized.as_object_mut() {
            if !obj.contains_key("mode") {
                obj.insert("mode".to_string(), json!("raw"));
            }
            if let Some(files) = obj.get_mut("files").and_then(|f| f.as_array_mut()) {
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

        let mut results = Vec::new();

        for file_args in files {
            let path_str = file_args["path"].as_str().unwrap_or_default();
            if path_str.is_empty() {
                results.push(json!({ "error": "Missing path" }));
                continue;
            }

            let start_line = file_args["start_line"].as_u64().map(|v| v as usize);
            let end_line = file_args["end_line"].as_u64().map(|v| v as usize);

            match self
                .read_single_file(context, revision, path_str, start_line, end_line)
                .await
            {
                Ok(mut val) => {
                    if let Some(obj) = val.as_object_mut() {
                        obj.insert("path".to_string(), json!(path_str));
                    }
                    results.push(val);
                }
                Err(e) => {
                    results.push(json!({
                        "path": path_str,
                        "error": e.to_string()
                    }));
                }
            }
        }

        Ok(json!({ "results": results }))
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
    ) -> Result<Value> {
        let revision_virt = context.virtualize_ref(revision);
        let revision = revision_virt.as_str();
        if path_str.starts_with('-') {
            return Err(anyhow!("Invalid path name: {}", path_str));
        }

        let mut cmd = Command::new("git");
        cmd.current_dir(&context.worktree_path)
            .args(["show", &format!("{}:{}", revision, path_str)]);

        let output = cmd.output().await?;
        if !output.status.success() {
            return Err(anyhow!(
                "git show failed to read file {} at {}: {}",
                path_str,
                revision,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();

        if let (Some(s), Some(e)) = (start_line, end_line) {
            ensure!(s <= e, "Invalid range: start_line ({s}) > end_line ({e})");
        }

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // total_lines is 0 for an empty file, and clamp() panics when its
        // lower bound exceeds its upper one. The bounds below already handle
        // the empty case; this pre-clamp only has to avoid tripping over it.
        let start_line = start_line.map(|s| s.clamp(1, total_lines.max(1)));
        let end_line = end_line.map(|e| e.clamp(1, total_lines.max(1)));

        let (start, end) = match (start_line, end_line) {
            (Some(s), Some(e)) => (s.max(1) - 1, e.min(total_lines)),
            (Some(s), None) => (s.max(1) - 1, total_lines),
            (None, Some(e)) => (0, e.min(total_lines)),
            (None, None) => (0, total_lines),
        };

        let start = start.min(total_lines);
        let end = end.clamp(start, total_lines);

        if start >= total_lines {
            return Ok(json!({
                "content": "",
                "truncated": false,
                "metadata": {
                    "total_items": total_lines,
                    "returned_items": 0,
                    "start_index": start + 1,
                    "end_index": end
                },
                "lines_read": 0,
                "total_lines": total_lines
            }));
        }

        let slice = &lines[start..end];
        let result = slice.join("\n");

        let res = Truncator::truncate_sequential(&result, 20_000);
        let truncated = res.content;
        let lines_kept = res.lines_kept;
        let is_truncated_content = res.truncated;

        let start_idx = start + 1;
        let is_truncated = is_truncated_content;

        let end_idx = if is_truncated_content && lines_kept > 0 {
            start + lines_kept
        } else {
            end
        };

        let returned_items = if is_truncated_content && lines_kept > 0 {
            lines_kept
        } else {
            slice.len()
        };

        let next_page_hint = if is_truncated {
            Some(format!(
                "Only lines {}-{} of {} are shown due to token limits. To read the remaining lines, call git_read_files with start_line={}.",
                start_idx,
                end_idx,
                total_lines,
                end_idx + 1
            ))
        } else {
            None
        };

        Ok(json!({
            "content": truncated,
            "truncated": is_truncated,
            "metadata": {
                "total_items": total_lines,
                "returned_items": returned_items,
                "start_index": start_idx,
                "end_index": end_idx
            },
            "next_page_hint": next_page_hint,
            "lines_read": returned_items,
            "total_lines": total_lines,
            "start_line": start_idx,
            "end_line": end_idx
        }))
    }
}
