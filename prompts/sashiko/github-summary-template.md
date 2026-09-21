# Inline Review Report Template

Produce a plain-text inline review report based on the findings provided.

## Text Formatting Rules (Strictly Enforced)

- **Plain text only.** No markdown, no backticks (`), no markdown headings
  (`#`), no bold/italics (`**` or `*`), and no fenced code blocks (` ``` `).
  Never quote function names, variable names, types, or file paths in backticks.
- **No section headers.** Do NOT include `Summary:`, `Findings:`, or any other
  headers or preamble. Output ONLY the plain bulleted list of findings (or
  `No issues found.` if there are no findings).
- **Wrap lines at 78 characters.** Every finding description must be
  hard-wrapped at 78 characters or fewer so the report fits cleanly inside an
  80-column terminal window. Indent wrapped continuation lines of a bullet item
  by 2 spaces.
- **No line numbers.** Never mention line numbers when referencing code
  locations. Name the file path and function, method, or struct name in plain
  text instead.
- **Factual and concise.** Keep problem descriptions short, conscious, direct,
  and self-contained. State where the issue is (file and function/symbol), what
  is wrong, and why it matters. Do not add greetings, praise, filler, or
  sign-offs.
- **Include every finding.** You MUST list every finding passed to this stage.
  Do not omit any finding.
- **Order findings by severity** from highest (`[CRITICAL]`) to lowest
  (`[LOW]`).
- **Name the stages that raised each finding.** Where a finding carries a
  `stages` array, put those entries in parentheses right after the severity,
  copied exactly and comma-separated. Add nothing when a finding has no
  `stages`, and never name a stage a finding does not list.
- **Empty line between findings.** Separate individual findings with a single
  empty line so each finding stands out clearly.

## Exact Output Structure

When findings are present, output ONLY a bulleted list where every bullet starts
with `- [<SEVERITY>]` and individual findings are separated by an empty line:

- [<SEVERITY>] (<stages>) <short, conscious problem description naming
  file and symbol>

- [<SEVERITY>] (<stages>) <short, conscious problem description naming
  file and symbol>

Where `<SEVERITY>` is strictly one of `CRITICAL`, `HIGH`, `MEDIUM`, or `LOW` in
uppercase square brackets, and `(<stages>)` is the finding's `stages`
entries, omitted entirely when it has none. If a finding is pre-existing
(not introduced by this patch), note `(pre-existing)` at the start of its
description, after the stages.

If there are no findings at all, output exactly:

No issues found.

## Example (findings present)

- [CRITICAL] (Concurrency & Async, DB & Persistence) In src/worker/sync.rs
  (GitSyncWorker::sync_all_remotes), holding the synchronous
  std::sync::MutexGuard across the async fetch_remote() call can deadlock
  Tokio worker threads when multiple remotes sync concurrently.

- [HIGH] (Security Audit) In src/worker/sync.rs
  (GitSyncWorker::sync_all_remotes), unredacted git fetch stderr is logged
  via warn! and error! when remote URLs fail, leaking embedded
  authentication tokens into application logs.

- [MEDIUM] (Interfaces & Compat) In src/api.rs (forge_webhook), the
  placeholder cover letter message ID omits the @sashiko.local domain suffix
  expected by resolve_root_msg_id(), causing git fetch ingestion to create a
  duplicate patchset row.

- [LOW] Commit message body contains unwrapped single-line paragraphs exceeding
  100 characters and quotes function names in backticks.

## Example (no issues found)

No issues found.
