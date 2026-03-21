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
use crate::ai::{
    AiTool,
    gemini::{FunctionDeclaration, Tool},
};
use anyhow::{Result, anyhow, ensure};
use grep::printer::StandardBuilder;
use grep::regex::RegexMatcher;
use grep::searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct ToolBox {
    worktree_path: PathBuf,
    prompts_path: Option<PathBuf>,
}

impl ToolBox {
    pub fn new(worktree_path: PathBuf, prompts_path: Option<PathBuf>) -> Self {
        Self {
            worktree_path,
            prompts_path,
        }
    }

    pub fn get_worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    /// Returns Gemini-specific tool declarations.
    /// TODO: Deprecate after migration.
    pub fn get_declarations(&self) -> Tool {
        let decls = self.get_declarations_generic();
        Tool {
            function_declarations: decls
                .into_iter()
                .map(|t| FunctionDeclaration {
                    name: t.name,
                    description: t.description,
                    parameters: t.parameters,
                })
                .collect(),
        }
    }

    /// Returns generic tool declarations.
    pub fn get_declarations_generic(&self) -> Vec<AiTool> {
        let mut decls = vec![
            AiTool {
                name: "read_files".to_string(),
                description: "Read the content of one or more files. In 'smart' mode, it collapses irrelevant code around the focus lines."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "description": "List of files to read.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string", "description": "Relative path to the file." },
                                    "start_line": { "type": "integer", "description": "1-based start line (optional). In smart mode, this is the start of the focus area." },
                                    "end_line": { "type": "integer", "description": "1-based end line (optional). In smart mode, this is the end of the focus area." }
                                },
                                "required": ["path"]
                            }
                        },
                        "mode": { "type": "string", "enum": ["raw", "smart"], "description": "Read mode. Defaults to 'raw'." }
                    },
                    "required": ["files"]
                }),
            },
            AiTool {
                name: "git_blame".to_string(),
                description: "Show what revision and author last modified each line of a file."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the file." },
                        "start_line": { "type": "integer", "description": "1-based start line (optional)." },
                        "end_line": { "type": "integer", "description": "1-based end line (optional)." }
                    },
                    "required": ["path"]
                }),
            },
            AiTool {
                name: "git_diff".to_string(),
                description: "Show changes between commits, commit and working tree, etc."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments for git diff (e.g., ['HEAD^', 'HEAD'])." }
                    },
                    "required": ["args"]
                }),
            },
            AiTool {
                name: "git_show".to_string(),
                description: "Show various types of objects (blobs, trees, tags and commits). Supports line filtering for blobs and diff suppression for commits."
                    .to_string(),
                parameters: json!({
                        "type": "object",
                        "properties": {
                            "object": { "type": "string", "description": "The object to show (e.g. 'HEAD:README.md' or 'HEAD')." },
                            "suppress_diff": { "type": "boolean", "description": "If true, suppresses the diff output for commits (shows only metadata). Useful for checking commit details cheaply." },
                            "start_line": { "type": "integer", "description": "1-based start line (optional). Useful for reading specific parts of a file (blob)." },
                            "end_line": { "type": "integer", "description": "1-based end line (optional)." }
                        },
                        "required": ["object"]
                }),
            },
            AiTool {
                name: "git_log".to_string(),
                description: "Show commit logs. IMPORTANT: When using expensive search flags like -S or -G, you MUST limit the search range using --since (e.g., '--since=1.year.ago') or specific commit ranges to avoid timeouts on large repositories.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments for git log (e.g., ['-n', '3', '--oneline']). Bounded to 100 commits by default unless overridden. For -S/-G searches, always include a time limit like '--since=1.year.ago'." }
                    },
                }),
            },
            AiTool {
                name: "git_status".to_string(),
                description: "Show the working tree status.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                }),
            },
            AiTool {
                name: "git_checkout".to_string(),
                description: "Switch branches or restore working tree files.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "target": { "type": "string", "description": "The branch or commit to checkout." }
                    },
                    "required": ["target"]
                }),
            },
            AiTool {
                name: "git_branch".to_string(),
                description: "List both remote-tracking branches and local branches.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                }),
            },
            AiTool {
                name: "git_tag".to_string(),
                description: "List tags.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                }),
            },
            AiTool {
                name: "list_dir".to_string(),
                description: "List files in a directory.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Directory path." }
                    },
                    "required": ["path"]
                }),
            },
            AiTool {
                name: "search_file_content".to_string(),
                description: "Search for a pattern in files using grep. Returns matching lines with context.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern to search for." },
                        "path": { "type": "string", "description": "Directory to search in (defaults to root)." },
                        "context_lines": { "type": "integer", "description": "Number of context lines to show (default 0)." }
                    },
                    "required": ["pattern"]
                }),
            },
            AiTool {
                name: "find_files".to_string(),
                description: "Find files matching a glob pattern (e.g., '*.rs', 'src/**/mod.rs').".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern to match." },
                        "path": { "type": "string", "description": "Directory to search in (defaults to root)." }
                    },
                    "required": ["pattern"]
                }),
            },
            AiTool {
                name: "TodoWrite".to_string(),
                description: "Add a new TODO item to the TODO.md file.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "The TODO item content." }
                    },
                    "required": ["content"]
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

    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let name_normalized = name.trim().to_lowercase();
        match name_normalized.as_str() {
            "read_files" => self.read_files(args).await,
            "git_blame" => self.git_blame(args).await,
            "git_diff" => self.git_diff(args).await,
            "git_show" => self.git_show(args).await,
            "git_log" => self.git_log(args).await,
            "git_status" => self.git_status(args).await,
            "git_checkout" => self.git_checkout(args).await,
            "git_branch" => self.git_branch(args).await,
            "git_tag" => self.git_tag(args).await,
            "list_dir" => self.list_dir(args).await,
            "search_file_content" => self.search_file_content(args).await,
            "find_files" => self.find_files(args).await,
            "todowrite" => self.todowrite(args).await,
            "read_prompt" => self.read_prompt(args).await,
            _ => Err(anyhow!("Unknown tool: {}", name)),
        }
    }

    fn truncate_output(&self, output: String) -> String {
        Truncator::truncate_diff(&output, 10_000)
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
        let files = args["files"]
            .as_array()
            .ok_or_else(|| anyhow!("Missing files"))?;
        let mode = args["mode"].as_str().unwrap_or("raw");

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
                .read_single_file(path_str, start_line, end_line, mode)
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
        path_str: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
        mode: &str,
    ) -> Result<Value> {
        let path = self.validate_path(path_str, &self.worktree_path)?;
        let content = fs::read_to_string(path).await?;

        if let (Some(s), Some(e)) = (start_line, end_line) {
            ensure!(s <= e, "Invalid range: start_line ({s}) > end_line ({e})");
        }

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let start_line = start_line.map(|s| s.clamp(1, total_lines));
        // No need to clamp start against end — the earlier validation already guarantees start <= end
        let end_line = end_line.map(|e| e.clamp(1, total_lines));

        if mode == "smart" {
            let focus = match (start_line, end_line) {
                (Some(s), Some(e)) => Some(s..e),
                (Some(s), None) => Some(s..s + 1),
                (None, Some(e)) => Some(1..e),
                (None, None) => None,
            };

            let truncated = Truncator::truncate_code(&content, focus, 20_000);

            return Ok(json!({
                "content": truncated,
                "total_lines": total_lines,
                "mode": "smart"
            }));
        }

        let (start, end) = match (start_line, end_line) {
            (Some(s), Some(e)) => (s.max(1) - 1, e.min(total_lines)),
            (Some(s), None) => (s.max(1) - 1, total_lines),
            (None, Some(e)) => (0, e.min(total_lines)),
            (None, None) => (0, total_lines),
        };

        let start = start.min(total_lines);
        let end = end.clamp(start, total_lines);

        if start >= total_lines {
            return Ok(json!({ "content": "", "lines_read": 0, "total_lines": total_lines }));
        }

        let slice = &lines[start..end];
        let result = slice.join("\n");
        let truncated = self.truncate_output(result);

        Ok(json!({
            "content": truncated,
            "lines_read": slice.len(),
            "total_lines": total_lines,
            "start_line": start + 1,
            "end_line": end
        }))
    }

    async fn git_blame(&self, args: Value) -> Result<Value> {
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

        cmd.arg("--").arg(path_str);

        let output = cmd.output().await?;
        if !output.status.success() {
            return Err(anyhow!(
                "git blame failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(json!({ "content": self.truncate_output(content) }))
    }

    async fn git_diff(&self, args: Value) -> Result<Value> {
        let diff_args = args["args"]
            .as_array()
            .ok_or_else(|| anyhow!("Missing args"))?;
        let diff_args_str: Vec<&str> = diff_args.iter().filter_map(|v| v.as_str()).collect();

        let output = Command::new("git")
            .current_dir(&self.worktree_path)
            .arg("diff")
            .arg("--diff-algorithm=histogram")
            .args(&diff_args_str)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut error_msg = format!("git diff failed: {}", stderr);

            if stderr.contains("unknown revision") || stderr.contains("ambiguous argument") {
                error_msg.push_str("\nHint: The repository might be a shallow clone (depth=1). You cannot access history beyond HEAD. Try using 'HEAD' or diffing against specific files without revision ranges.");
            }

            return Err(anyhow!(error_msg));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(json!({ "content": Truncator::truncate_diff(&content, 10_000) }))
    }

    async fn git_log(&self, args: Value) -> Result<Value> {
        let log_args_str: Vec<&str> = args["args"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.worktree_path)
            .arg("log")
            .args(["-n", "100"])
            .args(&log_args_str)
            .kill_on_drop(true);

        let output_result =
            tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output()).await;

        let output = match output_result {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                return Ok(
                    json!({ "error": "git log command timed out after 120 seconds. Please avoid using extremely slow search flags like -S or -G on large repositories." }),
                );
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok(json!({ "error": format!("git log failed: {}", stderr) }));
        }

        Ok(
            json!({ "output": self.truncate_output(String::from_utf8_lossy(&output.stdout).to_string()) }),
        )
    }

    async fn git_show(&self, args: Value) -> Result<Value> {
        let object = args["object"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing object"))?;
        let suppress_diff = args["suppress_diff"].as_bool().unwrap_or(false);
        let start_line = args["start_line"].as_u64().map(|v| v as usize);
        let end_line = args["end_line"].as_u64().map(|v| v as usize);

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.worktree_path).arg("show");

        if suppress_diff {
            cmd.arg("--no-patch");
        }

        cmd.arg(object);

        let output = cmd.output().await?;

        if !output.status.success() {
            return Err(anyhow!(
                "git show failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();

        if start_line.is_some() || end_line.is_some() {
            let lines: Vec<&str> = content.lines().collect();
            let total_lines = lines.len();
            let (start, end) = match (start_line, end_line) {
                (Some(s), Some(e)) => (s.max(1) - 1, e.min(total_lines)),
                (Some(s), None) => (s.max(1) - 1, total_lines),
                (None, Some(e)) => (0, e.min(total_lines)),
                (None, None) => (0, total_lines),
            };

            let start = start.min(total_lines);
            let end = end.clamp(start, total_lines);

            if start >= total_lines {
                return Ok(json!({ "content": "", "lines_read": 0, "total_lines": total_lines }));
            }

            let slice = &lines[start..end];
            let result = slice.join("\n");
            return Ok(json!({
                "content": self.truncate_output(result),
                "total_lines": total_lines,
                "start_line": start + 1,
                "end_line": end
            }));
        }

        Ok(json!({ "content": self.truncate_output(content) }))
    }

    async fn git_status(&self, _args: Value) -> Result<Value> {
        let content = crate::git_ops::git_status(&self.worktree_path).await?;
        Ok(json!({ "content": content }))
    }

    async fn git_checkout(&self, args: Value) -> Result<Value> {
        let target = args["target"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing target"))?;
        crate::git_ops::git_checkout(&self.worktree_path, target).await?;
        Ok(json!({ "status": "success", "message": format!("Checked out {}", target) }))
    }

    async fn git_branch(&self, _args: Value) -> Result<Value> {
        let content = crate::git_ops::git_branch(&self.worktree_path).await?;
        Ok(json!({ "content": content }))
    }

    async fn git_tag(&self, _args: Value) -> Result<Value> {
        let content = crate::git_ops::git_tag(&self.worktree_path).await?;
        Ok(json!({ "content": content }))
    }

    async fn list_dir(&self, args: Value) -> Result<Value> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing path"))?;
        let path = self.validate_path(path_str, &self.worktree_path)?;

        let mut entries = Vec::new();
        let mut read_dir = fs::read_dir(path).await?;

        while let Some(entry) = read_dir.next_entry().await? {
            let ty = if entry.file_type().await?.is_dir() {
                "dir"
            } else {
                "file"
            };
            entries.push(json!({ "name": entry.file_name().to_string_lossy(), "type": ty }));
        }

        if entries.len() > 1000 {
            entries.truncate(1000);
        }

        Ok(json!({ "entries": entries }))
    }

    fn validate_path(&self, relative: &str, base: &Path) -> Result<PathBuf> {
        if relative.contains("..") || relative.starts_with("/") {
            return Err(anyhow!("Invalid path: {}", relative));
        }
        let full_path = base.join(relative);
        if !full_path.starts_with(base) {
            return Err(anyhow!("Path traversal detected: {:?}", full_path));
        }
        Ok(full_path)
    }

    async fn search_file_content(&self, args: Value) -> Result<Value> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing pattern"))?
            .to_string();
        let path_str = args["path"].as_str().unwrap_or(".").to_string();
        let context_lines = args["context_lines"].as_u64().unwrap_or(0) as usize;

        let search_path = self.validate_path(&path_str, &self.worktree_path)?;
        let worktree_root = self.worktree_path.clone();

        // Perform blocking search operation in a separate thread
        let content = tokio::task::spawn_blocking(move || {
            let matcher =
                RegexMatcher::new(&pattern).map_err(|e| anyhow!("Invalid regex: {}", e))?;
            let mut searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .line_number(true)
                .before_context(context_lines)
                .after_context(context_lines)
                .build();

            // We use an Arc<Mutex<Vec<u8>>> to capture output because WalkBuilder is multithreaded (by default)
            // or if we use synchronous, we can just use a simple Vec if we don't thread.
            // But WalkBuilder::new() returns an iterator which is driven on the current thread.
            // So we can just use a simple buffer.
            let mut output_buffer = Vec::new();

            // Standard printer writes to the buffer.
            // Create a new printer for each file to write to the shared buffer.
            // Actually, `printer` takes a `W`.

            let walker = WalkBuilder::new(&search_path)
                .hidden(false) // Search hidden files (default ignore handles .git).
                .ignore(true) // Respect .ignore
                .git_ignore(true) // Respect .gitignore
                .build();

            for result in walker {
                match result {
                    Ok(entry) => {
                        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                            continue;
                        }

                        // We use a fresh buffer for this file to avoid borrowing issues if we reused one
                        // strictly speaking, but StandardBuilder::build_no_color takes W.
                        // We can just pass a mutable reference to our main buffer.
                        let mut printer = StandardBuilder::new().build_no_color(&mut output_buffer);

                        let path_to_print = entry
                            .path()
                            .strip_prefix(&worktree_root)
                            .unwrap_or(entry.path());

                        let _ = searcher.search_path(
                            &matcher,
                            entry.path(),
                            printer.sink_with_path(&matcher, path_to_print),
                        );
                    }
                    Err(_) => continue, // Ignore permission errors etc, similar to grep -r 2>/dev/null
                }
            }

            String::from_utf8(output_buffer)
                .map_err(|e| anyhow!("Search output was not valid UTF-8: {}", e))
        })
        .await??;

        if content.is_empty() {
            return Ok(json!({ "matches": [], "message": "No matches found." }));
        }

        Ok(json!({ "content": self.truncate_output(content) }))
    }

    async fn find_files(&self, args: Value) -> Result<Value> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing pattern"))?;
        let path_str = args["path"].as_str().unwrap_or(".");

        let path = self.validate_path(path_str, &self.worktree_path)?;

        let output = Command::new("find")
            .current_dir(&self.worktree_path)
            .arg(path)
            .arg("-name")
            .arg(pattern)
            .arg("-not")
            .arg("-path")
            .arg("*/.*")
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "find failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        let files: Vec<&str> = content.lines().collect();

        if files.len() > 1000 {
            let truncated = files[..1000].join("\n");
            return Ok(json!({
                 "files": truncated,
                 "total_found": files.len(),
                 "message": "Output truncated to 1000 files."
            }));
        }

        Ok(json!({ "files": content }))
    }

    async fn todowrite(&self, args: Value) -> Result<Value> {
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing content"))?;

        // We use validate_path to ensure we are staying within the worktree,
        // although we hardcode the filename.
        let path = self.validate_path("TODO.md", &self.worktree_path)?;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;

        file.write_all(format!("- [ ] {}\n", content).as_bytes())
            .await?;
        file.flush().await?;

        Ok(json!({ "status": "success", "message": "TODO added." }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_search_file_content() -> Result<()> {
        let dir = tempdir()?;
        let file_path = dir.path().join("test.rs");
        let mut file = File::create(&file_path)?;
        writeln!(file, "fn main() {{")?;
        writeln!(file, "    println!(\"Hello World\");")?;
        writeln!(file, "    // TODO: fix this")?;
        writeln!(file, "}}")?;

        let toolbox = ToolBox::new(dir.path().to_path_buf(), None);

        // Test basic search
        let args = json!({
            "pattern": "println",
            "path": "."
        });
        let result = toolbox.call("search_file_content", args).await?;
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("test.rs"));
        assert!(content.contains("2:    println!(\"Hello World\");"));

        // Test context
        let args = json!({
            "pattern": "TODO",
            "context_lines": 1
        });
        let result = toolbox.call("search_file_content", args).await?;
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("2-    println!(\"Hello World\");"));
        assert!(content.contains("3:    // TODO: fix this"));
        assert!(content.contains("4-}"));

        Ok(())
    }

    #[tokio::test]
    async fn test_todowrite() -> Result<()> {
        let dir = tempdir()?;
        let toolbox = ToolBox::new(dir.path().to_path_buf(), None);

        let args = json!({
            "content": "Implement more features"
        });
        toolbox.call("todowrite", args).await?;

        let todo_path = dir.path().join("TODO.md");
        let content = std::fs::read_to_string(todo_path)?;
        assert!(content.contains("- [ ] Implement more features"));

        // Append another one
        let args2 = json!({
            "content": "Fix bugs"
        });
        toolbox.call("todowrite", args2).await?;
        let content = std::fs::read_to_string(dir.path().join("TODO.md"))?;
        assert!(content.contains("- [ ] Implement more features"));
        assert!(content.contains("- [ ] Fix bugs"));

        Ok(())
    }

    #[tokio::test]
    async fn test_tool_normalization() -> Result<()> {
        let dir = tempdir()?;
        let toolbox = ToolBox::new(dir.path().to_path_buf(), None);

        // Test with whitespace and mixed case
        let args = json!({
            "content": "Normalization test"
        });
        toolbox.call("  TodoWrite  ", args).await?;

        let todo_path = dir.path().join("TODO.md");
        let content = std::fs::read_to_string(todo_path)?;
        assert!(content.contains("- [ ] Normalization test"));

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

        // Test git_status
        let result = toolbox.call("git_status", json!({})).await?;
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("nothing to commit"));

        // Test git_branch
        let result = toolbox.call("git_branch", json!({})).await?;
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("master"));

        // Create branch
        Command::new("git")
            .current_dir(&repo_path)
            .args(["branch", "new-feature"])
            .output()
            .await?;

        // Test git_checkout
        toolbox
            .call("git_checkout", json!({ "target": "new-feature" }))
            .await?;

        let result = toolbox.call("git_branch", json!({})).await?;
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("* new-feature"));

        // Create tag
        Command::new("git")
            .current_dir(&repo_path)
            .args(["tag", "v1.0"])
            .output()
            .await?;

        // Test git_tag
        let result = toolbox.call("git_tag", json!({})).await?;
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("v1.0"));

        Ok(())
    }
}
