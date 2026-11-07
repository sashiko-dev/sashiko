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

//! Agent-ready Markdown rendering for tracked pre-existing bugs.
//!
//! Renders a single bug's verified metadata, defect report, readable community
//! activity, and project-appropriate commit trailer guidance (`Fixes:`,
//! `Reported-by:`, `Link:`) in a self-contained Markdown document suitable for
//! handing directly to an AI coding agent tasked with fixing the defect.

use axum::http::HeaderMap;
use chrono::{TimeZone, Utc};
use serde_json::Value;

use crate::db::{Bug, BugLifecycleStatus, BugPipelineState, Severity};
use crate::email_policy::extract_bare_email;
use crate::project::ProjectId;
use crate::settings::Settings;

/// Summary of the canonical target bug when rendering a duplicate bug whose
/// counterpart is within the caller's read authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalDuplicate {
    pub bugid: String,
    pub problem: String,
}

/// Structured state of a bug's introducing-commit `Fixes:` trailer derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixesSuggestion {
    /// Both a commit SHA and its subject are known.
    Exact { sha12: String, line: String },
    /// Only the introducing commit SHA is known; the subject must be looked up
    /// in git history.
    ShaOnly { sha12: String },
    /// No introducing commit was recorded on the bug.
    Unknown,
}

/// Read-authorized inputs used to render a bug's Markdown prompt.
pub struct BugMarkdownContext<'a> {
    pub bug: &'a Bug,
    pub base_url: &'a str,
    pub reported_by: &'a str,
    pub project: ProjectId,
    pub canonical_duplicate: Option<&'a CanonicalDuplicate>,
    pub activity: &'a [Value],
}

