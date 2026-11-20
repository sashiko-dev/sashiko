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

//! Case-bound, deterministic, time-bounded read-only tools for the series
//! design assessment pipeline.
//!
//! Baseline commit pinning (`baseline_sha`), posting-time cutoffs (`cutoff_ts`),
//! series commit exclusion (`excluded_commits`), and Stage 2 patch blindness
//! (`allow_series = false`) are enforced here in Rust rather than by prompt
//! instructions.

use crate::git_cmd;
use crate::prior_context::sanitize_untrusted_discussion_text;
use crate::toolbox::SashikoToolContext;
use crate::toolbox::command::capped_output;
use crate::toolbox::framework::{LlmTool, ToolRegistry};
use crate::toolbox::utils::validate_path;
use crate::utils::utf8_prefix;
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Shared immutable configuration for a single design assessment's tools.
pub struct DesignToolState {
    pub repo_path: PathBuf,
    pub baseline_sha: String,
    pub cutoff_ts: i64,
    /// Full canonical lowercase hex OIDs of commits in or descending from the
    /// series under review, excluded from `git_show_commit` and
    /// `maintainer_tree_log` so Stage 2 remains patch-blind and Stage 1/3 do
    /// not mistake the local branch under review for an upstream tree.
    pub excluded_commits: BTreeSet<String>,
    /// Root commit(s) of the reviewed series used for ancestry checks against
    /// arbitrary commit OIDs passed to `git_show_commit`.
    pub series_roots: Vec<String>,
    pub maintainer_notes_dir: Option<PathBuf>,
    pub series_text: Arc<String>,
}

impl DesignToolState {
    /// Validates `baseline_sha`, `excluded_commits`, and `series_roots` as
    /// strict hexadecimal object identifiers so they can never inject git
    /// revision operators or flags.
    pub fn validate(&self) -> Result<()> {
        validate_hex_oid(&self.baseline_sha)?;
        for oid in &self.excluded_commits {
            validate_hex_oid(oid)?;
        }
        for oid in &self.series_roots {
            validate_hex_oid(oid)?;
        }
        Ok(())
    }

    /// Collects the canonical lowercase hex OIDs of `series_shas` and any
    /// commits descending from `series_shas[0]` across `HEAD`, `refs/heads`,
    /// and `refs/remotes`.
    pub async fn collect_excluded_descendants(
        repo_path: &Path,
        series_shas: &[String],
    ) -> BTreeSet<String> {
        let mut excluded = BTreeSet::new();
        for sha in series_shas {
            if let Ok(clean) = validate_hex_oid(sha) {
                excluded.insert(clean.to_ascii_lowercase());
            }
        }
        let Some(first_raw) = series_shas.first() else {
            return excluded;
        };
        let Ok(first_sha) = validate_hex_oid(first_raw) else {
            return excluded;
        };

        let mut tip_oids = BTreeSet::new();
        let mut refs_cmd = git_cmd::in_dir_async(repo_path);
        refs_cmd.args([
            "for-each-ref",
            &format!("--contains={first_sha}"),
            "--format=%(objectname)",
            "refs/heads",
            "refs/remotes",
        ]);
        if let Ok(out) = capped_output(&mut refs_cmd).await
            && out.status.success()
        {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Ok(oid) = validate_hex_oid(line) {
                    tip_oids.insert(oid.to_ascii_lowercase());
                }
            }
        }

        let mut head_cmd = git_cmd::in_dir_async(repo_path);
        head_cmd.args(["rev-parse", "--verify", "HEAD^{commit}"]);
        if let Ok(out) = capped_output(&mut head_cmd).await
            && out.status.success()
            && let Ok(head_oid) = validate_hex_oid(&String::from_utf8_lossy(&out.stdout))
        {
            let mut anc = git_cmd::in_dir_async(repo_path);
            anc.args([
                "merge-base",
                "--is-ancestor",
                "--end-of-options",
                first_sha,
                head_oid,
            ]);
            if capped_output(&mut anc)
                .await
                .is_ok_and(|o| o.status.success())
            {
                tip_oids.insert(head_oid.to_ascii_lowercase());
            }
        }

