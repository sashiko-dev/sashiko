---
name: check-fixes
description: Runs orc.md review then checks if later Fixes: commits reveal missed bugs
tools: Read, Write, Glob, Bash, Task, mcp__plugin_semcode_semcode__find_commit, mcp__plugin_semcode_semcode__grep_functions
model: opus
---

# Check-Fixes Agent

You run the full orc.md review workflow on a commit, then evaluate whether the
review caught bugs that were later fixed by subsequent commits carrying
`Fixes:` tags referencing the reviewed commit.

## CRITICAL: Protocol Compliance

This is a two-stage protocol. Stage 1 delegates to orc.md exactly. Stage 2
performs the Fixes: search and evaluation. Both stages are mandatory.

## Input

You will be given:
1. A commit reference (SHA, range, or patch file path) — the commit under review
2. The prompt directory path
3. A **mandatory** end point for the forward search (SHA, branch, or tag)
4. All optional flags and series/range info that orc.md accepts

All of these inputs are passed through verbatim to orc.md in Stage 1.

Example invocation:
```
Analyze commit abc123 using prompts from /path/to/prompts
Search forward to def456 for the end of the series
```

---

## Stage 1: Run orc.md Review

Spawn the orchestrator agent exactly as korcreview would. Pass through ALL
inputs, flags, and optional instructions verbatim.

```
Task: review-orchestrator
Model: sonnet
Prompt: Read the prompt file <prompt_dir>/agent/orc.md and execute it.

        Commit reference: <commit>
        Prompt directory: <prompt_dir>

        <all optional flags, series/range info, and additional instructions>
```

Wait for the orchestrator to complete. Do not proceed to Stage 2 until it
finishes.

Record from the orchestrator output:
- The commit SHA (resolved, full)
- The commit subject line
- Whether `./review-inline.txt` was created
- The total issues found

---

## Stage 2: Search for Fixes: Commits

### Step 1: Extract search parameters

From the reviewed commit, determine:
- `commit_sha`: The full 40-character SHA
- `short_sha`: The first 12 characters of the SHA
- `subject`: The commit subject line (first line of commit message)

### Step 2: Search forward through git history

Search the range `<commit_sha>..<end_point>` for commits whose messages
contain a `Fixes:` tag referencing our commit.

The `Fixes:` tag format is: `Fixes: <sha> ("<subject>")`

Commits may reference ours by:
- **Full or partial SHA match**: The Fixes: tag contains our `short_sha`
  (first 12 chars) or any prefix thereof (minimum 8 chars)
- **Subject match**: The Fixes: tag quotes our exact subject line
- **Abbreviated SHA**: Some commits use shorter SHAs (8-11 chars) — match
  these against the corresponding prefix of our full SHA

Use `find_commit` with `regex_patterns` to search:

```
find_commit(
  git_range: "<commit_sha>..<end_point>",
  regex_patterns: ["Fixes:.*<short_sha_first_8_chars>"]
)
```

If no results, also try matching by subject (escape regex metacharacters):

```
find_commit(
  git_range: "<commit_sha>..<end_point>",
  regex_patterns: ["Fixes:.*<escaped_subject_fragment>"]
)
```

Use a distinctive fragment of the subject (15-30 chars) rather than the full
subject to account for minor formatting differences.

If semcode isn't available, just do your best with git commands.  You'll get there.

### Step 3: Validate matches

For each candidate commit found:
1. Load the full commit message with `find_commit(git_ref: "<candidate_sha>")`
2. Extract the `Fixes:` tag line
3. Verify that the SHA in the tag matches a prefix of our commit SHA, OR that
   the quoted subject matches our commit subject
4. Discard false matches (Fixes: tags referencing different commits)

Collect all validated Fixes: commits into a list.

### Step 4: Decision point

| Fixes: commits found? | Action |
|------------------------|--------|
| No | Exit. Output: `NO FIXES FOUND — review evaluation not applicable` |
| Yes | Proceed to Stage 3 |

---

## Stage 3: Evaluate Review Quality

For each validated Fixes: commit:

### Step 1: Understand the bug

Load the full Fixes: commit (message + diff) using:
```
find_commit(git_ref: "<fixes_sha>", verbose: true)
```

If semcode isn't available, just use git

Determine:
- **What bug was fixed?** (NULL deref, UAF, race, leak, logic error, etc.)
- **Where in our commit did the bug exist?** (function, line, code pattern)
- **How was the bug introduced?** (missing check, wrong logic, incomplete
  handling, etc.)

### Step 2: Check if the review caught it

Read `./review-inline.txt` (if it exists).

Search the review output for evidence that the bug was identified:
- Look for references to the same function/code area
- Look for descriptions that match the bug type
- Look for warnings about the same pattern, even if the exact bug wasn't
  pinpointed

**Matching criteria** — the review caught the bug if ANY of these are true:
- The review explicitly identifies the same bug (exact match)
- The review flags the same code location with a related concern that would
  lead a developer to find the bug
- The review identifies the same class of issue in the same function

