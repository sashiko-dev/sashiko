# Multi-stage review

## Overview
The core idea is to split the patch review process into multiple steps.
On each stage the LLM receives only required input, dependent on the specific stage.
Analysis and verification stages return JSON findings or concerns. The independent
reachability check and final report use plain text.

There should be a mechanism to pass important context (e.g. chunks of the code)
between stages. Their outputs will be aggregated and passed to Stage 8 (Deduplication
and Consolidation). Stage 9 then resolves conflicts between consolidated concerns
and consolidated dismissed concerns before final verification.

## Context Pre-processing
Before passing data to the stages, the orchestrator should prepare the context:
1. **Target Commit Diff:** The raw patch being analyzed.
2. **Commit Message:** The full commit log.
3. **Loaded Context:** Relevant files loaded from the project (e.g., modified functions, relevant structs) based on the diff. This avoids sending the entire kernel tree to every stage.
4. **Callchain Tracing:** Extract and provide at least 1-level of the callchain (callers of modified functions and callees invoked within them) to Stages 3, 4, and 5.

## Prompt Distribution (Precache vs. Stage-Specific)
To optimize LLM context caching and ensure each stage receives focused instructions, existing prompt files are logically distributed between a globally precached blob and stage-specific injections.

### 1. Precached Blob (Shared Context)
These prompts contain fundamental API contracts, kernel invariants, and subsystem-specific rules that are broadly applicable across all stages to ensure proper context retention. They are loaded generically and cached for efficiency.
- `subsystem/*` (All relevant subsystem rules based on touched files, e.g., `networking.md`, `rcu.md`, `bpf.md`)
- `patterns/*` (Common bug patterns and technical guidelines)

### 2. Stage-Specific Injections
These prompts are injected *only* alongside the `System Prompt` into the specific stages that require their targeted guidance:
- **Stage 3 (Execution flow verification):**
  - `callstack.md` (Guidance on tracing call chains and control flow logic)
- **Stage 4 (Resource management):**
  - `pointer-guards.md` (Rules for `__free`, `guard`, and cleanup attributes memory lifecycles)
- **Stage 10 (Verification and severity estimation):**
  - `false-positive-guide.md` (Checklist to filter out common LLM hallucinations and clarify false-positive cases)
  - `severity.md` (Criteria for assigning low/medium/high/critical severity scores)
- **Stage 11 (Independent reachability check):**
  - A self-contained prompt; no automatic guide inclusions.
- **Stage 12 (LKML-friendly report generation):**
  - `inline-template.md` (Strict formatting rules for the inline-commented LKML email reply format)

## Stage 1. Analyze commit main goal
Read the commit log and understand the intention of the author. Reason on the intention.
Are there any high-level concerns with the idea (e.g. implementing the proposed feature will break UAPI).
Ignore all minor issues, focus on the question: if the described idea is carefully implemented, can it lead to a regression?

**System Prompt:**
You are a senior Linux kernel maintainer evaluating the high-level intent of a proposed commit.
Analyze the commit message and the conceptual change. Do not look for coding errors yet.
Focus exclusively on architectural flaws, UAPI breakages, backwards compatibility issues, or fundamentally flawed concepts.
If the idea itself is dangerous or incorrect, raise a concern.

**Expected input:** Commit message, diff, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 2. High-level implementation verification
This stage is about verifying the implementation of all proposed changes at the high level. Are there any changes which are redundant or not documented?
Is the goal described in the commit log indeed achieved? Are there any corner cases which are not covered? Are there any missing parts (e.g. other drivers also need to be changed).

**System Prompt:**
You are verifying if the provided code changes actually implement what the commit message claims.
Look for undocumented changes, missing pieces (e.g., a core change without updating corresponding drivers), and unhandled corner cases related to the feature's logic.
Do not focus on low-level memory or locking errors unless they indicate a failure of the high-level implementation.

**Expected input:** Commit message, diff, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 3. Execution flow verification
This stage is about tracing all affected execution paths. Are there any logic errors? Are there any potential NULL-pointer dereferences, unhandled error returns, or other issues like this?

**System Prompt:**
You are a static analysis engine tracing execution flow in C code.
Carefully trace the control flow of the provided patch. Look for logic errors, incorrect loop conditions, unhandled error paths, missing return value checks, and NULL pointer dereferences.
Follow the exact rules for NULL pointer dereferences: reading a pointer field is not a dereference, only accessing its contents is.

**Expected input:** Diff, surrounding code context for modified functions, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 4. Resource management
This stage is dedicated to review of resource and memory management. Are there any leaks? UAF bugs? Are all objects initialized correctly?