        tip_oids.remove(&first_sha.to_ascii_lowercase());
        if !tip_oids.is_empty() {
            let mut rev_cmd = git_cmd::in_dir_async(repo_path);
            rev_cmd.args([
                "rev-list",
                "--ancestry-path",
                "-n5000",
                "--end-of-options",
                &format!("^{first_sha}"),
            ]);
            for tip in &tip_oids {
                rev_cmd.arg(tip);
            }
            rev_cmd.arg("--");
            if let Ok(out) = capped_output(&mut rev_cmd).await
                && out.status.success()
            {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Ok(oid) = validate_hex_oid(line) {
                        excluded.insert(oid.to_ascii_lowercase());
                    }
                }
            }
        }

        excluded
    }
}

fn validate_hex_oid(raw: &str) -> Result<&str> {
    let trimmed = raw.trim();
    if !(4..=64).contains(&trimmed.len()) || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid git commit SHA {raw:?}: expected 4..64 hex characters");
    }
    Ok(trimmed)
}

fn sanitize_rel_tree_path(raw: &str, allow_dot: bool) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." {
        if allow_dot {
            return Ok(".".to_string());
        }
        bail!("path is required");
    }
    if trimmed.starts_with('-') || trimmed.contains(['\0', '\n', '\r']) {
        bail!("invalid path {raw:?}");
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        bail!("absolute paths are not allowed: {raw:?}");
    }
    let mut parts = Vec::new();
    for comp in p.components() {
        match comp {
            Component::Normal(seg) => {
                let Some(s) = seg.to_str() else {
                    bail!("non-UTF-8 path: {raw:?}");
                };
                parts.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("path traversal is not allowed: {raw:?}");
            }
        }
    }
    if parts.is_empty() {
        if allow_dot {
            Ok(".".to_string())
        } else {
            bail!("path is required")
        }
    } else {
        Ok(parts.join("/"))
    }
}

fn format_utc_iso(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts.max(0), 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

fn floor_utf8_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut pos = idx;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Registers all time-bounded design assessment tools on `registry`.
/// When `allow_series` is `false` (Stage 2 blind alternatives), `read_series`
/// is omitted so the model has no mechanism to inspect the proposed patch.
pub fn register_design_tools(
    registry: &mut ToolRegistry<SashikoToolContext>,
    state: Arc<DesignToolState>,
    allow_series: bool,
) {
    registry.register(DesignGitGrepTool(Arc::clone(&state)));
    registry.register(DesignGitShowFileTool(Arc::clone(&state)));
    registry.register(DesignGitLogTool(Arc::clone(&state)));
    registry.register(DesignGitShowCommitTool(Arc::clone(&state)));
    registry.register(DesignMaintainerTreeLogTool(Arc::clone(&state)));
    registry.register(DesignListMaintainerNotesTool(Arc::clone(&state)));
    registry.register(DesignReadMaintainerNoteTool(Arc::clone(&state)));
    if allow_series {
        registry.register(DesignReadSeriesTool(state));
    }
}

struct DesignGitGrepTool(Arc<DesignToolState>);

#[async_trait]
impl LlmTool<SashikoToolContext> for DesignGitGrepTool {
    fn name(&self) -> &'static str {
        "git_grep"
    }

    fn description(&self) -> &'static str {
        "Search the baseline tree with an extended regex, optionally under a path."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Extended regular expression." },
                "path": { "type": "string", "description": "Optional directory or file path filter." }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: Value, _ctx: &SashikoToolContext) -> Result<Value> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if pattern.is_empty() || pattern.len() > 500 || pattern.contains('\0') {
            return Ok(json!({ "error": "pattern must be 1..500 non-NUL characters" }));
        }
        let raw_path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let clean_path = match sanitize_rel_tree_path(raw_path, true) {
            Ok(p) => p,
            Err(e) => return Ok(json!({ "error": e.to_string() })),
        };

        let mut cmd = git_cmd::in_dir_async(&self.0.repo_path);
        cmd.args([
            "grep",
            "-n",
            "-I",
            "-E",
            "-e",
            pattern,
            &self.0.baseline_sha,
            "--",
            &clean_path,
        ]);
        let out = capped_output(&mut cmd).await?;
        let text = String::from_utf8_lossy(&out.stdout);
        if text.trim().is_empty() {
            if out.status.code() == Some(1) || out.status.success() {
                return Ok(json!({ "result": "no matches" }));
            }
            let err = String::from_utf8_lossy(&out.stderr);
            return Ok(json!({ "error": utf8_prefix(err.trim(), 1_000) }));
        }

        let prefix = format!("{}:", self.0.baseline_sha);
        let all_lines: Vec<&str> = text.lines().collect();
        let mut rendered = Vec::with_capacity(all_lines.len().min(300));
        for line in all_lines.iter().take(300) {
            rendered.push(line.strip_prefix(&prefix).unwrap_or(line));
        }
        let mut joined = rendered.join("\n");
        if all_lines.len() > 300 {
            joined.push_str(&format!("\n...[{} more]", all_lines.len() - 300));
        }
        Ok(json!({ "result": utf8_prefix(&joined, 24_000) }))
    }
}

