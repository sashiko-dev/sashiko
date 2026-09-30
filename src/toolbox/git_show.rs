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

pub struct GitShowTool;

#[async_trait]
impl LlmTool<SashikoToolContext> for GitShowTool {
    fn name(&self) -> &'static str {
        "git_show"
    }

    fn description(&self) -> &'static str {
        "Show commits, trees, tags or blobs."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object": { "type": "string", "description": "Git object specifier (e.g. 'HEAD:README.md' or 'HEAD')." },
                "suppress_diff": { "type": "boolean", "description": "If true, suppresses commit diff output." },
                "start_line": { "type": "integer", "description": "Focus start line (blobs only, optional)." },
                "end_line": { "type": "integer", "description": "Focus end line (blobs only, optional)." },
                "paths": {
                    "type": "array",
                    "description": "Path filters (commits only, optional).",
                    "items": { "type": "string" }
                }
            },
            "required": ["object"]
        })
    }

    fn normalize_args(&self, args: &Value) -> Value {
        let mut normalized = args.clone();
        if let Some(obj) = normalized.as_object_mut() {
            if !obj.contains_key("suppress_diff") {
                obj.insert("suppress_diff".to_string(), json!(false));
            }
            if !obj.contains_key("start_line") {
                obj.insert("start_line".to_string(), Value::Null);
            }
            if !obj.contains_key("end_line") {
                obj.insert("end_line".to_string(), Value::Null);
            }
            if !obj.contains_key("paths") {
                obj.insert("paths".to_string(), Value::Null);
            }
            if !obj.contains_key("mode") {
                obj.insert("mode".to_string(), json!("raw"));
            }
        }
        normalized
    }

    async fn call(&self, args: Value, context: &SashikoToolContext) -> Result<Value> {
        let object_raw = args["object"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing object"))?;
        let object_virt = context.virtualize_ref(object_raw);
        let object = object_virt.as_str();
        let suppress_diff = args["suppress_diff"].as_bool().unwrap_or(false);
        let start_line = args["start_line"].as_u64().map(|v| v as usize);
        let end_line = args["end_line"].as_u64().map(|v| v as usize);

        if object.starts_with('-') {
            return Err(anyhow!("Invalid object name: {}", object));
        }

        if let Some((rev_part, path_part)) = object.split_once(':')
            && !rev_part.is_empty()
            && !rev_part.ends_with('{')
            && !path_part.is_empty()
        {
            if path_part.starts_with('-') {
                return Err(anyhow!("Invalid path parameter: {}", path_part));
            }
            validate_path(path_part, &context.worktree_path)?;
        }

        let mut path_args = Vec::new();
        if let Some(paths_val) = args["paths"].as_array() {
            for p in paths_val {
                if let Some(p_str) = p.as_str() {
                    if p_str.starts_with('-') {
                        return Err(anyhow!("Invalid path parameter: {}", p_str));
                    }
                    validate_path(p_str, &context.worktree_path)?;
                    path_args.push(p_str);
                }
            }
        }

        let raw_key = format!(
            "git_show_raw:{}:{}:{:?}",
            object,
            suppress_diff,
            args.get("paths")
        );

        let content = {
            let cached_raw = {
                let cache = context.cache.read().unwrap();
                cache.get(&raw_key).cloned()
            };

            if let Some(Value::String(raw_str)) = cached_raw {
                raw_str
            } else {
                let mut cmd = crate::git_cmd::in_dir_async(&context.worktree_path);
                cmd.arg("show");

                if suppress_diff {
                    cmd.arg("--no-patch");
                }

                cmd.arg(object);

                if !path_args.is_empty() {
                    cmd.arg("--");
                    cmd.args(&path_args);
                }

                let output = capped_output(&mut cmd).await?;

                if !output.is_usable() {
                    return Err(anyhow!(
                        "git show failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }

                let raw_str = String::from_utf8_lossy(&output.stdout).to_string();
                {
                    let mut cache = context.cache.write().unwrap();
                    cache.insert(raw_key, Value::String(raw_str.clone()));
                }
                raw_str
            }
        };

        let is_file = object.contains(':') && !object.starts_with(':');
        if start_line.is_some() || end_line.is_some() {
            let lines: Vec<&str> = content.lines().collect();
            let total_lines = lines.len();

            // Default page limit of 100 lines if end_line is missing but start_line is present
            let resolved_end_line = match (start_line, end_line) {
                (Some(s), None) => Some(s + 100),
                (_, e) => e,
            };

            let (start, end) = match (start_line, resolved_end_line) {
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

            let (truncated, lines_kept, is_truncated_content) = if is_file {
                let res = Truncator::truncate_sequential(&result, 20_000);
                (res.content, res.lines_kept, res.truncated)
            } else {
                let res = Truncator::truncate_diff(&result, 10_000, "Commit");
                (res.content, 0, res.truncated)
            };

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

            return Ok(json!({
                "content": truncated,
                "truncated": is_truncated,
                "metadata": {
                    "total_items": total_lines,
                    "returned_items": returned_items,
                    "start_index": start + 1,
                    "end_index": end_idx
                },
                "next_page_hint": if is_truncated {
                    Some(format!("Only lines {}-{} of {} are shown. To read more, call git_show with start_line={}.", start + 1, end_idx, total_lines, end_idx + 1))
                } else {
                    None
                },
                "total_lines": total_lines,
                "start_line": start + 1,
                "end_line": end
            }));
        }

        let total_lines = content.lines().count();
        let (truncated, is_truncated) = if is_file {
            let res = Truncator::truncate_sequential(&content, 20_000);
            (res.content, res.truncated)
        } else {
            let res = Truncator::truncate_diff(&content, 10_000, "Commit");
            (res.content, res.truncated)
        };
        let returned_lines = truncated.lines().count();

        Ok(json!({
            "content": truncated,
            "truncated": is_truncated,
            "metadata": {
                "total_items": total_lines,
                "returned_items": returned_lines
            },
            "next_page_hint": if is_truncated {
                Some("This content was truncated due to token budget. Specify a start_line range to fetch the next slice.".to_string())
            } else {
                None
            }
        }))
    }
}