/// Collapses CR/LF line breaks into single spaces so untrusted titles, commit
/// subjects, or reporter strings cannot inject extra lines or git trailers.
fn single_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.trim().chars() {
        if ch == '\r' || ch == '\n' {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out.trim().to_string()
}

/// Percent-encodes bytes outside the RFC 3986 unreserved character set so a
/// `bugid` embeds safely in both path segments and hash-fragment routes.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Appends `body` inside a Markdown code fence strictly longer than any
/// contiguous backtick sequence in `body`, preventing untrusted text from
/// closing the fence prematurely.
fn push_fenced_block(out: &mut String, info: &str, body: &str) {
    let max_ticks = longest_backtick_run(body);
    let fence_len = max_ticks.saturating_add(1).max(4);
    let fence: String = std::iter::repeat_n('`', fence_len).collect();
    out.push_str(&fence);
    out.push_str(info);
    out.push('\n');
    let trimmed = body.trim_end_matches(['\r', '\n']);
    out.push_str(trimmed);
    out.push('\n');
    out.push_str(&fence);
    out.push('\n');
}

fn longest_backtick_run(s: &str) -> usize {
    let mut best = 0usize;
    let mut cur = 0usize;
    for b in s.bytes() {
        if b == b'`' {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// Validates that an HTTP authority (`host` or `host:port`) contains only safe
/// hostname / IPv4 / bracketed-IPv6 characters with no path, userinfo, or
/// whitespace.
fn is_valid_http_authority(authority: &str) -> bool {
    let authority = authority.trim();
    if authority.is_empty() || authority.len() > 255 {
        return false;
    }
    let (is_ipv6, host, port) = if let Some(inside) = authority.strip_prefix('[') {
        let Some((ipv6, rest)) = inside.split_once(']') else {
            return false;
        };
        if ipv6.is_empty() || !ipv6.chars().all(|c| c.is_ascii_hexdigit() || c == ':') {
            return false;
        }
        (true, ipv6, rest)
    } else if let Some((h, _)) = authority.split_once(':') {
        (false, h, &authority[h.len()..])
    } else {
        (false, authority, "")
    };
    if host.is_empty()
        || (!is_ipv6
            && (host.starts_with(['.', '-'])
                || host.ends_with('-')
                || host.contains("..")
                || !host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))))
    {
        return false;
    }
    if let Some(port_digits) = port.strip_prefix(':') {
        if port_digits.is_empty() || port_digits.parse::<u16>().is_err() {
            return false;
        }
    } else if !port.is_empty() {
        return false;
    }
    true
}

fn domain_base_url(domain: &str) -> Option<String> {
    let trimmed = domain.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if let Some((scheme, rest)) = trimmed.split_once("://") {
        if matches!(scheme, "http" | "https") && is_valid_http_authority(rest) {
            return Some(format!("{scheme}://{rest}"));
        }
        return None;
    }
    is_valid_http_authority(trimmed).then(|| format!("https://{trimmed}"))
}

fn header_base_url(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(axum::http::header::HOST)?.to_str().ok()?.trim();
    if !is_valid_http_authority(host) {
        return None;
    }
    let forwarded_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"));
    let host_lower = host.to_ascii_lowercase();
    let is_local_or_custom_port = host_lower == "localhost"
        || host_lower.starts_with("localhost:")
        || host_lower == "127.0.0.1"
        || host_lower.starts_with("127.0.0.1:")
        || host_lower == "[::1]"
        || host_lower.starts_with("[::1]:")
        || (host_lower.contains(':')
            && !host_lower.ends_with(":443")
            && !host_lower.ends_with(']'));
    let scheme = if forwarded_https || !is_local_or_custom_port {
        "https"
    } else {
        "http"
    };
    Some(format!("{scheme}://{host}"))
}

/// Resolves the public base URL (without trailing slash) used in `Link:`
/// trailers and bug reference links.
///
/// Configured settings (`server.public_base_url`, then `project.domain`) take
/// precedence over the request `Host` header so production instances never
/// reflect attacker-supplied hostnames.
pub fn resolve_public_base_url(settings: &Settings, headers: &HeaderMap) -> String {
    if let Some(url) = settings.server.mailed_link_base_url() {
        return url;
    }
    if let Some(url) = domain_base_url(&settings.project.domain) {
        return url;
    }
    if let Some(url) = header_base_url(headers) {
        return url;
    }
    "https://sashiko.dev".to_string()
}

/// Resolves the `Reported-by:` identity (`Name <email>`) for a bug.
///
/// Preserves external human or bot email addresses recorded on `bug.reporter`;
/// for Sashiko-discovered bugs, uses `smtp.sender_address` when configured with
/// a routable domain, and defaults to `Sashiko <sashiko@sashiko.dev>`.
pub fn resolve_reported_by(settings: &Settings, reporter: &str) -> String {
    let clean_reporter = single_line(reporter);
    let smtp_bare = settings
        .smtp
        .as_ref()
        .map(|s| extract_bare_email(&s.sender_address))
        .filter(|e| !e.is_empty());

    if clean_reporter.contains('@') {
        let reporter_bare = extract_bare_email(&clean_reporter);
        if !reporter_bare.is_empty()
            && !reporter_bare.ends_with("@localhost")
            && smtp_bare.as_deref() != Some(reporter_bare.as_str())
        {
            return clean_reporter;
        }
    }

    let project_name = single_line(&settings.project.name);
    let display_name = if project_name.is_empty() {
        "Sashiko"
    } else {
        project_name.as_str()
    };

    if let Some(smtp) = &settings.smtp {
        let sender = single_line(&smtp.sender_address);
        let bare = extract_bare_email(&sender);
        if bare.contains('@') && !bare.ends_with("@localhost") {
            if sender.contains('<') && sender.contains('>') {
                return sender;
            }
            return format!("{display_name} <{sender}>");
        }
    }

    format!("{display_name} <sashiko@sashiko.dev>")
}

/// Infers the target project for a bug from its `<project>-` ID prefix or the
/// daemon's configured project kind.
pub fn target_project_for_bug(settings: &Settings, bugid: &str) -> ProjectId {
    let lower = bugid.trim().to_ascii_lowercase();
    if lower.starts_with("linux-") {
        ProjectId::Linux
    } else if lower.starts_with("sashiko-") {
        ProjectId::Sashiko
    } else if lower.starts_with("qemu-") {
        ProjectId::Qemu
    } else if lower.starts_with("gcc-") {
        ProjectId::Gcc
    } else if lower.starts_with("zephyr-") {
        ProjectId::Zephyr
    } else {
        settings.project.kind.unwrap_or_default()
    }
}

/// Derives a canonical `Fixes: <12-char sha> ("<subject>")` trailer suggestion
/// from the bug's origin-discovery metadata.
pub fn derive_fixes_suggestion(bug: &Bug) -> FixesSuggestion {
    if let Some(raw) = bug.introduced_in_commit()
        && let Some(parsed) = parse_introduced_commit_str(&raw)
    {
        return parsed;
    }
    if let Some(sha) = bug.introducing_commit_sha() {
        let sha12: String = sha
            .chars()
            .take(12)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if sha12.len() >= 7 {
            return FixesSuggestion::ShaOnly { sha12 };
        }
    }
    FixesSuggestion::Unknown
}

fn parse_introduced_commit_str(raw: &str) -> Option<FixesSuggestion> {
    let clean = single_line(raw);
    let hex_len = clean.bytes().take_while(|b| b.is_ascii_hexdigit()).count();
    if !(7..=40).contains(&hex_len) {
        return None;
    }
    let sha12: String = clean[..hex_len.min(12)].to_ascii_lowercase();
    let mut rest = clean[hex_len..].trim();
    if rest.is_empty() {
        return Some(FixesSuggestion::ShaOnly { sha12 });
    }
    // Strip optional trailing release annotation ` [v6.2-rc1]` appended by
    // `format_commit()` so `Fixes:` complies with `checkpatch.pl`.
    if let Some((before_tag, tag)) = rest.rsplit_once(" [")
        && before_tag.trim_end().ends_with(')')
        && tag.ends_with(']')
        && !tag.contains('[')
    {
        rest = before_tag.trim();
    }
    if let Some(inner) = rest.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        let trimmed = inner.trim();
        let unquoted = if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
            trimmed[1..trimmed.len() - 1].trim()
        } else {
            trimmed
        };
        if !unquoted.is_empty() && !unquoted.eq_ignore_ascii_case("unknown") {
            let line = format!("Fixes: {sha12} (\"{unquoted}\")");
            return Some(FixesSuggestion::Exact { sha12, line });
        }
    }
    Some(FixesSuggestion::ShaOnly { sha12 })
}

