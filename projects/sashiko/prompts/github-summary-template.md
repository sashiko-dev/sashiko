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
- **Empty line between findings.** Separate individual findings with a single
  empty line so each finding stands out clearly.

## Exact Output Structure

When findings are present, output ONLY a bulleted list where every bullet starts
with `- [<SEVERITY>]` and individual findings are separated by an empty line:

- [<SEVERITY>] <short, conscious problem description naming file and symbol>

- [<SEVERITY>] <short, conscious problem description naming file and symbol>

Where `<SEVERITY>` is strictly one of `CRITICAL`, `HIGH`, `MEDIUM`, or `LOW` in
uppercase square brackets. If a finding is pre-existing (not introduced by this
patch), note `(pre-existing)` at the start of its description.

If there are no findings at all, output exactly:

No issues found.

## Example (findings present)

- [CRITICAL] In src/worker/sync.rs (GitSyncWorker::sync_all_remotes), holding
  the synchronous std::sync::MutexGuard across the async fetch_remote() call
  can deadlock Tokio worker threads when multiple remotes sync concurrently.

- [HIGH] In src/worker/sync.rs (GitSyncWorker::sync_all_remotes), unredacted
  git fetch stderr is logged via warn! and error! when remote URLs fail,
  leaking embedded authentication tokens into application logs.

- [MEDIUM] In src/api.rs (forge_webhook), the placeholder cover letter message
  ID omits the @sashiko.local domain suffix expected by resolve_root_msg_id(),
  causing git fetch ingestion to create a duplicate patchset row.

- [LOW] Commit message body contains unwrapped single-line paragraphs exceeding
  100 characters and quotes function names in backticks.

## Example (no issues found)

No issues found.
