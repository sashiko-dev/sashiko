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

use crate::ai::AiTool;
use crate::ai::truncator::Truncator;
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::fs;

use tokio::process::Command;

pub struct ToolBox {
    worktree_path: PathBuf,
    prompts_path: Option<PathBuf>,
    active_patch_files: Vec<String>,
    pub(crate) cache: std::sync::RwLock<std::collections::HashMap<String, Value>>,
}

impl ToolBox {
    pub fn new(worktree_path: PathBuf, prompts_path: Option<PathBuf>) -> Self {
        Self {
            worktree_path,
            prompts_path,
            active_patch_files: Vec::new(),
            cache: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    pub fn set_active_patch_files(&mut self, files: Vec<String>) {
        self.active_patch_files = files;
    }

    pub fn get_worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    /// Returns generic tool declarations.
    pub fn get_declarations_generic(&self) -> Vec<AiTool> {
        let mut decls = vec![
             AiTool {
                name: "git_read_files".to_string(),
                description: "Read files at a Git revision.".to_string(),
                parameters: json!({
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
                }),
            },
            AiTool {
                name: "git_blame".to_string(),
                description: "Show what revision and author last modified each line of a file."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "revision": { "type": "string", "description": "The Git commit SHA or reference to blame from." },
                        "path": { "type": "string", "description": "Relative path to the file." },
                        "start_line": { "type": "integer", "description": "1-based start line (optional)." },
                        "end_line": { "type": "integer", "description": "1-based end line (optional)." }
                    },
                    "required": ["revision", "path"]
                }),
            },
            AiTool {
                name: "git_diff".to_string(),
                description: "Show changes between two commits or revisions."
                    .to_string(),
                parameters: json!({
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
                }),
            },
            AiTool {
                name: "git_show".to_string(),
                description: "Show commits, trees, tags or blobs.".to_string(),
                parameters: json!({
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
                }),
            },
            AiTool {
                name: "git_log".to_string(),
                description: "Show commit logs in a specific range or revision history.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "range": { "type": "string", "description": "The commit range or reference to view logs for (e.g., 'baseline..HEAD' or 'HEAD')." },
                        "limit": { "type": "integer", "description": "Limit the number of commits returned (defaults to 10, max 100)." }
                    },
                    "required": ["range"]
                }),
            },
            AiTool {
                name: "git_ls".to_string(),
                description: "List files in a directory at a specific Git revision.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "revision": { "type": "string", "description": "The Git commit SHA or reference to list from." },
                        "path": { "type": "string", "description": "Relative path to the directory (e.g., '.' or 'src/')." }
                    },
                    "required": ["revision", "path"]
                }),
            },
             AiTool {
                name: "git_grep".to_string(),
                description: "Search for a pattern in files using git grep at a specific Git revision. Returns matching lines with context.".to_string(),
                parameters: json!({
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
                }),
            },
            AiTool {
                name: "git_find_files".to_string(),
                description: "Find files matching a glob pattern in a specific Git revision.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "revision": { "type": "string", "description": "The Git commit SHA or reference to search in." },
                        "pattern": { "type": "string", "description": "Glob pattern to match (e.g., '*.rs' or 'src/**/mod.rs')." },
                        "path": { "type": "string", "description": "Optional relative path to restrict the search (e.g., 'drivers/net/')." }
                    },
                    "required": ["revision", "pattern"]
                }),
            },

        ];

        if self.prompts_path.is_some() {
            decls.push(AiTool {
                name: "read_prompt".to_string(),
                description: "Read a specific prompt file from the prompt registry (e.g., 'mm.md', 'locking.md').".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Name of the prompt file (e.g., 'patterns/BPF-001.md')." }
                    },
                    "required": ["name"]
                }),
            });
        }

        decls
    }

    fn normalize_tool_args(name: &str, args: &Value) -> Value {
        let mut normalized = args.clone();
        let name_normalized = name.trim().to_lowercase();
        if let Some(obj) = normalized.as_object_mut() {
            match name_normalized.as_str() {
                "git_read_files" => {
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
                "git_blame" => {
                    if !obj.contains_key("start_line") {
                        obj.insert("start_line".to_string(), Value::Null);
                    }
                    if !obj.contains_key("end_line") {
                        obj.insert("end_line".to_string(), Value::Null);
                    }
                }
                "git_diff" => {
                    if !obj.contains_key("paths") {
                        obj.insert("paths".to_string(), Value::Null);
                    }
                }
                "git_show" => {
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
                "git_log" => {
                    if !obj.contains_key("limit") {
                        obj.insert("limit".to_string(), json!(10));
                    }
                }
                "git_grep" => {
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
                "git_find_files" => {
                    if !obj.contains_key("path") {
                        obj.insert("path".to_string(), Value::Null);
                    }
                }
                _ => {}
            }
        }
        normalized
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let name_normalized = name.trim().to_lowercase();
        let should_cache = name_normalized != "todowrite";

        let normalized_args = Self::normalize_tool_args(&name_normalized, &args);

        let key = if should_cache {
            let k = format!(
                "{}:{}",
                name_normalized,
                serde_json::to_string(&normalized_args)?
            );
            {
                let cache = self.cache.read().unwrap();
                if let Some(val) = cache.get(&k) {
                    return Ok(val.clone());
                }
            }
            Some(k)
        } else {
            None
        };

        let res = match name_normalized.as_str() {
            "git_read_files" => self.read_files(args).await,
            "git_blame" => self.git_blame(args).await,
            "git_diff" => self.git_diff(args).await,
            "git_show" => self.git_show(args).await,
            "git_log" => self.git_log(args).await,
            "git_ls" => self.git_ls(args).await,
            "git_grep" => self.git_grep(args).await,
            "git_find_files" => self.find_files(args).await,
            "read_prompt" => self.read_prompt(args).await,
            _ => Err(anyhow!("Unknown tool: {}", name)),
        }?;

        if let Some(k) = key {
            let mut cache = self.cache.write().unwrap();
            cache.insert(k, res.clone());
        }

        Ok(res)
    }

    async fn read_prompt(&self, args: Value) -> Result<Value> {
        let prompts_path = self
            .prompts_path
            .as_ref()
            .ok_or_else(|| anyhow!("read_prompt tool is not available"))?;
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing prompt name"))?;

        let path = self.validate_path(name, prompts_path)?;
        let content = fs::read_to_string(path).await?;

        Ok(json!({ "content": content }))
    }

    async fn read_files(&self, args: Value) -> Result<Value> {
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
                .read_single_file(revision, path_str, start_line, end_line)
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

    async fn read_single_file(
        &self,
        revision: &str,
        path_str: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<Value> {
        if path_str.starts_with('-') {
            return Err(anyhow!("Invalid path name: {}", path_str));
        }

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.worktree_path)
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

        let start_line = start_line.map(|s| s.clamp(1, total_lines));
        let end_line = end_line.map(|e| e.clamp(1, total_lines));

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

            // Backwards compatibility
            "lines_read": returned_items,
            "total_lines": total_lines,
            "start_line": start_idx,
            "end_line": end_idx
        }))
    }

    async fn git_blame(&self, args: Value) -> Result<Value> {
        let revision = args["revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing revision"))?;
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing path"))?;
        let start_line = args["start_line"].as_u64();
        let end_line = args["end_line"].as_u64();

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.worktree_path).arg("blame");

        if let (Some(s), Some(e)) = (start_line, end_line) {
            cmd.arg(format!("-L{},{}", s, e));
        }

        cmd.arg(revision).arg("--").arg(path_str);

        let output = cmd.output().await?;
        if !output.status.success() {
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

    async fn git_diff(&self, args: Value) -> Result<Value> {
        let base = args["base_revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing base_revision"))?;
        let target = args["target_revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing target_revision"))?;

        if base.starts_with('-') || target.starts_with('-') {
            return Err(anyhow!("Invalid revision names"));
        }

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.worktree_path).args([
            "diff",
            "--diff-algorithm=histogram",
            base,
            target,
        ]);

        if let Some(paths_val) = args["paths"].as_array() {
            cmd.arg("--");
            for p in paths_val {
                if let Some(p_str) = p.as_str() {
                    if p_str.starts_with('-') {
                        return Err(anyhow!("Invalid path parameter: {}", p_str));
                    }
                    cmd.arg(p_str);
                }
            }
        }

        let output = cmd.output().await?;

        if !output.status.success() {
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

    async fn git_log(&self, args: Value) -> Result<Value> {
        let range = args["range"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing range"))?;
        let limit = args["limit"].as_u64().unwrap_or(10).min(100) as usize;

        if range.starts_with('-') {
            return Err(anyhow!("Invalid range"));
        }

        let limit_str = limit.to_string();
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.worktree_path)
            .args(["log", "-n", &limit_str, range])
            .kill_on_drop(true);

        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Ok(json!({ "error": format!("git log failed: {}", stderr) }));
        }

        let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let total_log_lines = raw_stdout.lines().count();
        let res = Truncator::truncate_sequential(&raw_stdout, 10_000);
        let truncated_log = res.content;
        let lines_kept = res.lines_kept;
        let is_truncated = res.truncated;

        let returned_items = if is_truncated && lines_kept > 0 {
            lines_kept
        } else {
            total_log_lines
        };

        Ok(json!({
            "content": truncated_log,
            "truncated": is_truncated,
            "metadata": {
                "total_items": total_log_lines,
                "returned_items": returned_items
            },
            "next_page_hint": if is_truncated {
                Some("The log output was truncated. Use a smaller commit range or set a lower 'limit' parameter.".to_string())
            } else {
                None
            },

            // Backwards compatibility
            "output": truncated_log
        }))
    }

    async fn git_show(&self, args: Value) -> Result<Value> {
        let object = args["object"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing object"))?;
        let suppress_diff = args["suppress_diff"].as_bool().unwrap_or(false);
        let start_line = args["start_line"].as_u64().map(|v| v as usize);
        let end_line = args["end_line"].as_u64().map(|v| v as usize);

        if object.starts_with('-') {
            return Err(anyhow!("Invalid object name: {}", object));
        }

        let raw_key = format!(
            "git_show_raw:{}:{}:{:?}",
            object,
            suppress_diff,
            args.get("paths")
        );

        let content = {
            let cached_raw = {
                let cache = self.cache.read().unwrap();
                cache.get(&raw_key).cloned()
            };

            if let Some(Value::String(raw_str)) = cached_raw {
                raw_str
            } else {
                let mut cmd = Command::new("git");
                cmd.current_dir(&self.worktree_path).arg("show");

                if suppress_diff {
                    cmd.arg("--no-patch");
                }

                cmd.arg(object);

                if let Some(paths_val) = args["paths"].as_array() {
                    cmd.arg("--");
                    for p in paths_val {
                        if let Some(p_str) = p.as_str() {
                            if p_str.starts_with('-') {
                                return Err(anyhow!("Invalid path parameter: {}", p_str));
                            }
                            cmd.arg(p_str);
                        }
                    }
                }

                let output = cmd.output().await?;

                if !output.status.success() {
                    return Err(anyhow!(
                        "git show failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }

                let raw_str = String::from_utf8_lossy(&output.stdout).to_string();
                {
                    let mut cache = self.cache.write().unwrap();
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
                (Some(s), None) => (s.max(1) - 1, total_lines), // Should not happen
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

                // Backwards compatibility
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

    async fn git_ls(&self, args: Value) -> Result<Value> {
        let revision = args["revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing revision"))?;
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing path"))?;

        if revision.starts_with('-') || path_str.starts_with('-') {
            return Err(anyhow!("Invalid revision or path name"));
        }

        let tree_spec = if path_str.is_empty() || path_str == "." {
            revision.to_string()
        } else {
            format!("{}:{}", revision, path_str)
        };

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.worktree_path)
            .args(["ls-tree", &tree_spec]);

        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Ok(
                json!({ "error": format!("git ls-tree failed for {}: {}", tree_spec, stderr) }),
            );
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        let mut entries = Vec::new();
        for line in content.lines() {
            let split_tab: Vec<&str> = line.split('\t').collect();
            if split_tab.len() >= 2 {
                let filename = split_tab[1];
                let metadata: Vec<&str> = split_tab[0].split_whitespace().collect();
                if metadata.len() >= 2 {
                    let ty = match metadata[1] {
                        "tree" => "dir",
                        _ => "file",
                    };
                    entries.push(json!({ "name": filename, "type": ty }));
                }
            }
        }

        let total_entries = entries.len();
        let truncated = total_entries > 1000;
        if truncated {
            entries.truncate(1000);
        }

        Ok(json!({
            "entries": entries,
            "truncated": truncated,
            "total_entries": total_entries,
            "next_page_hint": if truncated {
                Some("Directory listing truncated to 1000 entries. Please call git_ls with a specific subdirectory path (e.g., 'src/worker/') to see more files.".to_string())
            } else {
                None
            }
        }))
    }

    async fn git_grep(&self, args: Value) -> Result<Value> {
        let revision = args["revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing revision"))?;
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing pattern"))?;
        let path_str = args["path"].as_str();
        let context_lines = args["context_lines"].as_u64().unwrap_or(0) as usize;
        let count_only = args["count_only"].as_bool().unwrap_or(false);
        let is_literal = args["is_literal"].as_bool().unwrap_or(false);

        if revision.starts_with('-') || pattern.starts_with('-') {
            return Err(anyhow!("Invalid revision or pattern"));
        }

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.worktree_path).arg("grep");

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

        cmd.arg(pattern).arg(revision);

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
            format_git_grep_output(&content, revision, &self.active_patch_files)
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

    async fn find_files(&self, args: Value) -> Result<Value> {
        let revision = args["revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing revision"))?;
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing pattern"))?;

        let path_str = args["path"].as_str();

        if revision.starts_with('-') {
            return Err(anyhow!("Invalid revision"));
        }

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.worktree_path)
            .args(["ls-tree", "-r", "--name-only", revision]);

        if let Some(p) = path_str
            && p != "."
            && !p.is_empty()
        {
            if p.starts_with('-') {
                return Err(anyhow!("Invalid path parameter"));
            }
            cmd.arg("--").arg(p);
        }

        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Ok(json!({ "error": format!("git ls-tree failed: {}", stderr) }));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        let files: Vec<&str> = content.lines().collect();

        let regex = glob_to_regex(pattern)?;
        let mut matched_files = Vec::new();
        for f in files {
            if regex.is_match(f) {
                matched_files.push(f);
            }
        }

        let total_found = matched_files.len();
        let (truncated_files, is_truncated) = if total_found > 1000 {
            (matched_files[..1000].join("\n"), true)
        } else {
            (matched_files.join("\n"), false)
        };

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

            // Backwards compatibility
            "files": truncated_files,
            "total_found": total_found,
            "message": if is_truncated { Some("Output truncated to 1000 files.") } else { None }
        }))
    }

    fn validate_path(&self, relative: &str, base: &Path) -> Result<PathBuf> {
        if relative.contains("..") || relative.starts_with("/") {
            return Err(anyhow!("Invalid path: {}", relative));
        }
        let full_path = base.join(relative);

        let canonical_base = base
            .canonicalize()
            .map_err(|e| anyhow!("Failed to canonicalize base path: {}", e))?;

        let canonical_full = match full_path.canonicalize() {
            Ok(p) => p,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = full_path.parent() {
                    let canonical_parent = parent
                        .canonicalize()
                        .map_err(|e| anyhow!("Failed to canonicalize parent path: {}", e))?;
                    if !canonical_parent.starts_with(&canonical_base) {
                        return Err(anyhow!("Path traversal detected in parent: {:?}", parent));
                    }
                    full_path
                } else {
                    return Err(anyhow!("No parent directory for path: {:?}", full_path));
                }
            }
            Err(e) => return Err(anyhow!("Failed to canonicalize path: {}", e)),
        };

        if !canonical_full.starts_with(&canonical_base) {
            return Err(anyhow!("Path traversal detected: {:?}", canonical_full));
        }

        Ok(canonical_full)
    }
}