fn format_status_label(bug: &Bug) -> &'static str {
    match bug.pipeline_state {
        BugPipelineState::Pending => "Queued (analysis pending)",
        BugPipelineState::Running => "Analyzing",
        BugPipelineState::Failed => "Analysis failed",
        BugPipelineState::Abandoned => "Analysis abandoned",
        BugPipelineState::Succeeded => {
            if bug.is_fixed() {
                "Fixed"
            } else {
                match bug.lifecycle_status {
                    BugLifecycleStatus::New => "New",
                    BugLifecycleStatus::Open => "Open",
                    BugLifecycleStatus::Fixed => "Fixed",
                    BugLifecycleStatus::Dismissed => "Dismissed",
                    BugLifecycleStatus::Duplicate => "Duplicate",
                    BugLifecycleStatus::Closed => "Closed",
                }
            }
        }
    }
}

fn format_unix_utc(ts: i64) -> Option<String> {
    if ts <= 0 {
        return None;
    }
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
}

fn format_locations(locations: &Value) -> Vec<String> {
    let Some(items) = locations.as_array() else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for item in items {
        let file = item
            .get("file")
            .or_else(|| item.get("path"))
            .and_then(Value::as_str)
            .map(single_line)
            .unwrap_or_default();
        if file.is_empty() {
            continue;
        }
        let line_span = match (
            item.get("line")
                .or_else(|| item.get("start_line"))
                .and_then(Value::as_u64),
            item.get("end_line").and_then(Value::as_u64),
        ) {
            (Some(start), Some(end)) if end > start => format!(":{start}-{end}"),
            (Some(start), _) => format!(":{start}"),
            _ => String::new(),
        };
        let symbol = item
            .get("function_or_symbol")
            .or_else(|| item.get("function"))
            .or_else(|| item.get("symbol"))
            .and_then(Value::as_str)
            .map(single_line)
            .filter(|s| !s.is_empty());
        let formatted = match symbol {
            Some(sym) => format!("`{file}{line_span}` (`{sym}`)",),
            None => format!("`{file}{line_span}`"),
        };
        if !lines.contains(&formatted) {
            lines.push(formatted);
        }
    }
    lines
}

/// Renders the complete Markdown prompt and bug dossier for an AI coding agent.
pub fn render_bug_markdown(ctx: &BugMarkdownContext<'_>) -> String {
    let bug = ctx.bug;
    let bugid = single_line(&bug.bugid);
    let encoded_id = encode_uri_component(&bugid);
    let bug_url = format!("{}/#/bug/{encoded_id}", ctx.base_url);
    let md_url = format!("{}/bug/{encoded_id}.md", ctx.base_url);
    let title = single_line(bug.problem());
    let heading_title = if title.is_empty() { &bugid } else { &title };

    let mut out = String::with_capacity(4096);
    push_header_and_task(
        &mut out,
        ctx.project,
        heading_title,
        bug,
        ctx.canonical_duplicate,
    );
    push_metadata_section(&mut out, ctx, &bugid, &bug_url, &md_url);
    push_description_section(&mut out, bug);
    push_supplemental_activity(&mut out, ctx.activity);
    push_patch_guidance(&mut out, ctx, &bugid, &bug_url);
    out
}