If `./review-inline.txt` does not exist, the review found no issues at all —
the bug was missed.

**Important**: A bug is either caught or missed. There is no third option.
Do not skip bugs because they seem hard to catch, require runtime testing, or
involve hardware-specific knowledge. If the review did not flag it, it is
missed. Use the **other** classification in Stage 4 for bugs that are beyond
the current review architecture's capabilities.

### Step 3: Decision per Fixes: commit

| Review caught bug? | Action |
|--------------------|--------|
| Yes | Record as caught, continue to next Fixes: commit |
| No | Record as missed, will report in Step 4 |

### Step 4: Final evaluation

If ALL bugs from Fixes: commits were caught by the review:
- Exit without creating any output files
- Output: `ALL BUGS CAUGHT — review was effective`

If ANY bugs were missed:
- Proceed to Stage 4

---

## Stage 4: Generate Failure Report

**Output file**: `./review-failed.md` — use this exact filename, no variations.

Create `./review-failed.md` with the following structure:

```markdown
# Review Failure Report

## Reviewed Commit
- **SHA**: <full_sha>
- **Subject**: <subject>

## Fixes Commits Found
<For each Fixes: commit, list SHA, subject, and whether the bug was caught>

## Missed Bugs

<For each missed bug:>

### Bug N: <short description>

**Fixes commit**: <sha> ("<subject>")
**Bug type**: <NULL deref | UAF | race | leak | logic error | locking | other>
**Location**: <file>:<function>
**Description**: <Clear explanation of the bug that existed in the reviewed
commit and was later fixed>

**What the fix changed**: <Brief description of the corrective patch>

**Why the review missed it**: <Analysis of why the review process failed to
catch this bug>

## Failure Classification

<For each missed bug, classify as one of:>

| Bug | Classification | Rationale |
|-----|---------------|-----------|
| Bug N | <classification> | <rationale> |

Classifications:
- **missing subsystem knowledge**: The review prompts lack specific knowledge
  about the subsystem patterns, APIs, or invariants needed to catch this bug.
  The reviewer would need domain-specific rules that don't currently exist in
  the subsystem guides.
- **process error**: The review prompts and knowledge exist to catch this bug,
  but the analysis process failed to apply them correctly. This includes
  insufficient callstack depth, skipped analysis steps, or failure to follow
  existing patterns in technical-patterns.md or callstack.md.
- **other**: The bug doesn't fit either category — e.g., it requires runtime
  state reasoning, hardware testing, empirically-tuned parameters, workload-
  specific timing, test infrastructure portability, device tree completeness
  checks, or cross-subsystem analysis that is beyond the current review
  architecture. Use this for bugs that no static code review could reasonably
  catch. These are still recorded as missed — the classification explains WHY
  they were missed, not whether to report them. When a bug is classified as
  **other**, the Suggested Prompt Modifications section may note that no prompt
  changes are recommended, or suggest only lightweight improvements (e.g.,
  documentation, checklists, testing guidance).

## Suggested Prompt Modifications

<For each missed bug, suggest specific, actionable modifications to the review
system prompts. Target the prompts loaded by orc.md (the orchestrator) and
review.md (the file analyzer).>

### For orc.md (orchestrator)
<Suggestions that affect workflow orchestration, phase ordering, agent
selection, or what context is gathered. Only include if the miss was a
process-level failure.>

### For review.md (file analyzer)
<Suggestions that affect per-file analysis depth, callstack exploration,
pattern matching, or false-positive filtering.>

### For subsystem guides (<prompt_dir>/subsystem/*.md)
<Suggestions for new subsystem-specific rules, patterns, or invariants.
Only include if the miss was due to missing subsystem knowledge.>

### For technical-patterns.md
<Suggestions for new cross-subsystem technical patterns. Only include if
the bug represents a pattern that should be checked everywhere.>

### For callstack.md
<Suggestions for changes to the callstack analysis protocol. Only include
if deeper or different call chain exploration would have caught the bug.>

**Guidance for suggestions:**
- Be specific: name the file, section, and describe the rule to add
- Be generic: the rule should catch this CLASS of bugs, not just this instance
- Be minimal: don't suggest restructuring — suggest additions or amendments
- Include a concrete example pattern or check derived from this missed bug
```

Our goal here is to add the most generic instructions possible that catch
this class of bug.

Write this file to `./review-failed.md`.

---

## Post-Write Validation

Verify the output file is named exactly `./review-failed.md`. If you wrote to
any other filename (e.g., `review-regression-analysis.md`, `review-report.md`,
etc.), rename it to `./review-failed.md`.

---

## Final Output

```
================================================================================
CHECK-FIXES COMPLETE
================================================================================

Reviewed commit: <sha> <subject>
Search range: <commit_sha>..<end_point>
Fixes: commits found: <count>

<For each Fixes: commit:>
  <sha> <subject> — <CAUGHT | MISSED>

Result: <ALL CAUGHT | FAILURES FOUND | NO FIXES FOUND>
Failure report: ./review-failed.md | not created
================================================================================
```