struct DesignGitShowFileTool(Arc<DesignToolState>);

#[async_trait]
impl LlmTool<SashikoToolContext> for DesignGitShowFileTool {
    fn name(&self) -> &'static str {
        "git_show_file"
    }

    fn description(&self) -> &'static str {
        "Show a file at the baseline commit with line numbers (default 400-line window)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer" },
                "end_line": { "type": "integer" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value, _ctx: &SashikoToolContext) -> Result<Value> {
        let raw_path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let clean_path = match sanitize_rel_tree_path(raw_path, false) {
            Ok(p) => p,
            Err(e) => return Ok(json!({ "error": e.to_string() })),
        };
        let start = args
            .get("start_line")
            .and_then(Value::as_i64)
            .unwrap_or(1)
            .max(1) as usize;
        let requested_end = args
            .get("end_line")
            .and_then(Value::as_i64)
            .map(|e| e.max(start as i64) as usize)
            .unwrap_or_else(|| start.saturating_add(399));
        let capped_end = requested_end.min(start.saturating_add(599));

        let spec = format!("{}:{clean_path}", self.0.baseline_sha);
        let mut cmd = git_cmd::in_dir_async(&self.0.repo_path);
        cmd.args(["show", "--end-of-options", &spec]);
        let out = capped_output(&mut cmd).await?;
        if !out.is_usable() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Ok(json!({ "error": utf8_prefix(err.trim(), 1_000) }));
        }
        let content = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = content.lines().collect();
        let end = capped_end.min(lines.len());
        let mut buf = String::new();
        if start <= end {
            for (idx, line) in lines[start - 1..end].iter().enumerate() {
                buf.push_str(&format!("{}: {}\n", start + idx, line));
            }
        }
        buf.push_str(&format!("[{} lines total]", lines.len()));
        Ok(json!({ "result": utf8_prefix(&buf, 30_000) }))
    }
}

struct DesignGitLogTool(Arc<DesignToolState>);

#[async_trait]
impl LlmTool<SashikoToolContext> for DesignGitLogTool {
    fn name(&self) -> &'static str {
        "git_log"
    }

    fn description(&self) -> &'static str {
        "One-line history of the baseline commit, optionally filtered by path and/or commit-message grep."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "grep": { "type": "string" },
                "max_count": { "type": "integer" }
            }
        })
    }

    async fn call(&self, args: Value, _ctx: &SashikoToolContext) -> Result<Value> {
        let raw_path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let clean_path = match sanitize_rel_tree_path(raw_path, true) {
            Ok(p) => p,
            Err(e) => return Ok(json!({ "error": e.to_string() })),
        };
        let max_count = args
            .get("max_count")
            .and_then(Value::as_i64)
            .unwrap_or(30)
            .clamp(1, 100);
        let grep = args
            .get("grep")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(g) = grep
            && (g.len() > 300 || g.contains(['\0', '\n', '\r']))
        {
            return Ok(json!({ "error": "invalid grep filter" }));
        }

        let mut cmd = git_cmd::in_dir_async(&self.0.repo_path);
        cmd.args(["log", "--oneline", &format!("-n{max_count}")]);
        if let Some(g) = grep {
            cmd.args(["-i", &format!("--grep={g}")]);
        }
        cmd.args(["--end-of-options", &self.0.baseline_sha, "--", &clean_path]);
        let out = capped_output(&mut cmd).await?;
        if !out.is_usable() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Ok(json!({ "error": utf8_prefix(err.trim(), 1_000) }));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let trimmed = text.trim();
        Ok(json!({
            "result": if trimmed.is_empty() {
                "no commits"
            } else {
                utf8_prefix(trimmed, 20_000)
            }
        }))
    }
}

