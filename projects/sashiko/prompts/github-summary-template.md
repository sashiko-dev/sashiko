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
- **Factual and concise.** Keep problem descriptions short, direct, and
  self-contained. State where the issue is (file and function/symbol), what
  is wrong, and why it matters. Do not add greetings, praise, filler, or
  sign-offs.
- **Simple, direct LKML-style language.** Write the way an experienced
  maintainer replies to a patch on a mailing list:
  - Use short sentences, plain words, and the active voice. Name the code
    that misbehaves and say what it does wrong: "foo() returns without
    releasing the lock", not "there is a potential concern regarding lock
    handling".
  - Describe the concrete failure: the input or sequence of events that
    triggers it and what the user or operator sees as a result.
  - When the fix is obvious, say it in one sentence ("Should this use
    spawn_blocking()?" or "Return the error instead of logging it.").
  - Do not hedge or inflate. Avoid filler and buzzwords such as "it is worth
    noting", "potentially", "robust", "comprehensive", "critical
    vulnerability", or "seamless". The severity tag already says how serious
    the issue is; do not repeat it in prose.
  - Do not restate the patch, explain basic Rust or Sashiko concepts, or
    describe the review process (stages, tools, verification steps).
  - Write function names with trailing parentheses (fetch_remote()) and
    refer to other patches in the series by subject, never by hash.
- **Include every finding.** You MUST list every finding passed to this stage.
  Do not omit any finding.
- **Order findings by severity** from highest (`[CRITICAL]`) to lowest
  (`[LOW]`).
- **Empty line between findings.** Separate individual findings with a single
  empty line so each finding stands out clearly.

## Exact Output Structure

When findings are present, output ONLY a bulleted list where every bullet starts
with `- [<SEVERITY>]` and individual findings are separated by an empty line:

- [<SEVERITY>] <short, direct problem description naming file and symbol>

- [<SEVERITY>] <short, direct problem description naming file and symbol>

Where `<SEVERITY>` is strictly one of `CRITICAL`, `HIGH`, `MEDIUM`, or `LOW` in
uppercase square brackets. If a finding is pre-existing (not introduced by this
patch), note `(pre-existing)` at the start of its description. If a finding
includes `prior_attribution` with an `attribution_line` (for example
`As reported by Alice on v1`), begin its description with
`<attribution_line> [N], ...` integrated naturally into the opening sentence,
print only the reviewer's display name (never their `<email>` address), number
footnotes sequentially (`[1]`, `[2]`, ...) across the review, and list all
`[N] <reference_url>` entries together at the very bottom of the review when
`reference_url` is present. Never credit earlier reviewers anywhere else, and
use no backticks or markdown link syntax.

If there are no findings at all, output exactly:

No issues found.

## Example (findings present)

- [CRITICAL] src/worker/sync.rs: GitSyncWorker::sync_all_remotes() holds a
  std::sync::MutexGuard across fetch_remote().await. When two remotes sync at
  the same time, this deadlocks the Tokio worker threads. Drop the guard
  before the await.

- [HIGH] src/worker/sync.rs: when a fetch fails, sync_all_remotes() logs the
  raw git stderr with warn!() and error!(). The stderr contains the remote
  URL, so any token embedded in it ends up in the logs.

- [MEDIUM] src/api.rs: forge_webhook() builds the placeholder cover letter
  message ID without the @sashiko.local suffix that resolve_root_msg_id()
  expects. Git fetch ingestion then creates a second patchset row for the
  same pull request.

- [LOW] The commit message has unwrapped paragraphs longer than 100
  characters and quotes function names in backticks.

## Example (no issues found)

No issues found.