fn push_header_and_task(
    out: &mut String,
    project: ProjectId,
    heading_title: &str,
    bug: &Bug,
    canonical_duplicate: Option<&CanonicalDuplicate>,
) {
    let project_label = match project {
        ProjectId::Linux => "Linux Kernel",
        other => other.display_name(),
    };
    out.push_str(&format!("# Fix {project_label} Bug: {heading_title}\n\n"));
    out.push_str(
        "You are tasked with investigating and fixing the pre-existing bug described below.\n\
         Before writing a patch, verify the defect directly in your local git checkout:\n\
         confirm that the buggy code path still exists on the target branch, inspect all\n\
         callers and locking/lifetime invariants, and implement a minimal root-cause fix\n\
         that follows the surrounding subsystem's conventions without unrelated refactoring.\n\n",
    );

    if bug.is_fixed() || bug.lifecycle_status.is_resolved() {
        out.push_str("> **Note:** This bug report is currently marked **");
        out.push_str(format_status_label(bug));
        out.push_str("**. Check whether the fix has already landed on your target branch");
        if let Some(canon) = canonical_duplicate {
            out.push_str(&format!(
                " (canonical report: `{}`)",
                single_line(&canon.bugid)
            ));
        }
        out.push_str(" before preparing a new patch.\n\n");
    }
}

fn push_metadata_section(
    out: &mut String,
    ctx: &BugMarkdownContext<'_>,
    bugid: &str,
    bug_url: &str,
    md_url: &str,
) {
    let bug = ctx.bug;
    out.push_str("## Bug Metadata\n\n");
    out.push_str(&format!("- **Bug ID**: `{bugid}`\n"));
    out.push_str(&format!("- **Bug URL**: {bug_url}\n"));
    out.push_str(&format!("- **Markdown URL**: {md_url}\n"));
    out.push_str(&format!("- **Status**: {}\n", format_status_label(bug)));
    if let Some(canon) = ctx.canonical_duplicate {
        let canon_id = single_line(&canon.bugid);
        let canon_url = format!("{}/#/bug/{}", ctx.base_url, encode_uri_component(&canon_id));
        let canon_prob = single_line(&canon.problem);
        out.push_str(&format!(
            "- **Duplicate Of**: `{canon_id}` ({canon_prob}) — {canon_url}\n"
        ));
    }
    let sev = bug.severity();
    match bug.severity_explanation().filter(|s| !s.trim().is_empty()) {
        Some(exp) if sev != Severity::Unknown => {
            out.push_str(&format!(
                "- **Severity**: {} — {}\n",
                sev.as_str(),
                single_line(&exp)
            ));
        }
        _ => out.push_str(&format!("- **Severity**: {}\n", sev.as_str())),
    }
    if !bug.subsystems.is_empty() {
        let subs: Vec<String> = bug
            .subsystems
            .iter()
            .map(|s| format!("`{}`", single_line(s)))
            .collect();
        out.push_str(&format!("- **Subsystems**: {}\n", subs.join(", ")));
    }
    if let Some(files) = bug.attributable_files().filter(|f| !f.is_empty()) {
        let formatted: Vec<String> = files
            .iter()
            .map(|f| format!("`{}`", single_line(f)))
            .collect();
        out.push_str(&format!("- **Affected Files**: {}\n", formatted.join(", ")));
    }
    if let Some(locs) = bug.locations() {
        let rendered = format_locations(&locs);
        if !rendered.is_empty() {
            out.push_str("- **Code Locations**:\n");
            for loc in rendered {
                out.push_str(&format!("  - {loc}\n"));
            }
        }
    }
    if let Some(sha) = bug.verified_on_sha().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!(
            "- **Verified on Commit**: `{}`\n",
            single_line(&sha)
        ));
    }
    if let Some(intro) = bug.introduced_in_commit().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!(
            "- **Introduced by Commit**: `{}`\n",
            single_line(&intro)
        ));
    }
    if let Some(fixed) = bug.fixed_in_commit().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!(
            "- **Fixed by Commit**: `{}`\n",
            single_line(&fixed)
        ));
    }
    let reported_ts = if bug.reported_at > 0 {
        bug.reported_at
    } else {
        bug.created_at
    };
    if let Some(ts) = format_unix_utc(reported_ts) {
        out.push_str(&format!("- **Reported**: {ts}\n"));
    }
    out.push('\n');
}

