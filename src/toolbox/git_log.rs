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
use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::Path;

pub struct GitLogTool;

struct LineRangeSpec {
    path: String,
    flag: String,
}

fn parse_paths(args: &Value, worktree: &Path) -> Result<Vec<String>> {
    let Some(paths_val) = args.get("paths") else {
        return Ok(Vec::new());
    };
    if paths_val.is_null() {
        return Ok(Vec::new());
    }
    let arr = paths_val
        .as_array()
        .ok_or_else(|| anyhow!("Invalid paths parameter: expected array of strings"))?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let p = entry
            .as_str()
            .ok_or_else(|| anyhow!("Invalid path entry: expected string"))?;
        if p.is_empty() || p == "." {
            continue;
        }
        if p.starts_with('-') {
            return Err(anyhow!("Invalid path parameter: {}", p));
        }
        validate_path(p, worktree)?;
        out.push(p.to_string());
    }
    Ok(out)
}

fn parse_line_range(args: &Value, worktree: &Path) -> Result<Option<LineRangeSpec>> {
    let Some(lr) = args.get("line_range") else {
        return Ok(None);
    };
    if lr.is_null() {
        return Ok(None);
    }
    let obj = lr
        .as_object()
        .ok_or_else(|| anyhow!("Invalid line_range: expected object"))?;
    let path = obj
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("line_range requires a non-empty path"))?;
    if path.starts_with('-') {
        return Err(anyhow!("Invalid line_range path: {}", path));
    }
    validate_path(path, worktree)?;

    let start = obj.get("start_line").and_then(|v| v.as_u64());
    let end = obj.get("end_line").and_then(|v| v.as_u64());
    let symbol = obj
        .get("symbol")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    let flag = match (start, end, symbol) {
        (Some(s), Some(e), None) => {
            ensure!(
                s >= 1 && s <= e,
                "Invalid line_range: start_line ({s}) must be >= 1 and <= end_line ({e})"
            );
            format!("-L{s},{e}:{path}")
        }
        (None, None, Some(sym)) => {
            ensure!(
                sym.len() <= 256 && sym.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "Invalid line_range symbol '{sym}': only ASCII identifier characters [A-Za-z0-9_] are allowed"
            );
            format!("-L:{sym}:{path}")
        }
        (Some(_), Some(_), Some(_)) | (Some(_), None, Some(_)) | (None, Some(_), Some(_)) => {
            return Err(anyhow!(
                "line_range cannot specify both start_line/end_line and symbol"
            ));
        }
        _ => {
            return Err(anyhow!(
                "line_range requires either both start_line and end_line, or symbol"
            ));
        }
    };

    Ok(Some(LineRangeSpec {
        path: path.to_string(),
        flag,
    }))
}

fn parse_pickaxe_flag(args: &Value) -> Result<Option<String>> {
    let pickaxe_regex = args["pickaxe_regex"].as_bool().unwrap_or(false);
    let Some(raw) = args.get("pickaxe").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    ensure!(
        !raw.contains('\0') && raw.len() <= 1024,
        "Invalid pickaxe string"
    );
    let prefix = if pickaxe_regex { "-G" } else { "-S" };
    // Attached flag form prevents a leading dash in raw from being parsed as a git option.
    Ok(Some(format!("{prefix}{raw}")))
}