struct DesignGitShowCommitTool(Arc<DesignToolState>);

#[async_trait]
impl LlmTool<SashikoToolContext> for DesignGitShowCommitTool {
    fn name(&self) -> &'static str {
        "git_show_commit"
    }

    fn description(&self) -> &'static str {
        "Show a commit (allowed only for baseline ancestors or other-branch commits dated before the cutoff)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sha": { "type": "string" },
                "path": { "type": "string" }
            },
            "required": ["sha"]
        })
    }

    async fn call(&self, args: Value, _ctx: &SashikoToolContext) -> Result<Value> {
        let raw_sha = args.get("sha").and_then(Value::as_str).unwrap_or("");
        let sha = match validate_hex_oid(raw_sha) {
            Ok(s) => s,
            Err(e) => return Ok(json!({ "error": e.to_string() })),
        };
        let raw_path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let clean_path = if raw_path.trim().is_empty() {
            None
        } else {
            match sanitize_rel_tree_path(raw_path, false) {
                Ok(p) => Some(p),
                Err(e) => return Ok(json!({ "error": e.to_string() })),
            }
        };

        // Resolve full canonical commit OID so abbreviated prefixes cannot
        // bypass `excluded_commits` or `series_roots` checks.
        let mut rev_cmd = git_cmd::in_dir_async(&self.0.repo_path);
        rev_cmd.args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{sha}^{{commit}}"),
        ]);
        let rev_out = capped_output(&mut rev_cmd).await?;
        if !rev_out.status.success() {
            let err = String::from_utf8_lossy(&rev_out.stderr);
            return Ok(json!({ "error": utf8_prefix(err.trim(), 1_000) }));
        }
        let resolved_sha = String::from_utf8_lossy(&rev_out.stdout)
            .trim()
            .to_ascii_lowercase();
        if validate_hex_oid(&resolved_sha).is_err() {
            return Ok(json!({ "error": "failed to resolve commit OID" }));
        }

        // 1. Check whether `resolved_sha` is an ancestor of `baseline_sha`.
        let mut anc_cmd = git_cmd::in_dir_async(&self.0.repo_path);
        anc_cmd.args([
            "merge-base",
            "--is-ancestor",
            "--end-of-options",
            &resolved_sha,
            &self.0.baseline_sha,
        ]);
        let is_ancestor = capped_output(&mut anc_cmd)
            .await
            .is_ok_and(|o| o.status.success());

        if !is_ancestor {
            // 2. Reject any commit in or descending from the series under review.
            if self.0.excluded_commits.contains(&resolved_sha) {
                return Ok(json!({
                    "result": "not allowed: commit is part of or descends from the series under review"
                }));
            }
            for root in &self.0.series_roots {
                let mut root_anc = git_cmd::in_dir_async(&self.0.repo_path);
                root_anc.args([
                    "merge-base",
                    "--is-ancestor",
                    "--end-of-options",
                    root,
                    &resolved_sha,
                ]);
                if capped_output(&mut root_anc)
                    .await
                    .is_ok_and(|o| o.status.success())
                {
                    return Ok(json!({
                        "result": "not allowed: commit is part of or descends from the series under review"
                    }));
                }
            }

            // 3. Otherwise require committer timestamp strictly `< cutoff_ts`.
            let mut ts_cmd = git_cmd::in_dir_async(&self.0.repo_path);
            ts_cmd.args([
                "show",
                "-s",
                "--format=%ct",
                "--end-of-options",
                &resolved_sha,
                "--",
            ]);
            let ts_out = capped_output(&mut ts_cmd).await?;
            let ct = String::from_utf8_lossy(&ts_out.stdout)
                .trim()
                .parse::<i64>()
                .ok();
            if !ts_out.status.success() || ct.is_none_or(|t| t >= self.0.cutoff_ts) {
                return Ok(json!({
                    "result": "not allowed: commit is not in the baseline history and not dated before the cutoff"
                }));
            }
        }

        let mut show_cmd = git_cmd::in_dir_async(&self.0.repo_path);
        show_cmd.args([
            "show",
            "--stat",
            "--patch",
            "--end-of-options",
            &resolved_sha,
            "--",
        ]);
        if let Some(ref p) = clean_path {
            show_cmd.arg(p);
        }
        let out = capped_output(&mut show_cmd).await?;
        let combined = if out.is_usable() {
            String::from_utf8_lossy(&out.stdout).into_owned()
        } else {
            String::from_utf8_lossy(&out.stderr).into_owned()
        };
        Ok(json!({ "result": utf8_prefix(&combined, 30_000) }))
    }
}