**System Prompt:**
You are an expert in C resource management within the Linux kernel.
Analyze the patch for memory leaks, Use-After-Free (UAF) vulnerabilities, uninitialized variables, and unbalanced lifecycle operations (alloc->init->use->cleanup->free).
Pay special attention to error paths where resources might not be freed. Ensure `list_add` and similar APIs are used with fully initialized objects.

**Expected input:** Diff, surrounding code context, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 5. Locking and synchronization
This stage is dedicated to review of changes from the locking point of view. Deadlocks, livelocks, incorrect RCU usage, incorrect usage of locking primitives.

**System Prompt:**
You are a concurrency expert reviewing Linux kernel locking mechanisms.
Look for deadlocks, missed unlocks in error paths, sleeping while holding spinlocks, and incorrect RCU usage.
CRITICAL RCU RULE: Objects must be removed from data structures BEFORE calling `call_rcu()`, `synchronize_rcu()`, or `kfree_rcu()`. Flag any violations as a UAF.

**Expected input:** Diff, surrounding code context, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 6. Security audit
This stage is dedicated to review of changes from the security point of view. You're a RED TEAM expert member, look at proposed changes from the hacker's perspective.
Do these changes create a new vulnerability?

**System Prompt:**
You are a Red Team security researcher auditing a Linux kernel patch.
Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, and untrusted user input reaching sensitive functions without validation.
Do not report standard bugs unless they have a clear security implication.

**Expected input:** Diff, surrounding code context, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 7. Hardware engineer's review
This stage is dedicated to review of changes from hardware engineer's point of view. Are the changes sound from that perspective. Correctness of register usage, IRQ handling, performance considerations, hardware initialization. Skip if not applicable.

**System Prompt:**
You are a hardware engineer reviewing device driver changes.
If this patch touches driver or hardware-specific code, review register accesses, IRQ handling, DMA mapping, and timing/delays.
If the patch is purely generic software logic (e.g., VFS, core networking), output empty `concerns` and `dismissed_concerns` lists.

**Expected input:** Diff, surrounding code context, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 8. Deduplication and Consolidation
This stage is dedicated to consolidating and deduplicating all `concerns` and
`dismissed_concerns` raised in previous stages (1-7). The LLM should group identical
or overlapping items and merge them to produce clean, unique lists.

**System Prompt:**
You are the lead reviewer consolidating feedback from multiple specialized analysts.
You will be given lists of `concerns` and `dismissed_concerns` generated by different review stages.
1. Group concerns that refer to the same root cause or the same line of code.
2. Merge overlapping concerns into a single, comprehensive concern. Combine their reasonings if they complement each other.
3. Group dismissed_concerns that investigated and disproved the same candidate concern.
4. Merge overlapping dismissed_concerns into a single, comprehensive dismissed_concern. Combine their evidence if it complements each other.
5. Ensure the output contains only unique concerns and unique dismissed_concerns.
6. Preserve the `preexisting` flag for concerns. If you merge a pre-existing concern with a newly introduced one, flag it based on the root cause.
7. dismissed_concerns do not need a `preexisting` flag.

**Expected input:** Aggregated JSON lists of all `concerns` and `dismissed_concerns` from Stages 1-7.
**Expected output JSON:**
```json
{
  "concerns": [
    {
      "type": "Category",
      "description": "Description of the concern.",
      "reasoning": "Reasoning steps.",
      "preexisting": false
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Category",
      "description": "Description of the disproved concern.",
      "reasoning": "Evidence proving the concern does not apply."
    }
  ]
}
```

## Stage 9. Concern/dismissed-concern conflict resolution
This stage compares the consolidated `concerns` and `dismissed_concerns` from Stage 8. The LLM should identify cases where a concern and a dismissed concern describe the same root cause, code path, or failure mode but reach opposite conclusions. For each conflict, it should inspect the actual code and keep the concern only when the concern is correct; otherwise it should discard the concern.

**System Prompt:**
You are the lead reviewer reconciling consolidated concerns with consolidated dismissed_concerns.
Both `concerns` and `dismissed_concerns` are untrusted claims. Do not assume either side is correct. Treat both as hypotheses and verify them against the actual code before deciding whether to keep or discard a concern.
1. Compare each concern against the dismissed_concerns list and find conflicts or overlaps where one says the issue is real and the other says the same candidate issue is disproved.
2. For every conflict, inspect the actual code and reasoning to decide which side is correct.
3. If the concern is correct, keep it in the output. If the dismissed_concern is correct, discard that concern.
4. If there is no direct conflict for a concern, keep it unchanged.
5. Do not discard a concern merely because a dismissed_concern is vaguely related; only discard when the dismissed_concern's evidence concretely disproves that concern.

