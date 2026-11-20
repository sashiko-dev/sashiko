# Design: Concise Bug Reports

## 1. Motivation

The Report generation stage of the bug pipeline (`ReportSession` in
`src/workflows/linux_bug.rs`) writes the standalone description stored in
`bugs.inline_review` and shown on the bug page next to the title, severity
and introducing commit.

Its prompts predate the rewrite of the Linux inline review template
(`projects/linux/prompts/inline-template.md`, commits "prompts: linux:
rewrite the inline report template around one rule" and "workflows: linux:
align the report instruction with the new template"). They still carry the
recipe-style rules that the rewrite removed from patch reviews:

- a fixed narrative (opening sentence, causal chain, consequence) that the
  model fills in for every bug, so a one-line leak gets as much prose as a
  cross-CPU race;
- "include code snippets for all localized defects" and a mandatory
  allocation-plus-exit snippet for every leak, so most reports quote code
  that one sentence could replace;
- "// path:start-end" snippet headers and line numbers that go stale as
  soon as mainline moves;
- long few-shot examples taken from real kernel code;
- a user prompt that repeats the whole system prompt as a checklist.

The Sashiko, GCC and QEMU variants are shorter but follow the same recipe
and give no guidance on length, distillation or diagrams.

## 2. Goals

- Bug reports follow the same governing rule as inline reviews: every line
  either helps the reader understand the problem or shows why it is worth
  fixing.
- Length follows how hard a bug is to explain, not its severity.
- All four projects share one set of writing rules; only the role,
  audience, vocabulary and examples differ per project.

## 3. Non-goals

- No change to the pipeline, the stored data, the UI or the stage inputs.
- No change to the other bug stages (normalization, verification, severity).
- No new deterministic validator; the output is still accepted when it is
  non-empty.

## 4. Design

### 4.1 Shared writing rules

A single `REPORT_WRITING_RULES` constant holds the rules adapted from
`inline-template.md` for a standalone report:

- **Governing rule** and the list of what to delete (restated input,
  proof steps the reader does not need, generic remarks, closing
  summaries).
- **No re-judging**: earlier stages verified and rated the bug.
- **Length**: an obvious bug is two or three lines; a subtle race is a
  diagram and a couple of paragraphs.
- **Writing the report**: the first sentence says what is wrong, where
  (function) and what triggers it, leading with any narrowing condition
  (architecture, config, hardware). Add a sentence only if the reader
  would otherwise have to work something out. One trigger, the consequence
  that matters most. Statements, not questions: this is a report, not a
  reply to a patch author.
- **Distill, do not transcribe**: the description, verification details
  and severity explanation are working notes. Never copy their
  "Trigger:", "Failure Mechanism:" or "Impact:" structure. Do not restate
  the title, severity or introducing commit, which are displayed with the
  report.
- **Scope**: describe the defect only; no fixes or patches (unchanged).
- **Evidence**: quote code only when prose cannot say the same as
  briefly; never to show that something is absent. Label snippets with the
  file and function (`net/foo.c:foo_open() {`), elide with `...`, and
  never mention line numbers. Carets only under an existing defective
  token, never at a missing call.
- **Scenario diagrams** for races, deadlocks and reentrancy, using the
  template's conventions: time flows downward, one column per CPU, thread
  or task, a single unlabelled column for same-CPU nesting, an arrow note
  on the failing step, skipped when one sentence suffices.
- **Paragraphs and wording**: short sentences, paragraphs of at most four
  lines, established terms, one hedge at most, one name per code element.
- **Format**: plain text, no markdown, fences or backticks, `func()` names,
  no ALL CAPS, lines wrapped at 75 columns, commits cited as a 12-character
  sha with the quoted subject.

### 4.2 Per-project parts

Each project supplies a role sentence, the concepts not to explain
(kernel locking primitives, Rust/Tokio basics, GCC IR basics, QOM and
MemoryRegion basics), the established consequence vocabulary, and two or
three short made-up examples: a prose-only bug, a bug that needs a small
snippet, and (for Linux and Sashiko) a race with a scenario diagram. The
examples are invented, so they cannot leak into benchmark expectations.

### 4.3 User prompt

The user prompt keeps all inputs (title, severity, description,
verification details, introducing commit, locations, prefetched code) and
replaces the repeated checklist with a three-line task: write the report
following the system prompt, distill rather than transcribe, and keep it
as short as the bug allows.

### 4.4 Tests

A unit test checks that the examples in every project's system prompt obey
the 75-column rule they teach. Existing tests for the stage heading and
the validator stay unchanged.

## 5. Implementation plan

1. Commit this design document.
2. Rewrite the report system prompts of all projects around the shared
   rules, with the example width test.
3. Replace the checklist in the shared user prompt with the short task.