fn push_description_section(out: &mut String, bug: &Bug) {
    out.push_str("## Bug Description\n\n");
    if let Some(desc) = bug.description().filter(|d| !d.trim().is_empty()) {
        push_fenced_block(out, "text", &desc);
    } else if bug.pipeline_state.is_in_progress() {
        out.push_str("_Automated verification and report generation are still in progress._\n");
    } else if let Some(exp) = bug.severity_explanation().filter(|s| !s.trim().is_empty()) {
        push_fenced_block(out, "text", &exp);
    } else {
        out.push_str("_No standalone bug description has been recorded._\n");
    }
    out.push('\n');
}

fn push_supplemental_activity(out: &mut String, activity: &[Value]) {
    let mut reproducers = Vec::new();
    let mut comments = Vec::new();
    let mut fixes = Vec::new();

    for item in activity {
        let Some(kind) = item.get("kind").and_then(Value::as_str) else {
            continue;
        };
        let content = item
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let fixed_ref = item
            .get("fixed_in_commit")
            .or_else(|| item.get("commit_sha"))
            .and_then(Value::as_str)
            .map(single_line)
            .filter(|s| !s.is_empty());

        if matches!(kind, "reproducer" | "testcase" | "poc") && !content.is_empty() {
            reproducers.push((item, content));
        } else if kind == "comment" && !content.is_empty() {
            comments.push((item, content));
        } else if (kind == "fix"
            || kind == "fix_candidate"
            || kind.starts_with("fix_")
            || kind == "proposed_fix"
            || kind == "suggested_fix")
            && (!content.is_empty() || fixed_ref.is_some())
        {
            fixes.push((item, content, fixed_ref));
        }
    }

    if !reproducers.is_empty() {
        out.push_str("## Reproducers\n\n");
        for (item, content) in reproducers {
            push_activity_heading(out, item);
            push_fenced_block(out, "text", content);
            out.push('\n');
        }
    }

    if !comments.is_empty() {
        comments
            .sort_by_key(|(item, _)| item.get("created_at").and_then(Value::as_i64).unwrap_or(0));
        out.push_str("## Comments & Discussion\n\n");
        for (item, content) in comments {
            push_activity_heading(out, item);
            push_fenced_block(out, "text", content);
            out.push('\n');
        }
    }

    if !fixes.is_empty() {
        out.push_str("## Prior Fix Candidates\n\n");
        for (item, content, fixed_ref) in fixes {
            push_activity_heading(out, item);
            if let Some(commit_ref) = fixed_ref {
                out.push_str(&format!("Commit: `{commit_ref}`\n\n"));
            }
            if !content.is_empty() {
                push_fenced_block(out, "text", content);
                out.push('\n');
            }
        }
    }
}

fn push_activity_heading(out: &mut String, item: &Value) {
    let author = item
        .get("author")
        .and_then(Value::as_str)
        .map(single_line)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sashiko".to_string());
    let when = item
        .get("created_at")
        .and_then(Value::as_i64)
        .and_then(format_unix_utc);
    match when {
        Some(ts) => out.push_str(&format!("### {author} ({ts})\n\n")),
        None => out.push_str(&format!("### {author}\n\n")),
    }
}