fn glob_to_regex(glob: &str) -> Result<regex::Regex> {
    let mut regex_str = String::new();
    regex_str.push('^');
    for c in glob.chars() {
        match c {
            '*' => regex_str.push_str(".*"),
            '?' => regex_str.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\' => {
                regex_str.push('\\');
                regex_str.push(c);
            }
            _ => regex_str.push(c),
        }
    }
    regex_str.push('$');
    regex::Regex::new(&regex_str)
        .map_err(|e| anyhow::anyhow!("Invalid glob converted to regex: {}", e))
}

fn get_grep_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^([a-zA-Z0-9_./-]+)(:|-)([0-9]+)(:|-)(.*)$").unwrap())
}

fn format_git_grep_output(stdout: &str, revision: &str, active_files: &[String]) -> String {
    let prefix = format!("{}:", revision);
    let re = get_grep_regex();

    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current_file: Option<String> = None;

    for line in stdout.lines() {
        if line == "--" {
            if let Some(ref cur) = current_file
                && let Some(list) = grouped.get_mut(cur)
            {
                list.push("  --".to_string());
            }
            continue;
        }

        let stripped = if line.starts_with(&prefix) {
            &line[prefix.len()..]
        } else {
            line
        };

        if let Some(caps) = re.captures(stripped) {
            let path = &caps[1];
            let sep1 = &caps[2];
            let line_num = &caps[3];
            let sep2 = &caps[4];
            let content = &caps[5];

            if sep1 == sep2 {
                let formatted_line = format!("  {}{}{}", line_num, sep1, content);
                let path_str = path.to_string();
                current_file = Some(path_str.clone());
                grouped.entry(path_str).or_default().push(formatted_line);
            } else if let Some(ref cur) = current_file {
                grouped
                    .entry(cur.clone())
                    .or_default()
                    .push(stripped.to_string());
            }
        } else if let Some(ref cur) = current_file {
            grouped
                .entry(cur.clone())
                .or_default()
                .push(stripped.to_string());
        }
    }

    // Proximity Ranking
    let mut blocks: Vec<(String, Vec<String>)> = grouped.into_iter().collect();
    blocks.sort_by_key(|(path, _)| (get_priority_score(path, active_files), path.clone()));

    let mut result = String::new();
    for (path, lines) in blocks {
        result.push_str(&format!("[file: {}]\n", path));
        for l in lines {
            result.push_str(&l);
            result.push('\n');
        }
        result.push('\n');
    }

    result.trim_end().to_string()
}