struct DesignMaintainerTreeLogTool(Arc<DesignToolState>);

#[async_trait]
impl LlmTool<SashikoToolContext> for DesignMaintainerTreeLogTool {
    fn name(&self) -> &'static str {
        "maintainer_tree_log"
    }

    fn description(&self) -> &'static str {
        "Commits touching a path in maintainer or remote branches dated within 150 days before the series."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "grep": { "type": "string" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value, _ctx: &SashikoToolContext) -> Result<Value> {
        let raw_path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let clean_path = match sanitize_rel_tree_path(raw_path, false) {
            Ok(p) => p,
            Err(e) => return Ok(json!({ "error": e.to_string() })),
        };
        let grep = args
            .get("grep")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(g) = grep
            && (g.len() > 300 || g.contains(['\0', '\n', '\r']))
        {
            return Ok(json!({ "error": "invalid grep filter" }));
        }

        // Enforce committer timestamp strictly `< cutoff_ts` and within 150 days.
        let before_iso = format_utc_iso(self.0.cutoff_ts.saturating_sub(1).max(0));
        let since_iso = format_utc_iso(self.0.cutoff_ts.saturating_sub(150 * 86_400).max(0));

        let mut rem_chk = git_cmd::in_dir_async(&self.0.repo_path);
        rem_chk.args(["for-each-ref", "--count=1", "refs/remotes"]);
        let has_remotes = capped_output(&mut rem_chk)
            .await
            .is_ok_and(|o| !o.stdout.is_empty());

        let mut cmd = git_cmd::in_dir_async(&self.0.repo_path);
        cmd.args([
            "log",
            "--no-merges",
            &format!("--before={before_iso}"),
            &format!("--since={since_iso}"),
            "-n150",
            if has_remotes {
                "--remotes"
            } else {
                "--branches"
            },
            "--format=%H\t%h %cs %s (%an)",
        ]);
        if let Some(g) = grep {
            cmd.args(["-i", &format!("--grep={g}")]);
        }
        cmd.args(["--end-of-options", "--", &clean_path]);
        let out = capped_output(&mut cmd).await?;
        if !out.is_usable() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Ok(json!({ "error": utf8_prefix(err.trim(), 1_000) }));
        }
        let text = String::from_utf8_lossy(&out.stdout);

        let mut filtered = Vec::new();
        for line in text.lines() {
            let Some((full_oid, display)) = line.split_once('\t') else {
                continue;
            };
            if self
                .0
                .excluded_commits
                .contains(&full_oid.trim().to_ascii_lowercase())
            {
                continue;
            }
            filtered.push(display);
            if filtered.len() >= 100 {
                break;
            }
        }
        let joined = filtered.join("\n");
        Ok(json!({
            "result": if joined.is_empty() {
                "no commits"
            } else {
                utf8_prefix(&joined, 24_000)
            }
        }))
    }
}

fn maintainer_paths_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"paths\s*=\s*\[([^\]]*)\]").expect("constant regex"))
}

struct DesignListMaintainerNotesTool(Arc<DesignToolState>);

