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
explanation of *what* and *why* on non-trivial code changes, genuinely
unwrapped prose lines exceeding ~85 characters, backticks quoting code/symbols
in the commit message, or internal metadata tags (`TAG=`, `CONV=`) — are valid
findings and must NOT be dismissed as linter issues. However, do NOT nitpick
commit message lines that are 73-80 characters long or lines that exceed the
margin for reasonable reasons (quoting code, compiler/log output, URLs, or file
paths), and do NOT demand additional rationale for trivial or self-explanatory
commits (such as adding a mailing list to track, adding an email policy entry
or subsystem git remote, simple configuration/deployment updates, or typo fixes)
where a brief one-sentence description is already self-sufficient.

- Bad: "Consider using `iter()` instead of `into_iter()` here."
- Bad: "This `clone()` looks unnecessary."
- Bad: "The commit message body has lines that are 74 characters long."
- Bad: "The commit message adds a mailing list to track without explaining why."
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

### 5. Assuming a caller does not handle it (Symmetrical Proof Bar)

This is the most common way a real analysis turns into a false positive in
reverse. Do not dismiss a defect inside the changed code by assuming the
surrounding system handles it, unless you can point at the specific code that
makes the failure structurally impossible across **all** callers and entry
points (`git_grep` across CLI, daemon, worker, HTTP API, and webhook paths).
"The API layer probably validates this" or "caller X checks it" is not
evidence if caller Y does not. Likewise, never dismiss an unpaired
lifecycle/state transition (such as claiming a worktree, task, or outbox row
without a matching release/terminal status on error or cancellation), a
swallowed error, or overwritten state by rationalizing the side effect as a
"harmless no-op". Go read all callers and quote the disproving code in
`locations`.

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

Set `preexisting: false` whenever the patch introduces the bug or newly
exposes, triggers, or makes reachable a latent defect that could not be
triggered before the patch. Set `preexisting: true` when the problem already
existed at `Baseline SHA` (before the commit/series) and was already
triggerable before the patch—even if it lives in the same function,
caller/callee, or error path modified by the patch without fixing it—so the
workflow routes it to the pre-existing bug tracker rather than reporting it as
a new patch regression. Never dismiss a real bug into `dismissed_concerns`
merely because it already existed before the patch. Check the baseline revision
(`HEAD~1` or `Baseline SHA`) when in doubt, not just the `+` lines.

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

### 11. Unnecessary demands for validation or `benchmarks/` file diffs

Do not ask authors to add manual test procedures, validation logs, or benchmark
numbers to commit messages for ordinary code, CLI, UI, or bug-fix commits, nor
for changes to Sashiko's own self-review prompts (`projects/sashiko/prompts/`,
`sashiko_patch_review.rs`), nor for subsystem maintainer notes
(`projects/linux/prompts/maintainers/*.md`) that record a subsystem's subjective
coding preferences, conventions, or narrowly-scoped policies, nor for generated
code-notes snapshots (`projects/*/prompts/notes/<set>/`), nor for post-triage
maintenance tasks (such as periodic upstream fix checks on already-triaged open
bugs) or filtering pre-existing issues out of patch review reports that are not
exercised by the patch-review or initial bug-discovery benchmark suites
(`benchmarks/`).
Furthermore, **never** report that a prompt or workflow commit failed to add or
modify test cases in the `benchmarks/` directory: `benchmarks/*.json` are static
ground-truth datasets of known positive kernel bugs (`BenchmarkEntry` in
`src/benchmark.rs`) and do not support negative/false-positive test cases or
per-commit test additions. For targeted false-positive refinements in
`projects/linux/prompts/`, local re-review results on the affected Linux
patches/series reported in the commit message are sufficient validation. Only
flag missing validation (as **High** severity) when a change that can
meaningfully affect overall Linux AI patch-review or initial bug-discovery
quality across the board (`projects/linux/prompts/*.md`, `general.md`,
`linux_patch_review.rs`, `linux_bug.rs`, generic workflow graph, model
parameters, or shared verification/deduplication logic) provides neither
benchmark results nor targeted patch re-review validation in its commit message.
(Note: over-broad suppression rules, volatile code facts hardcoded as policy, or
overly generic `paths`/`symbols` triggers in `projects/linux/prompts/maintainers/*.md`
*are* valid defects and should be flagged directly per `subsystem/notes.md`.)

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

### 13. Asserting what a dependency does

The review tools read this repository. They do not read the source of the
crates named in `Cargo.lock`. A statement about what happens inside an external
crate (`lettre`, `tokio`, `axum`, `reqwest`, `libsql`, or any other) is
something you recall, not something you read, so it is not evidence under the
core principle.

"Do not give code the benefit of the doubt" does not reverse this. That rule
covers code in this repository, which you can read. Being unable to read a
crate is not evidence that the crate lacks a safeguard, and "I have no evidence
otherwise, so I must assume it is unsafe" is a guess.

When a concern turns on how a crate behaves:

- Look in this repository for evidence of that behavior: a test assertion that
  pins it, a comment or commit message that states it, or a caller that already
  relies on it at the `Baseline SHA`. A test assertion is concrete evidence.
- If the patch states the crate's behavior and nothing you have read
  contradicts it, do not assert the opposite.
- If the question stays open, the finding is speculative. Cap it at Medium,
  word the crate's behavior as the unverified assumption ("if `lettre` does not
  ..."), and name what would settle it. Never state "crate X does not do Y" as
  fact.
- If the patch only moves an existing call into a new function, the crate's
  behavior at that call did not change. Apply pattern 8.

- Bad: "`lettre` does not insert a `--` separator, so a recipient that begins
  with a hyphen is parsed as an option." (Nothing read supports the first
  clause.)
- Bad: "`lettre` does not set `kill_on_drop(true)`, so the timeout leaks the
  child process."
- Good: "Speculative: the timeout relies on `lettre` killing the child when the
  future is dropped. The comment says it does; no test in this patch shows it.
  A test with a stub that never exits would settle it."

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
