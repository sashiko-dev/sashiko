# False Positive Prevention Guide

This guide is used where avoiding false positives matters most. Shift bias away
from fast processing and follow it carefully.

## Core principle

**If you cannot prove an issue exists with concrete evidence from this
codebase, do not report it.**

Evidence means code you have read. Not "Rust services usually", not "this looks
like it could", not "a caller might". Name the function, name the path, name
the input that reaches it.

The corollary matters as much: proving a path is *structurally possible* is
enough. You do not have to prove it executes on every run. A lock held across
an `.await` is a bug even if the contention window is small. An outbox row with
no terminal state is a bug even if it has not stuck yet.

## Patterns that produce false positives here

### 1. The linter already ran

`make lint` runs `cargo clippy` and `cargo fmt` on Rust source files before any
human sees the change. Do not report source code formatting, naming, import
order, `needless_borrow`, missing `#[derive]`, redundant clones, or anything
else clippy emits on Rust source files.

Note: `make lint` does NOT check commit messages. Commit message defects —
such as missing or cryptic/nickname `Signed-off-by` trailers, missing
explanation of *what* and *why*, genuinely unwrapped prose lines exceeding ~85
characters, backticks quoting code/symbols in the commit message, or internal
metadata tags (`TAG=`, `CONV=`) — are valid findings and must NOT be dismissed
as linter issues. However, do NOT nitpick commit message lines that are 73-80
characters long or lines that exceed the margin for reasonable reasons (quoting
code, compiler/log output, URLs, or file paths).

- Bad: "Consider using `iter()` instead of `into_iter()` here."
- Bad: "This `clone()` looks unnecessary."
- Bad: "The commit message body has lines that are 74 characters long."
- Good: "This `clone()` copies the full patch body on every stage, and
  `max_input_tokens` is already the binding constraint" — a consequence, not a
  style preference.

### 2. The compiler already ran — never vibe-guess build bugs

Build and compilation checks (`cargo check`, `cargo test`, `cargo clippy`) are
100% deterministic. Never vibe-guess whether Rust code compiles, links, or
passes borrow checking. None of the following can reach review and none of them
are valid LLM findings:
- Syntax errors, macro expansion errors, or unclosed delimiters.
- Missing or unresolved imports (`use ...`), missing crate dependencies or
  feature flags in `Cargo.toml`, module visibility (`pub` / `pub(crate)`), or
  unresolved symbols/types/methods/functions.
- Type mismatches, wrong function arity/signatures, missing struct fields, or
  unsatisfied trait bounds (`Send`, `Sync`, `'static`, etc.).
- Borrow-checker, move, mutability (`mut`), or lifetime errors.
- Non-exhaustive `match` arms on closed enums (note: wildcard `_ =>` catch-all
  arms that silently do the wrong thing at runtime *are* valid logic bugs
  because they compile cleanly).
- Unused variables, dead code, or compiler/clippy warnings.

If your finding claims the patch "fails to compile", "breaks the build", or
"causes a borrow/type/import error", you have misread the code (or missed a
re-export, macro expansion, trait blanket impl, or deref coercion). Drop the
concern immediately.

### 3. "Add a check for safety"

Do not ask for defensive validation unless you can show all three:
- the value comes from somewhere untrusted (a patch, a webhook, a PR, a model
  response, a config file), **and**
- a concrete path carries it to the code in question, **and**
- the current code demonstrably misbehaves on a value that path can produce.

- Bad: "This should validate the index before use."
- Good: "`selected_prompts` comes from model output, is joined onto the prompt
  root, and reaches `include_file` — a name containing `..` would escape the
  prompt directory."

### 4. `unwrap` and `expect` are not automatically bugs

The project's rule is that they need a proof they cannot panic, and many in
this tree have one. Before reporting, check whether the invariant holds:

- Bad: "`.unwrap()` here can panic."
- Good: "`.unwrap()` here assumes the patchset has at least one patch, but
  `create_patchset` is reachable with an empty `patches` array from
  `/api/submit`."

Also check *where* it is. A panic in the worker subprocess is recovered by the
reviewer and retried; a panic in the daemon's main loop is not. The same
`unwrap` has different severity in different modules.

### 5. Assuming a caller does not handle it

This is the most common way a real analysis turns into a false positive in
reverse. Do not dismiss a defect inside the changed code by assuming the
surrounding system handles it, unless you can point at the specific code that
makes the failure structurally impossible. "The API layer probably validates
this" is not evidence. Go read the API layer.