fn push_patch_guidance(out: &mut String, ctx: &BugMarkdownContext<'_>, bugid: &str, bug_url: &str) {
    let fixes = derive_fixes_suggestion(ctx.bug);
    let reported_by = single_line(ctx.reported_by);

    out.push_str("## Patch Formatting Guidance\n\n");
    if ctx.project == ProjectId::Linux {
        out.push_str(
            "Format the commit fixing this bug according to\n\
             `Documentation/process/submitting-patches.rst`:\n\n",
        );
    } else {
        out.push_str("Format the commit fixing this bug according to repository conventions:\n\n");
    }

    out.push_str(
        "1. **Subject Line**: Use `<subsystem_prefix>: <concise imperative summary>`\n\
            (keep under 72 characters; do not use backticks in the commit message).\n\
            Run `git log --oneline -n 20 -- <affected_file>` to match the exact prefix\n\
            style used by recent commits to the modified files.\n\
         2. **Commit Message Body**: State the broken invariant, trigger condition, and\n\
            concrete technical impact first, then describe the fix in imperative mood.\n\
            Hard-wrap prose lines at 72 characters.\n\
         3. **Suggested Commit Trailers**: Place all trailers in one contiguous block at\n\
            the end of the commit message:\n\n",
    );

    let mut trailer_block = String::new();
    match &fixes {
        FixesSuggestion::Exact { line, .. } => {
            trailer_block.push_str(line);
            trailer_block.push('\n');
        }
        FixesSuggestion::ShaOnly { sha12 } => {
            trailer_block.push_str(&format!("Fixes: {sha12} (\"<commit subject>\")\n"));
        }
        FixesSuggestion::Unknown => {
            trailer_block.push_str(
                "# Fixes: <12-char-sha> (\"<commit subject>\")  # include if introducing commit is identified\n",
            );
        }
    }
    if ctx.project == ProjectId::Sashiko {
        trailer_block.push_str(&format!("Fixes: {bugid}\n"));
    }
    trailer_block.push_str(&format!("Reported-by: {reported_by}\n"));
    trailer_block.push_str(&format!("Link: {bug_url}\n"));
    trailer_block.push_str("Signed-off-by: Your Name <your.email@example.com>");
    push_fenced_block(out, "text", &trailer_block);
    out.push('\n');

    push_fixes_bullet(out, &fixes);
    out.push_str(&format!(
        "- **`Reported-by:` & `Link:`**: Keep `Reported-by: {reported_by}` and\n\
           `Link: {bug_url}` so reviewers can inspect the full bug report",
    ));
    if ctx.project == ProjectId::Linux {
        out.push_str(
            " (if\n  `scripts/checkpatch.pl` in the target tree requests `Closes:` after\n\
             `Reported-by:`, `Closes: <url>` may be used in place of `Link:`).\n",
        );
    } else {
        out.push_str(".\n");
    }
    out.push_str(
        "- **`Signed-off-by:`**: Sign off with the human patch author's real name and\n\
           email (`git commit -s`). Do not emit AI/bot `Co-authored-by:` or\n\
           `Assisted-by:` trailers unless explicitly requested by the user or required\n\
           by target project policy.\n",
    );
    if ctx.project == ProjectId::Linux {
        out.push_str(
            "4. **Validation**: Build the modified subsystem/files, run relevant KUnit or\n\
                kselftest targets, and verify the formatted patch with\n\
                `./scripts/checkpatch.pl`.\n",
        );
    }
}

