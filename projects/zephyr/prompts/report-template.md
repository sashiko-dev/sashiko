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
- **State the trigger and the evidence.** Each finding names where the problem
  is, what triggers it (including any Kconfig option or Devicetree configuration
  it depends on), and the code or document that shows it is wrong.
- **Write like a seasoned maintainer, not a tutorial.** One line for a minor
  point. Two to three lines for a bug: state the bug, cite the code or document,
  suggest the fix. Do not re-explain the code being reviewed. Do not pad with
  praise or filler.
- **Include every finding.** You MUST list every finding passed to this stage.
  Do not omit any finding.
- **Order findings by severity** from highest (`[CRITICAL]`) to lowest
  (`[LOW]`).
- **Empty line between findings.** Separate individual findings with a single
  empty line so each finding stands out clearly.

## Exact Output Structure

When findings are present, output ONLY a bulleted list where every bullet starts
with `- [<SEVERITY>]` and individual findings are separated by an empty line:

- [<SEVERITY>] <short problem description naming file and symbol>

- [<SEVERITY>] <short problem description naming file and symbol>

Where `<SEVERITY>` is strictly one of `CRITICAL`, `HIGH`, `MEDIUM`, or `LOW` in
uppercase square brackets. If a finding is pre-existing (not introduced by this
patch), note `(pre-existing)` at the start of its description.

## Example (findings present)

- [CRITICAL] In subsys/net/lib/foo/foo.c (foo_parse_option), the option length
  from the received packet is used for a memcpy into a fixed stack buffer
  without a bounds check, so a remote peer can overwrite the stack.

- [HIGH] In drivers/sensor/foo/foo.c (foo_irq_handler), k_mutex_lock() is
  called from the ISR. kernel/mutex.c only asserts against this, so builds
  without CONFIG_ASSERT corrupt the mutex wait queue. Offload to a work item.

- [LOW] Commit message body restates the diff without explaining why the
  default of FOO_BUFFER_SIZE changes.

## Example (no issues found)

No issues found.