fn get_priority_score(path: &str, active_files: &[String]) -> u32 {
    if active_files.is_empty() {
        return 4;
    }

    // 1. Exact Match
    if active_files.iter().any(|f| f == path) {
        return 1;
    }

    // 2. Directory Prefix Match
    let path_parent = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if !path_parent.is_empty() {
        for active_file in active_files {
            let active_parent = std::path::Path::new(active_file)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if !active_parent.is_empty() && path_parent == active_parent {
                return 2;
            }
        }
    }

    // 3. Include Directory Match
    if path.starts_with("include/") {
        return 3;
    }

    // 4. Default
    4
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_git_grep() -> Result<()> {
        let dir = tempdir()?;
        let repo_path = dir.path().to_path_buf();

        // Init git repo
        Command::new("git")
            .current_dir(&repo_path)
            .args(["init"])
            .output()
            .await?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        let file_path = repo_path.join("test.rs");
        let mut file = File::create(&file_path)?;
        writeln!(file, "fn main() {{")?;
        writeln!(file, "    println!(\"Hello World\");")?;
        writeln!(file, "    // TODO: fix this")?;
        writeln!(file, "}}")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial"])
            .output()
            .await?;

        let toolbox = ToolBox::new(repo_path.clone(), None);

        // Test basic search
        let args = json!({
            "revision": "HEAD",
            "pattern": "println",
            "path": "."
        });
        let result = toolbox.call("git_grep", args).await?;
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("test.rs"));
        assert!(content.contains("2:    println!(\"Hello World\");"));

        // Test context
        let args = json!({
            "revision": "HEAD",
            "pattern": "TODO",
            "context_lines": 1
        });
        let result = toolbox.call("git_grep", args).await?;
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("2-    println!(\"Hello World\");"));
        assert!(content.contains("3:    // TODO: fix this"));
        assert!(content.contains("4-}"));

        // Test pathspec exclusion
        let ignored_file_path = repo_path.join("ignored.rs");
        let mut ignored_file = File::create(&ignored_file_path)?;
        writeln!(ignored_file, "// TODO: do not find this")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Add ignored file"])
            .output()
            .await?;

        let args_excl = json!({
            "revision": "HEAD",
            "pattern": "TODO",
            "path": ". :!ignored.rs"
        });
        let result = toolbox.call("git_grep", args_excl).await?;
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("test.rs"));
        assert!(!content.contains("ignored.rs"));

        // Test count_only mode
        let args_count = json!({
            "revision": "HEAD",
            "pattern": "TODO",
            "count_only": true
        });
        let result = toolbox.call("git_grep", args_count).await?;
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("test.rs:1"));
        assert!(content.contains("ignored.rs:1"));

        // Test literal matching with parenthesis
        let args_literal = json!({
            "revision": "HEAD",
            "pattern": "println!(",
            "is_literal": true
        });
        let result = toolbox.call("git_grep", args_literal).await?;
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("println!(\"Hello World\");"));

        // Test that regex matching without escaping parenthesis fails (returns error object)
        let args_regex_fail = json!({
            "revision": "HEAD",
            "pattern": "println!(",
            "is_literal": false
        });
        let result = toolbox.call("git_grep", args_regex_fail).await?;
        assert!(result.get("error").is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_toolbox_caching() -> Result<()> {
        let dir = tempdir()?;
        let repo_path = dir.path().to_path_buf();

        Command::new("git")
            .current_dir(&repo_path)
            .args(["init"])
            .output()
            .await?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        let file_path = repo_path.join("cache_test.txt");
        std::fs::write(&file_path, "cached content")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial"])
            .output()
            .await?;

        let toolbox = ToolBox::new(repo_path.clone(), None);

        let args = json!({
            "revision": "HEAD",
            "files": [{"path": "cache_test.txt"}],
            "mode": "raw"
        });

        let result1 = toolbox.call("git_read_files", args.clone()).await?;
        let content1 = result1["results"][0]["content"].as_str().unwrap();
        assert_eq!(content1, "cached content");

        {
            let cache = toolbox.cache.read().unwrap();
            assert_eq!(cache.len(), 1);
        }

        std::fs::remove_file(&file_path)?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Delete file"])
            .output()
            .await?;

        let result2 = toolbox.call("git_read_files", args).await?;
        let content2 = result2["results"][0]["content"].as_str().unwrap();
        assert_eq!(content2, "cached content");

        Ok(())
    }

    #[tokio::test]
    async fn test_toolbox_caching_normalization() -> Result<()> {
        let dir = tempdir()?;
        let repo_path = dir.path().to_path_buf();

        Command::new("git")
            .current_dir(&repo_path)
            .args(["init"])
            .output()
            .await?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        let file_path = repo_path.join("grep_test.txt");
        std::fs::write(&file_path, "search target")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial"])
            .output()
            .await?;

        let toolbox = ToolBox::new(repo_path.clone(), None);

        // Call 1: omit optional parameter count_only and is_literal
        let args1 = json!({
            "revision": "HEAD",
            "pattern": "search target"
        });

        let _ = toolbox.call("git_grep", args1).await?;
        {
            let cache = toolbox.cache.read().unwrap();
            assert_eq!(cache.len(), 1);
        }

        // Remove the file physically so if it hits git, it will fail
        std::fs::remove_file(&file_path)?;

        // Call 2: include optional parameter explicitly set to default value
        let args2 = json!({
            "revision": "HEAD",
            "pattern": "search target",
            "count_only": false,
            "is_literal": false
        });

        let result = toolbox.call("git_grep", args2).await?;
        // It should successfully hit the cache and return the result (which contains search target)
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("search target"));

        Ok(())
    }

    #[tokio::test]
    async fn test_tool_normalization() -> Result<()> {
        let dir = tempdir()?;
        let prompt_path = dir.path().join("test.md");
        std::fs::write(&prompt_path, "prompt content")?;

        let toolbox = ToolBox::new(dir.path().to_path_buf(), Some(dir.path().to_path_buf()));

        let args = json!({
            "name": "test.md"
        });
        let result = toolbox.call("  Read_Prompt  ", args).await?;
        assert_eq!(result["content"].as_str().unwrap(), "prompt content");

        Ok(())
    }

    #[tokio::test]
    async fn test_git_tools() -> Result<()> {
        let dir = tempdir()?;
        let repo_path = dir.path().to_path_buf();
        let toolbox = ToolBox::new(repo_path.clone(), None);

        // Init repo
        Command::new("git")
            .current_dir(&repo_path)
            .args(["init"])
            .output()
            .await?;

        // Ensure we are on master
        let _ = Command::new("git")
            .current_dir(&repo_path)
            .args(["branch", "-m", "master"])
            .output()
            .await;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        // Create a file and commit
        let file_path = repo_path.join("test.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Hello")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial"])
            .output()
            .await?;

        // Test read_files
        let result = toolbox
            .call(
                "git_read_files",
                json!({
                    "revision": "HEAD",
                    "files": [{"path": "test.txt"}]
                }),
            )
            .await?;
        let content = result["results"][0]["content"].as_str().unwrap();
        assert!(content.contains("Hello"));

        // Test git_log
        let result = toolbox.call("git_log", json!({ "range": "HEAD" })).await?;
        let output = result["output"].as_str().unwrap();
        assert!(output.contains("Initial"));

        Ok(())
    }

    #[tokio::test]
    async fn test_validate_path_security() -> Result<()> {
        let dir = tempdir()?;
        let wt_path = dir.path().to_path_buf();
        let toolbox = ToolBox::new(wt_path.clone(), None);

        // Create a target file outside the worktree
        let outside_dir = tempdir()?;
        let outside_file = outside_dir.path().join("secret.txt");
        std::fs::write(&outside_file, "my secret key")?;

        // Create a target file inside the worktree
        let inside_file = wt_path.join("safe.txt");
        std::fs::write(&inside_file, "safe content")?;

        // 1. Test valid relative path inside
        let path = toolbox.validate_path("safe.txt", &wt_path);
        assert!(path.is_ok());
        assert_eq!(path.unwrap(), inside_file.canonicalize()?);

        // 2. Test path traversal attempt
        let path = toolbox.validate_path("../secret.txt", &wt_path);
        assert!(path.is_err());

        // 3. Test symlink pointing outside (should be blocked)
        #[cfg(unix)]
        {
            let symlink_outside = wt_path.join("link_outside");
            std::os::unix::fs::symlink(&outside_file, &symlink_outside)?;

            let path = toolbox.validate_path("link_outside", &wt_path);
            assert!(path.is_err(), "Symlink pointing outside should be blocked");
        }

        // 4. Test symlink pointing inside (should be allowed)
        #[cfg(unix)]
        {
            let symlink_inside = wt_path.join("link_inside");
            std::os::unix::fs::symlink(&inside_file, &symlink_inside)?;

            let path = toolbox.validate_path("link_inside", &wt_path);
            assert!(path.is_ok(), "Symlink pointing inside should be allowed");
            assert_eq!(path.unwrap(), inside_file.canonicalize()?);
        }

        // 5. Test non-existent file inside (should be allowed for creation)
        let path = toolbox.validate_path("new_file.txt", &wt_path);
        assert!(path.is_ok());
        assert_eq!(path.unwrap(), wt_path.join("new_file.txt"));

        // 6. Test non-existent file inside nested directory (should be allowed if parent is safe)
        let nested_dir = wt_path.join("nested");
        std::fs::create_dir(&nested_dir)?;
        let path = toolbox.validate_path("nested/new_file.txt", &wt_path);
        assert!(path.is_ok());
        assert_eq!(path.unwrap(), nested_dir.join("new_file.txt"));

        // 7. Test non-existent file in symlinked outside directory (should be blocked)
        #[cfg(unix)]
        {
            let symlink_dir_outside = wt_path.join("link_dir_outside");
            std::os::unix::fs::symlink(outside_dir.path(), &symlink_dir_outside)?;

            let path = toolbox.validate_path("link_dir_outside/new_file.txt", &wt_path);
            assert!(
                path.is_err(),
                "Creating file in symlinked outside directory should be blocked"
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_git_tools_security() -> Result<()> {
        let dir = tempdir()?;
        let repo_path = dir.path().to_path_buf();
        let toolbox = ToolBox::new(repo_path.clone(), None);

        // Init repo so git commands work
        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()
            .await?;

        // 1. Test git_diff with safe args
        let args = json!({
            "base_revision": "HEAD^",
            "target_revision": "HEAD"
        });
        let res = toolbox.call("git_diff", args).await;
        if let Err(e) = res {
            assert!(!e.to_string().contains("Invalid revision names"));
        }

        // Test git_diff with forbidden args
        let args = json!({
            "base_revision": "--output=malicious.txt",
            "target_revision": "HEAD"
        });
        let res = toolbox.call("git_diff", args).await;
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("Invalid revision names")
        );

        // 2. Test git_log with safe args
        let args = json!({
            "range": "HEAD"
        });
        let res = toolbox.call("git_log", args).await;
        if let Err(e) = res {
            assert!(!e.to_string().contains("Invalid range"));
        }

        // Test git_log with forbidden args
        let args = json!({
            "range": "--output=malicious.txt"
        });
        let res = toolbox.call("git_log", args).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("Invalid range"));

        // 3. Test git_show with safe object
        let args = json!({
            "object": "HEAD:README.md"
        });
        let res = toolbox.call("git_show", args).await;
        if let Err(e) = res {
            assert!(!e.to_string().contains("Invalid object name"));
        }

        // Test git_show with forbidden object (starts with -)
        let args = json!({
            "object": "--output=malicious.txt"
        });
        let res = toolbox.call("git_show", args).await;
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("Invalid object name: --output=malicious.txt")
        );

        Ok(())
    }

    #[test]
    fn test_format_git_grep_output() {
        let input = "\
HEAD:drivers/gpu/drm/tegra/output.c-167-\tif (ddc) {
HEAD:drivers/gpu/drm/tegra/output.c:168:\t\toutput->ddc = of_find_i2c_adapter_by_node(ddc);
HEAD:drivers/gpu/drm/tegra/output.c-169-\t\tif (!output->ddc) {
--
HEAD:drivers/i2c/muxes/i2c-mux-gpio.c:80:\tadapter = of_find_i2c_adapter_by_node(adapter_np);
--
HEAD:drivers/i2c/busses/i2c-10-bit.c:20:\tfoo();
";

        let expected = "\
[file: drivers/gpu/drm/tegra/output.c]
  167-	if (ddc) {
  168:\t\toutput->ddc = of_find_i2c_adapter_by_node(ddc);
  169-\t\tif (!output->ddc) {
  --

[file: drivers/i2c/busses/i2c-10-bit.c]
  20:\tfoo();

[file: drivers/i2c/muxes/i2c-mux-gpio.c]
  80:\tadapter = of_find_i2c_adapter_by_node(adapter_np);
  --";

        let formatted = super::format_git_grep_output(input, "HEAD", &[]);
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_git_grep_output_with_proximity() {
        let input = "\
HEAD:block/blk-core.c:10:foo
HEAD:drivers/soc/tegra/pmc.c:20:foo
HEAD:include/linux/i2c.h:30:foo
HEAD:io_uring/io_uring.c:40:foo
HEAD:io_uring/rw.c:50:foo
";
        let active_files = vec!["io_uring/rw.c".to_string()];

        let expected = "\
[file: io_uring/rw.c]
  50:foo

[file: io_uring/io_uring.c]
  40:foo

[file: include/linux/i2c.h]
  30:foo

[file: block/blk-core.c]
  10:foo

[file: drivers/soc/tegra/pmc.c]
  20:foo";

        let formatted = super::format_git_grep_output(input, "HEAD", &active_files);
        assert_eq!(formatted, expected);
    }
}