fn push_fixes_bullet(out: &mut String, fixes: &FixesSuggestion) {
    match fixes {
        FixesSuggestion::Exact { sha12, .. } => {
            out.push_str(&format!(
                "- **`Fixes:`**: Verify the introducing commit in your local repository with\n\
                   `git show {sha12}` and confirm the canonical trailer with\n\
                   `git log -1 --format='Fixes: %h (\"%s\")' --abbrev=12 {sha12}`. Do not\n\
                   abbreviate or paraphrase the commit subject; omit `Fixes:` if inspection\n\
                   shows that commit did not introduce the bug and no single introducing\n\
                   commit can be proven.\n"
            ));
        }
        FixesSuggestion::ShaOnly { sha12 } => {
            out.push_str(&format!(
                "- **`Fixes:`**: Verify commit `{sha12}` with `git show {sha12}` and populate\n\
                   the exact quoted commit subject using\n\
                   `git log -1 --format='Fixes: %h (\"%s\")' --abbrev=12 {sha12}`.\n"
            ));
        }
        FixesSuggestion::Unknown => {
            out.push_str(
                "- **`Fixes:`**: Use `git blame` and `git show` on the buggy lines to determine\n\
                   whether a specific commit introduced the defect. When identified with\n\
                   confidence, generate the trailer with\n\
                   `git log -1 --format='Fixes: %h (\"%s\")' --abbrev=12 <sha>`; otherwise\n\
                   omit the `Fixes:` line rather than guessing.\n",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::BugEnrichment;
    use axum::http::HeaderValue;
    use serde_json::json;

    fn sample_bug(bugid: &str) -> Bug {
        Bug {
            id: 42,
            bugid: bugid.to_string(),
            title: "smb: server: fix fp leak in parse_durable_handle_context()".to_string(),
            lifecycle_status: BugLifecycleStatus::Open,
            pipeline_state: BugPipelineState::Succeeded,
            reporter: "sashiko".to_string(),
            reported_at: 1_762_345_678,
            assignee: None,
            assigned_at: None,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            created_at: 1_762_345_678,
            updated_at: 1_762_345_678,
            subsystems: vec!["SMB3 SERVER (KSMBD)".to_string()],
            enrichments: Vec::new(),
        }
    }

    fn enrichment(kind: &str, content: Option<&str>, data_json: Option<Value>) -> BugEnrichment {
        BugEnrichment {
            id: 1,
            bug_id: 42,
            kind: kind.to_string(),
            tool: "sashiko:linux_bug".to_string(),
            model: Some("gemini-test".to_string()),
            author: Some("sashiko".to_string()),
            created_at: 1_762_345_678,
            content: content.map(str::to_string),
            data_json,
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
            logs: None,
        }
    }

    #[test]
    fn zephyr_bug_prefix_overrides_missing_or_different_project_configuration() {
        let bug = sample_bug("zephyr-7f9a2c4b");
        for configured in [None, Some(ProjectId::Linux), Some(ProjectId::Sashiko)] {
            let mut settings = Settings::new().unwrap();
            settings.project.kind = configured;
            let project = target_project_for_bug(&settings, " ZEPHYR-7f9a2c4b ");
            assert_eq!(project, ProjectId::Zephyr);
            let md = render_bug_markdown(&BugMarkdownContext {
                bug: &bug,
                base_url: "https://example.org",
                reported_by: "Sashiko <sashiko@example.org>",
                project,
                canonical_duplicate: None,
                activity: &[],
            });
            assert!(md.contains("# Fix Zephyr Bug:"));
            assert!(!md.contains("Documentation/process/submitting-patches.rst"));
            assert!(!md.contains("KUnit"));
        }
    }

    #[test]
    fn test_derive_fixes_suggestion_strips_release_tag_and_quotes_subject() {
        let mut bug = sample_bug("linux-abc123");
        bug.enrichments.push(enrichment(
            "origin_discovery",
            Some("3330ee8db9890123456789012345678901234567 (\"bpf: Support stack arguments\") [v6.2-rc1]"),
            None,
        ));
        assert_eq!(
            derive_fixes_suggestion(&bug),
            FixesSuggestion::Exact {
                sha12: "3330ee8db989".to_string(),
                line: "Fixes: 3330ee8db989 (\"bpf: Support stack arguments\")".to_string(),
            }
        );

        let mut unquoted = sample_bug("linux-abc124");
        unquoted.enrichments.push(enrichment(
            "origin_discovery",
            None,
            Some(json!({
                "introducing_commit_sha": "AABBCCDDEEFF00112233445566778899AABBCCDD",
                "introducing_commit_title": "Revert \"net: broken [tag] (v2)\""
            })),
        ));
        assert_eq!(
            derive_fixes_suggestion(&unquoted),
            FixesSuggestion::Exact {
                sha12: "aabbccddeeff".to_string(),
                line: "Fixes: aabbccddeeff (\"Revert \"net: broken [tag] (v2)\"\")".to_string(),
            }
        );
    }

    #[test]
    fn test_derive_fixes_suggestion_sha_only_and_unknown() {
        let mut sha_only = sample_bug("linux-shaonly");
        sha_only.enrichments.push(enrichment(
            "origin_discovery",
            Some("1234567890abcdef (\"unknown\") [v6.8]"),
            None,
        ));
        assert_eq!(
            derive_fixes_suggestion(&sha_only),
            FixesSuggestion::ShaOnly {
                sha12: "1234567890ab".to_string()
            }
        );

        let unknown = sample_bug("linux-none");
        assert_eq!(derive_fixes_suggestion(&unknown), FixesSuggestion::Unknown);
    }

    #[test]
    fn test_resolve_reported_by_variants() {
        let mut settings = Settings::new().unwrap();
        settings.project.name = "Sashiko".to_string();
        settings.smtp = None;

        assert_eq!(
            resolve_reported_by(&settings, "sashiko"),
            "Sashiko <sashiko@sashiko.dev>"
        );
        assert_eq!(
            resolve_reported_by(&settings, "sashiko.dev"),
            "Sashiko <sashiko@sashiko.dev>"
        );
        assert_eq!(
            resolve_reported_by(&settings, "authorized client (127.0.0.1)"),
            "Sashiko <sashiko@sashiko.dev>"
        );
        assert_eq!(
            resolve_reported_by(&settings, "Jane Developer <jane@kernel.org>"),
            "Jane Developer <jane@kernel.org>"
        );
        assert_eq!(
            resolve_reported_by(&settings, "sashiko-bot@localhost"),
            "Sashiko <sashiko@sashiko.dev>"
        );

        settings.smtp = Some(crate::settings::SmtpSettings {
            transport: crate::settings::MailTransport::Smtp,
            server: Some("smtp.example.org".to_string()),
            port: Some(465),
            username: None,
            password: None,
            sendmail_path: None,
            sender_address: "bot@custom.example.org".to_string(),
            reply_to: None,
            dry_run: true,
        });
        assert_eq!(
            resolve_reported_by(&settings, "sashiko"),
            "Sashiko <bot@custom.example.org>"
        );
        assert_eq!(
            resolve_reported_by(&settings, "bot@custom.example.org"),
            "Sashiko <bot@custom.example.org>"
        );
    }

    #[test]
    fn test_resolve_public_base_url_priority_and_host_validation() {
        let mut settings = Settings::new().unwrap();
        settings.server.public_base_url = Some("https://sashiko.dev/".to_string());
        settings.project.domain = "other.example.org".to_string();

        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("evil.example.com"));
        assert_eq!(
            resolve_public_base_url(&settings, &headers),
            "https://sashiko.dev"
        );

        settings.server.public_base_url = None;
        assert_eq!(
            resolve_public_base_url(&settings, &headers),
            "https://other.example.org"
        );

        settings.project.domain.clear();
        let mut local_headers = HeaderMap::new();
        local_headers.insert("host", HeaderValue::from_static("localhost:8080"));
        assert_eq!(
            resolve_public_base_url(&settings, &local_headers),
            "http://localhost:8080"
        );

        let mut bad_headers = HeaderMap::new();
        bad_headers.insert("host", HeaderValue::from_static("evil.com/path#inject"));
        assert_eq!(
            resolve_public_base_url(&settings, &bad_headers),
            "https://sashiko.dev"
        );
    }

    #[test]
    fn test_render_bug_markdown_fences_backticks_and_emits_trailers() {
        let mut bug = sample_bug("linux-7f9a2c4b");
        bug.enrichments.push(enrichment(
            "verification",
            None,
            Some(json!({
                "verified_on_sha": "deadbeefcafe0123456789abcdef0123456789ab",
                "source_files": ["fs/smb/server/smb2pdu.c"],
                "locations": [
                    {"file": "fs/smb/server/smb2pdu.c", "function_or_symbol": "parse_durable_handle_context", "line": 2450}
                ]
            })),
        ));
        bug.enrichments.push(enrichment(
            "origin_discovery",
            Some("3330ee8db989 (\"smb: server: add durable handle\") [v6.1-rc1]"),
            None,
        ));
        bug.enrichments.push(enrichment(
            "severity_calibration",
            Some("Unbounded kernel memory leak reachable by unauthenticated clients."),
            Some(json!({ "severity": "High" })),
        ));
        bug.enrichments.push(enrichment(
            "report",
            Some("Report body containing ````quadruple backticks```` and < ... > elisions."),
            None,
        ));

        let activity = vec![json!({
            "kind": "comment",
            "author": "Maintainer <m@kernel.org>",
            "created_at": 1_762_346_000i64,
            "content": "Please also free fp->handle on the second error return."
        })];

        let md = render_bug_markdown(&BugMarkdownContext {
            bug: &bug,
            base_url: "https://sashiko.dev",
            reported_by: "Sashiko <sashiko@sashiko.dev>",
            project: ProjectId::Linux,
            canonical_duplicate: None,
            activity: &activity,
        });

        assert!(md.contains("# Fix Linux Kernel Bug: smb: server: fix fp leak"));
        assert!(md.contains("- **Bug ID**: `linux-7f9a2c4b`"));
        assert!(md.contains("- **Bug URL**: https://sashiko.dev/#/bug/linux-7f9a2c4b"));
        assert!(md.contains("- **Markdown URL**: https://sashiko.dev/bug/linux-7f9a2c4b.md"));
        assert!(md.contains("`fs/smb/server/smb2pdu.c:2450` (`parse_durable_handle_context`)"));
        assert!(md.contains("`````text\nReport body containing ````quadruple backticks````"));
        assert!(md.contains("## Comments & Discussion"));
        assert!(md.contains("Please also free fp->handle on the second error return."));
        assert!(md.contains("Fixes: 3330ee8db989 (\"smb: server: add durable handle\")\n"));
        assert!(md.contains("Reported-by: Sashiko <sashiko@sashiko.dev>\n"));
        assert!(md.contains("Link: https://sashiko.dev/#/bug/linux-7f9a2c4b\n"));
        assert!(!md.contains("Bug #42"));
    }
}
