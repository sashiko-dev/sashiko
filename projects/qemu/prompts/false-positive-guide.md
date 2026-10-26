# False Positive Prevention Guide

This guide is used where avoiding false positives matters most. Shift bias away
from fast processing and follow it carefully.

## Core principle

**If you cannot prove an issue exists with concrete evidence from this
codebase, do not report it.**

Evidence means code you have read. Not "device models usually", not "this looks
like it could", not "a caller might". Name the function, name the register or
descriptor field, name the concrete execution path that reaches it.

The corollary matters as much: proving a path is *structurally possible* is
enough. You do not have to prove it executes on every boot. A guest-controlled
index reaching a ring base without a bounds check is a bug even if no known
guest driver sends that index yet.

## Patterns that produce false positives here

### 1. Speculation without a scenario

If you cannot construct a specific input or sequence that triggers the issue,
demote it to `Low` or drop it entirely. Speculation wastes reviewer and author
time. "This size field is not validated" is not a finding until you name the
path along which a guest puts an unvalidatable value into it and the memory
that path then overruns.

### 2. Restating obviously correct code

Clean code does not need praise. Do not report that a hunk "is correct and
well-commented", and do not pad dismissed-concern reasoning with praise. Skip
code you have no comment on.

### 3. Assuming a caller or the core handles it

Do not dismiss a defect inside the changed code by assuming the core device
model, the memory API, or the DMA layer validates it, unless you can point at
the specific code that makes the failure structurally impossible. "The caller
probably checks the length" is not evidence. Go read the caller.

The same rule applies in reverse: do not report a missing check when a specific
piece of code you have read makes the failure impossible.

### 4. Style-only issues

Style that `scripts/checkpatch.pl` flags mechanically (brace style, alignment,
line length) is `Low` at most. Do not escalate style into Medium or above
without a functional consequence.

### 5. Pre-existing issues

If the problem existed in the codebase before this commit/series was applied,
you MUST mark `preexisting: true` so the workflow routes it exclusively to the
bugs database rather than reporting it alongside new patch findings. Never mark
an issue in unchanged code (or an existing defect merely exposed or moved by a
refactor) as `preexisting: false`. Check the parent revision (`HEAD~1` or
`Baseline SHA`) when in doubt, not just the `+` lines.

### 6. Patch series false positive removal

Large changes are split into small, self-contained patches (`[PATCH n/m]`) so
each logical layer is easier to review. Do not second-guess how a feature is
divided across commits in a series:

- **Work completed later in the series is not a bug:** If a candidate concern
  on patch `k` is simply incomplete wiring, unused helpers, or missing callers
  that are completed in patches `k+1..N`, inspect the final state of the series
  (`Series End Commit` via `git_read_files` or `git_diff`) and discard the
  concern as a false positive.

## Before you report

For each finding, confirm you can answer all of these. If you cannot answer
one, the finding is speculative: report it, cap it at Medium, and say which
question you could not answer.

1. Which function, in which file, contains the defect?
2. What concrete input or sequence triggers it?
3. What is the observable consequence?
4. What code did you read that rules out the obvious reason this would be
   safe?
5. If this commit is part of a multi-patch series, does the defect still exist
   at the `Series End Commit`?