#[async_trait]
impl LlmTool<SashikoToolContext> for DesignListMaintainerNotesTool {
    fn name(&self) -> &'static str {
        "list_maintainer_notes"
    }

    fn description(&self) -> &'static str {
        "List Sashiko maintainer-note files and the paths they apply to."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn call(&self, _args: Value, _ctx: &SashikoToolContext) -> Result<Value> {
        let Some(ref dir) = self.0.maintainer_notes_dir else {
            return Ok(json!({ "result": "" }));
        };
        let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
            return Ok(json!({ "result": "" }));
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = rd.next_entry().await {
            if let Some(name) = entry.file_name().to_str()
                && name.ends_with(".md")
            {
                names.push((name.to_string(), entry.path()));
            }
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = Vec::with_capacity(names.len());
        for (name, path) in names {
            let head = tokio::fs::read_to_string(&path)
                .await
                .map(|s| utf8_prefix(&s, 800).to_string())
                .unwrap_or_default();
            let paths_hint = maintainer_paths_re()
                .captures(&head)
                .and_then(|c| c.get(1))
                .map(|m| utf8_prefix(m.as_str().trim(), 200))
                .unwrap_or("");
            out.push(format!("{name}: {paths_hint}"));
        }
        Ok(json!({ "result": out.join("\n") }))
    }
}

struct DesignReadMaintainerNoteTool(Arc<DesignToolState>);

#[async_trait]
impl LlmTool<SashikoToolContext> for DesignReadMaintainerNoteTool {
    fn name(&self) -> &'static str {
        "read_maintainer_note"
    }

    fn description(&self) -> &'static str {
        "Read one maintainer-note file."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        })
    }

    async fn call(&self, args: Value, _ctx: &SashikoToolContext) -> Result<Value> {
        let Some(ref dir) = self.0.maintainer_notes_dir else {
            return Ok(json!({ "result": "no such note" }));
        };
        let raw_name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if raw_name.is_empty() || raw_name.contains('/') || raw_name.contains('\\') {
            return Ok(json!({ "result": "no such note" }));
        }
        let Ok(validated) = validate_path(raw_name, dir) else {
            return Ok(json!({ "result": "no such note" }));
        };
        match tokio::fs::read_to_string(&validated).await {
            Ok(content) => Ok(json!({ "result": utf8_prefix(&content, 20_000) })),
            Err(_) => Ok(json!({ "result": "no such note" })),
        }
    }
}

struct DesignReadSeriesTool(Arc<DesignToolState>);