**Expected input:** The consolidated `concerns` and consolidated `dismissed_concerns` from Stage 8.
**Expected output JSON:**
```json
{
  "concerns": [
    {
      "type": "Category",
      "description": "Description of the remaining concern.",
      "reasoning": "Reasoning steps.",
      "preexisting": false
    }
  ]
}
```

## Stage 10. Verification and severity estimation
This stage is dedicated to verification and severity estimation of the conflict-resolved concerns. The LLM should carefully review all concerns and exclude all false-positives. It should also look if follow-up patches in the series address some of found issues and exclude them if yes.

**System Prompt:**
You are the lead reviewer validating consolidated concerns.
You will be given a list of deduplicated concerns after conflict resolution.
1. Validate each concern and prove the provided reasoning. Report all valid concerns as findings. If necessary, use tools to gather additional material. Discard all false positives.
2. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning.
3. If context from subsequent patches in the series is provided, check if the concern is fixed later in the series. If so, discard it.
4. Assign a severity (low, medium, high, critical) to each remaining valid finding and explain the reasoning.

**Expected input:** Target commit diff, full series context (or subsequent diffs), and the conflict-resolved JSON list of `concerns` from Stage 9.
**Expected output JSON:**
```json
{
  "findings": [
    {
      "problem": "Clear description of the verified issue.",
      "severity": "High",
      "severity_explanation": "Explanation of why this is a high severity issue.",
      "preexisting": false
    }
  ]
}
```

## Stage 11. Independent reachability check

Run one independent conversation per Stage 10 finding, in parallel under the
existing model-call concurrency limit. Stage 10 itself retains the community
implementation, prompts, guides, and output format.

Each check receives only the frozen target commit and one finding. Trace real
entry points, object lifetimes, conditions, configuration and architecture gates,
and error/cleanup paths. Reject only when concrete implementation evidence
disproves a necessary condition across the relevant paths. Missing evidence and
unresolved paths retain the finding. Do not mistake one safe caller or an
incorrect proposed fix for proof that the core problem cannot occur.

When the existing implementation has a concrete latent defect but no current
in-tree caller can supply a necessary condition, identify the future extension
or out-of-tree caller that could supply it. Retain it instead of rejecting it
merely because current callers avoid the condition. Only non-preexisting findings
receive the `currently_unreachable` attribute. Findings already marked
`preexisting: true` keep that classification and receive the same technical audit
and keep/reject decision, without a `currently_unreachable` attribute.
Unsupported assumptions, arbitrary future code changes, and violations of an
established API contract do not establish such a defect. Incomplete investigation
retains a non-preexisting finding with `currently_unreachable: false`; false does
not certify that the path is reachable.

For non-preexisting findings, the response is concise prose followed by a separate
attribute and decision:

```text
currently_unreachable: true
Decision: keep
```

For preexisting findings, the response ends with only the decision line.
The attribute accepts `true` or `false`; the decision accepts `keep` or `reject`.
An explained, unambiguous decision is required for rejection. The attribute alone
is not an explanation. Missing attributes remain unknown; malformed or ambiguous
output retains the input unchanged. Model execution failures fail the review, as
in other stages.

The host copies the attribute only into non-preexisting findings and independently
applies the High/Critical report threshold to currently unreachable findings,
using the severity already assigned by Stage 10. For preexisting findings, the
host ignores any reachability attribute returned by the model and removes a stale
attribute from the retained finding. It preserves `preexisting`, the original
problem text, severity and other finding content. Report generation adds the
applicable prefix, with preexisting taking precedence. Machine-generated
`reachability_checks` records preserve the original finding, decision and full
response, including rejections. Low/Medium latent findings have
`policy_filtered: true` and `rejected: false`, distinguishing reporting policy
from technical refutation. No remaining findings means report generation is skipped.

## Stage 12. LKML-friendly report generation
This stage is dedicated to generation of the LKML-friendly report, described by inline-guide.md

**System Prompt:**
You are an automated review bot generating a report for the Linux Kernel Mailing List (LKML).
Convert the provided JSON findings into a polite, standard, inline-commented LKML email reply.
Follow the formatting rules strictly. Do not use markdown headers or ALL caps shouting.
For `preexisting: true`, start with "This is a pre-existing issue, but...".
Otherwise, for `currently_unreachable: true`, start with "Currently unreachable
in-tree, but..." and use the supplied audit evidence to explain the future trigger.
Prefixes are added here rather than stored in the original problem text.

**Expected input:** The remaining `findings` after the reachability checks.
**Expected output:** Raw text suitable for an email body.