### 6. Missing error handling that cannot happen

Before reporting an unhandled error, confirm the error is reachable. Many
`Result`-returning helpers in this tree are infallible in practice for a
specific call site. Say why you believe the error can occur.

### 7. Prompt and instruction text

Prompt wording is a legitimate review target — an ambiguous field name or a
missing escape hatch in an enum is a real defect, and `llm-stages.md` explains
why. But do not report stylistic preferences about prompt prose, and do not
report that a prompt "could be clearer" without naming the specific
misinterpretation it permits and what the model would do instead.

### 8. Pre-existing issues

If the problem existed in the codebase before this commit/series was applied,
you MUST mark `preexisting: true` so the workflow routes it exclusively to the
bugs database rather than reporting it alongside new patch findings. Never mark
an issue in unchanged code (or an existing defect merely exposed or moved by a
refactor) as `preexisting: false`. Check the parent revision (`HEAD~1` or
`Baseline SHA`) when in doubt, not just the `+` lines.

### 9. Test code

Tests are held to a different standard than production code. An `unwrap` in a
test is fine. Standard Unix pseudo-devices (such as `/dev/null` or `/dev/ptmx`
for PTY tests) are completely normal in this Unix-only codebase and must NOT be
flagged as filesystem isolation or portability violations. A fixed port, a
shared mutable database file, or a dependency on mutable global state across
concurrent tests is *not* fine — report those when they cause real cross-test
interference.

### 10. Non-Unix / Windows compatibility is a non-goal

Sashiko exclusively targets Linux/Unix operating systems. Never report Windows
or non-Unix portability issues (such as `tokio::signal::unix`, `rustix`,
`/dev/ptmx`, `libc`, POSIX paths, or Unix signals) as findings.

### 11. Unnecessary demands for validation or benchmark data

Do not ask authors to add manual test procedures, validation logs, or benchmark
numbers to commit messages for ordinary code, CLI, UI, or bug-fix commits, nor
for changes to Sashiko's own self-review prompts (`prompts/sashiko/`,
`sashiko_patch_review.rs`) since `benchmarks/` only covers Linux kernel reviews.
Only demand benchmark validation when a change can meaningfully affect overall
Linux AI review quality across the board (`third_party/prompts/`,
`linux_patch_review.rs`, `linux_bug.rs`, generic workflow graph, model
parameters, or shared verification/deduplication logic) — and when such a change
lacks benchmark backing, classify it as **High** severity.

### 12. Patch series false positive removal and design documents

Large changes are split into small, self-contained commits (`Patch 1 of N`,
`Patch 2 of N`, ..., `Patch N of N`) so each logical layer is easier to review:
- Example valid series:
  - `Patch 1`: add foundational types, struct fields, or database helpers
  - `Patch 2`: wire HTTP API endpoints and middleware to use the new helpers
  - `Patch 3`: wire CLI subcommands and integration tests
  - `Patch 4`: add or update architecture/design documentation (`designs/*.md`)

Do not second-guess how a feature is divided across commits in a series:
- **Work completed later in the series is not a bug:** If a candidate concern on
  `Patch k` is simply incomplete wiring, unused helpers, or missing callers/CLI
  integration that are completed in `Patch k+1..N`, inspect the final state of
  the series (`Series End Commit` via `git_read_files` or `git_diff`) and
  discard the concern as a false positive.
- **Design and documentation commits (`designs/*.md`):** Illustrative code
  snippets, pseudo-code, or abbreviated struct definitions in `designs/*.md` or
  `README.md` are documentation, not compiled code. Before reporting that a
  struct or snippet in a design document is missing `#[serde(default)]`, bounds
  validation, or an authorization check, inspect the actual Rust implementation
  in `src/` at the series head (`Series End Commit` / `HEAD`). If the actual
  Rust code enforces the invariant, discard the concern.

## Before you report

For each finding, confirm you can answer all of these. If you cannot answer
one, the finding is speculative: report it, cap it at Medium, and say which
question you could not answer.

1. Which function, in which file, contains the defect?
2. What concrete input or sequence triggers it?
3. What is the observable consequence?
4. What code did you read that rules out the obvious reason this would be safe?
5. If this commit is part of a multi-patch series, does the defect still exist at
   the `Series End Commit` (and in the actual `src/` implementation rather than
   an abbreviated `designs/*.md` snippet)?