#[async_trait]
impl LlmTool<SashikoToolContext> for GitLogTool {
    fn name(&self) -> &'static str {
        "git_log"
    }

    fn description(&self) -> &'static str {
        "Show commit logs in a revision range, optionally scoped by file paths, line/function evolution (-L), or pickaxe string/regex (-S/-G)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "range": { "type": "string", "description": "The commit range or reference to view logs for (e.g., 'baseline..HEAD' or 'HEAD')." },
                "limit": { "type": "integer", "description": "Limit the number of commits returned (defaults to 10, max 100)." },
                "paths": {
                    "type": "array",
                    "description": "Optional relative file or directory paths to filter commits (e.g., ['mm/vmscan.c']).",
                    "items": { "type": "string" }
                },
                "line_range": {
                    "type": "object",
                    "description": "Trace the evolution of a line range or top-level C function in a single file (git log -L). Cannot be combined with other paths.",
                    "properties": {
                        "path": { "type": "string", "description": "Relative file path to trace." },
                        "start_line": { "type": "integer", "description": "1-based start line (use with end_line)." },
                        "end_line": { "type": "integer", "description": "1-based end line (use with start_line)." },
                        "symbol": { "type": "string", "description": "Top-level C function identifier to trace via -L:<symbol>:<path> (prefer start_line/end_line for indented methods or macros)." }
                    },
                    "required": ["path"]
                },
                "pickaxe": { "type": "string", "description": "Filter commits that add/remove occurrences of this string (-S) or match this diff regex (-G when pickaxe_regex is true)." },
                "pickaxe_regex": { "type": "boolean", "description": "If true, treat pickaxe as a diff regex (-G) instead of a literal count search (-S). Default: false." },
                "oneline": { "type": "boolean", "description": "If true, format commit headers concisely with --oneline. Default: false." }
            },
            "required": ["range"]
        })
    }

    fn normalize_args(&self, args: &Value) -> Value {
        let mut normalized = args.clone();
        if let Some(obj) = normalized.as_object_mut() {
            if obj.get("limit").is_none_or(Value::is_null) {
                obj.insert("limit".to_string(), json!(10));
            }
            let empty_paths = obj
                .get("paths")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty);
            if !obj.contains_key("paths") || empty_paths {
                obj.insert("paths".to_string(), Value::Null);
            }
            if let Some(lr_obj) = obj.get_mut("line_range").and_then(Value::as_object_mut) {
                for key in ["start_line", "end_line", "symbol"] {
                    if !lr_obj.contains_key(key) {
                        lr_obj.insert(key.to_string(), Value::Null);
                    }
                }
            } else if !obj.contains_key("line_range") {
                obj.insert("line_range".to_string(), Value::Null);
            }
            let empty_pickaxe = obj
                .get("pickaxe")
                .and_then(Value::as_str)
                .is_some_and(str::is_empty);
            if !obj.contains_key("pickaxe") || empty_pickaxe {
                obj.insert("pickaxe".to_string(), Value::Null);
            }
            if obj.get("pickaxe_regex").is_none_or(Value::is_null) {
                obj.insert("pickaxe_regex".to_string(), json!(false));
            }
            if obj.get("oneline").is_none_or(Value::is_null) {
                obj.insert("oneline".to_string(), json!(false));
            }
        }
        normalized
    }

    async fn call(&self, args: Value, context: &SashikoToolContext) -> Result<Value> {
        let range_raw = args["range"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing range"))?;
        let range_virt = context.virtualize_ref(range_raw);
        let range = range_virt.as_str();
        if range.is_empty() || range.starts_with('-') {
            return Err(anyhow!("Invalid range"));
        }

        let limit = args["limit"].as_u64().unwrap_or(10).min(100) as usize;
        let oneline = args["oneline"].as_bool().unwrap_or(false);
        let mut paths = parse_paths(&args, &context.worktree_path)?;
        let line_range = parse_line_range(&args, &context.worktree_path)?;
        let pickaxe_flag = parse_pickaxe_flag(&args)?;

        if let Some(ref lr) = line_range {
            if paths.iter().all(|p| p == &lr.path) {
                // Git forbids combining -L with pathspecs; omit redundant matching pathspec.
                paths.clear();
            } else {
                return Err(anyhow!(
                    "line_range already scopes git_log to '{}'; omit paths or pass only that path",
                    lr.path
                ));
            }
        }

        let limit_str = limit.to_string();
        let mut cmd = crate::git_cmd::in_dir_async(&context.worktree_path);
        cmd.args(["log", "-n", &limit_str]);
        if oneline {
            cmd.arg("--oneline");
        }
        if let Some(ref lr) = line_range {
            cmd.arg(&lr.flag);
        }
        if let Some(ref pf) = pickaxe_flag {
            cmd.arg(pf);
        }
        cmd.arg(range);
        if !paths.is_empty() {
            cmd.arg("--").args(&paths);
        }
        cmd.kill_on_drop(true);

        let output = capped_output(&mut cmd).await?;
        if !output.is_usable() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Ok(Value::String(format!("Error: git log failed: {}", stderr)));
        }

        let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if raw_stdout.trim().is_empty() {
            return Ok(Value::String("No commits found.".to_string()));
        }

        let res = Truncator::truncate_sequential(&raw_stdout, 10_000);
        let mut out = res.content;
        if res.truncated {
            out.push_str(
                "\n[Truncated: log output exceeded token budget. Use a smaller commit range or set a lower limit.]",
            );
        }

        Ok(Value::String(out))
    }
}