#[async_trait]
impl LlmTool<SashikoToolContext> for DesignReadSeriesTool {
    fn name(&self) -> &'static str {
        "read_series"
    }

    fn description(&self) -> &'static str {
        "Read the patch series (commit messages, diffs, and any prior-version discussion)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "start": { "type": "integer", "description": "Byte offset into the series text (default 0)." },
                "length": { "type": "integer", "description": "Maximum bytes to read (default 40000)." }
            }
        })
    }

    async fn call(&self, args: Value, _ctx: &SashikoToolContext) -> Result<Value> {
        let s = self.0.series_text.as_str();
        let start_req = args.get("start").and_then(Value::as_u64).unwrap_or(0) as usize;
        let len_req = args
            .get("length")
            .and_then(Value::as_u64)
            .unwrap_or(40_000)
            .clamp(1, 80_000) as usize;
        let start = floor_utf8_boundary(s, start_req);
        let end = floor_utf8_boundary(s, start.saturating_add(len_req));
        let mut out = sanitize_untrusted_discussion_text(&s[start..end]);
        if end < s.len() {
            out.push_str(&format!("\n...[{} more bytes]", s.len() - end));
        }
        Ok(json!({ "result": out }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolbox::ToolBox;

    #[tokio::test]
    async fn blind_toolbox_forbids_series_and_excludes_reviewed_commits_and_descendants() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        let run_git = |args: &[&str], env: &[(&str, &str)]| {
            let mut cmd = git_cmd::in_dir(repo);
            cmd.args(args);
            for &(k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        run_git(&["init", "-b", "main"], &[]);
        run_git(&["config", "user.name", "Test"], &[]);
        run_git(&["config", "user.email", "test@example.com"], &[]);

        std::fs::write(repo.join("foo.c"), "int foo(void) { return 1; }\n").unwrap();
        run_git(&["add", "foo.c"], &[]);
        run_git(
            &["commit", "-m", "base commit"],
            &[
                ("GIT_AUTHOR_DATE", "@1700001000"),
                ("GIT_COMMITTER_DATE", "@1700001000"),
            ],
        );
        let base_sha = run_git(&["rev-parse", "HEAD"], &[]);

        // Reviewed series on `feature` branch: P1 (1700001400) and descendant P2 (1700001600),
        // both dated BEFORE `cutoff_ts` (1700002000) — must still be excluded!
        run_git(&["checkout", "-b", "feature"], &[]);
        std::fs::write(
            repo.join("foo.c"),
            "int secret_new_fn(void) { return 9; }\n",
        )
        .unwrap();
        run_git(&["add", "foo.c"], &[]);
        run_git(
            &["commit", "-m", "secret series patch 1"],
            &[
                ("GIT_AUTHOR_DATE", "@1700001400"),
                ("GIT_COMMITTER_DATE", "@1700001400"),
            ],
        );
        let p1_sha = run_git(&["rev-parse", "HEAD"], &[]);

        std::fs::write(
            repo.join("foo.c"),
            "int secret_new_fn(void) { return 10; }\n",
        )
        .unwrap();
        run_git(&["add", "foo.c"], &[]);
        run_git(
            &["commit", "-m", "followup on top of series"],
            &[
                ("GIT_AUTHOR_DATE", "@1700001600"),
                ("GIT_COMMITTER_DATE", "@1700001600"),
            ],
        );
        let p2_sha = run_git(&["rev-parse", "HEAD"], &[]);

        // Independent maintainer side branch off `base_sha`: before cutoff (1500) and after cutoff (2500).
        run_git(&["checkout", "-b", "maint-tree", &base_sha], &[]);
        std::fs::write(repo.join("foo.c"), "int foo(void) { return 2; }\n").unwrap();
        run_git(&["add", "foo.c"], &[]);
        run_git(
            &["commit", "-m", "maintainer rework before cutoff"],
            &[
                ("GIT_AUTHOR_DATE", "@1700001500"),
                ("GIT_COMMITTER_DATE", "@1700001500"),
            ],
        );
        let before_sha = run_git(&["rev-parse", "HEAD"], &[]);

        std::fs::write(repo.join("foo.c"), "int foo(void) { return 3; }\n").unwrap();
        run_git(&["add", "foo.c"], &[]);
        run_git(
            &["commit", "-m", "future fix after cutoff"],
            &[
                ("GIT_AUTHOR_DATE", "@1700002500"),
                ("GIT_COMMITTER_DATE", "@1700002500"),
            ],
        );
        let after_sha = run_git(&["rev-parse", "HEAD"], &[]);

        let excluded =
            DesignToolState::collect_excluded_descendants(repo, std::slice::from_ref(&p1_sha))
                .await;
        assert!(excluded.contains(&p1_sha.to_ascii_lowercase()));
        assert!(excluded.contains(&p2_sha.to_ascii_lowercase()));
        assert!(!excluded.contains(&before_sha.to_ascii_lowercase()));

        let state = Arc::new(DesignToolState {
            repo_path: repo.to_path_buf(),
            baseline_sha: base_sha,
            cutoff_ts: 1_700_002_000,
            excluded_commits: excluded,
            series_roots: vec![p1_sha.clone()],
            maintainer_notes_dir: None,
            series_text: Arc::new("SECRET PATCH DIFF — int secret_new_fn(void)".to_string()),
        });

        let (full_tb, blind_tb) = ToolBox::new_design_pair(Arc::clone(&state)).unwrap();

        // Populate cache via full_tb `read_series`, then verify `blind_tb` still rejects `read_series`.
        let series_res = full_tb.call("read_series", json!({})).await.unwrap();
        assert!(
            series_res["result"]
                .as_str()
                .unwrap()
                .contains("SECRET PATCH DIFF")
        );
        assert!(blind_tb.call("read_series", json!({})).await.is_err());
        assert!(
            !blind_tb
                .get_declarations_generic()
                .iter()
                .any(|d| d.name == "read_series")
        );

        // Baseline file read shows baseline content (`return 1`), NOT post-baseline (`return 3` or `secret_new_fn`).
        let file_res = blind_tb
            .call("git_show_file", json!({ "path": "foo.c" }))
            .await
            .unwrap();
        let file_str = file_res["result"].as_str().unwrap();
        assert!(file_str.contains("return 1"));
        assert!(!file_str.contains("return 3"));
        assert!(!file_str.contains("secret_new_fn"));

        // git_show_commit blocks P1 (even by short prefix!), blocks descendant P2, blocks after_sha,
        // and allows side-branch `before_sha`.
        for blocked in [&p1_sha[..8], p2_sha.as_str(), after_sha.as_str()] {
            let res = blind_tb
                .call("git_show_commit", json!({ "sha": blocked }))
                .await
                .unwrap();
            assert!(
                res["result"].as_str().unwrap().contains("not allowed"),
                "expected {blocked} to be blocked, got {res}"
            );
        }
        let ok_commit = full_tb
            .call("git_show_commit", json!({ "sha": before_sha }))
            .await
            .unwrap();
        assert!(
            ok_commit["result"]
                .as_str()
                .unwrap()
                .contains("maintainer rework before cutoff")
        );

        // maintainer_tree_log includes `before_sha` and excludes P1, P2, and `after_sha`.
        let mlog = blind_tb
            .call("maintainer_tree_log", json!({ "path": "foo.c" }))
            .await
            .unwrap();
        let mlog_str = mlog["result"].as_str().unwrap();
        assert!(mlog_str.contains("maintainer rework before cutoff"));
        assert!(!mlog_str.contains("secret series patch 1"));
        assert!(!mlog_str.contains("followup on top of series"));
        assert!(!mlog_str.contains("future fix after cutoff"));

        // Invalid --grep regex in git_log and maintainer_tree_log reports `error` instead of `no commits`.
        let bad_log = full_tb
            .call("git_log", json!({ "path": "foo.c", "grep": "[unclosed" }))
            .await
            .unwrap();
        assert!(
            bad_log.get("error").is_some(),
            "expected error, got {bad_log}"
        );
        let bad_mlog = full_tb
            .call(
                "maintainer_tree_log",
                json!({ "path": "foo.c", "grep": "[unclosed" }),
            )
            .await
            .unwrap();
        assert!(
            bad_mlog.get("error").is_some(),
            "expected error, got {bad_mlog}"
        );

        // git_show_file with start_line >= 400 and omitted end_line returns a 400-line window.
        let big_content: String = (1..=900).map(|i| format!("line {i}\n")).collect();
        std::fs::write(repo.join("big.c"), &big_content).unwrap();
        run_git(&["checkout", "-b", "big-base"], &[]);
        run_git(&["add", "big.c"], &[]);
        run_git(
            &["commit", "-m", "add big file"],
            &[
                ("GIT_AUTHOR_DATE", "@1700001100"),
                ("GIT_COMMITTER_DATE", "@1700001100"),
            ],
        );
        let big_sha = run_git(&["rev-parse", "HEAD"], &[]);
        let big_state = Arc::new(DesignToolState {
            repo_path: repo.to_path_buf(),
            baseline_sha: big_sha,
            cutoff_ts: 1_700_002_000,
            excluded_commits: BTreeSet::new(),
            series_roots: Vec::new(),
            maintainer_notes_dir: None,
            series_text: Arc::new(String::new()),
        });
        let (big_tb, _) = ToolBox::new_design_pair(big_state).unwrap();
        let win = big_tb
            .call(
                "git_show_file",
                json!({ "path": "big.c", "start_line": 450 }),
            )
            .await
            .unwrap();
        let win_str = win["result"].as_str().unwrap();
        assert!(win_str.contains("450: line 450\n"));
        assert!(win_str.contains("849: line 849\n"));
        assert!(!win_str.contains("850: line 850\n"));
    }
}
