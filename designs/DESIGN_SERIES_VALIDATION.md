# Design Document: Context-Optimized Series Validation

## Problem Statement
During patch verification in Stage 10 (Verification and severity estimation), Sashiko validates candidate concerns against follow-up commits when reviewing patches in a multi-patch series. However, the existing prompt assembly has two issues:

1. **Context Pollution in Single-Patch and Final-Patch Reviews:**
   When reviewing a single patch or the final patch in a series (`series_range == None`), Stage 10 currently injects:
   ```text
   Full Series Context:
   Not applicable (single patch or last patch in series).
   ```
   This placeholder text wastes prompt tokens and clutters the model's context window.

2. **Suboptimal Context for Intermediate Patches:**
   When reviewing an intermediate patch ($P_i$ where $i < N$), the current implementation formats the output of `git log --reverse baseline..end_sha`. This provides the entire series history from the baseline rather than focusing on the *follow-up* commits ($P_{i+1} \dots P_N$), and omits the explicit series tip commit SHA required by tool calls such as `git_diff` and `git_read_files`.

## Proposed Solution

### 1. Dynamic Injection (Zero Boilerplate for Non-Series Reviews)
If `series_range` is `None` (single patch or the final patch of a series):
- Do not inject any series context block into the user prompt.
- The prompt transitions directly from the general verification directives to the conflict-resolved concerns list.

### 2. Structured Follow-Up Context (For Intermediate Patches)
When `series_range` is present (`Some(range)`):
- Extract the series end SHA from the range (`range.split("..").nth(1)`).
- Filter the patch list to identify follow-up commits with `index > current_patch_index`.
- Inject a structured, actionable block:

```text
=== Follow-Up Patches in Series ===
Current Patch: [Patch {i} of {N}] - {current_subject}
Series End SHA (Final State): {series_end_sha}

Subsequent patches in this series:
- [Patch {i+1} of {N}] ({sha_i+1}): {subject_i+1}
...
- [Patch {N} of {N}] ({sha_N}): {subject_N}

SERIES VERIFICATION DIRECTIVE:
Check whether any candidate concern raised against this patch is resolved, refactored, or fixed in the subsequent patches listed above. Use tools (e.g., `git_diff(base_revision="{current_sha}", target_revision="{series_end_sha}")` or `git_read_files(revision="{series_end_sha}")`) to inspect the final code state at the series end. If an issue is resolved by follow-up patches in this series, discard it as a false positive.
==================================
```

## Implementation Plan
1. **Design Document:** Add `designs/DESIGN_SERIES_VALIDATION.md`.
2. **Stage 10 Context Builder:** Update Stage 10 in `src/worker/prompts.rs` to format the follow-up series context dynamically when `self.series_range` is set, and omit it completely when `None`.
3. **Stage 10 Prompt Guidelines:** Refine the Stage 10 prompt instructions in `src/worker/prompts.rs` to be concise.
4. **Unit Tests:** Add unit tests to verify that `series_range == None` produces no series clutter and that `Some(range)` generates the structured follow-up block.
5. **Validation:** Run `make check-pr` to ensure linting, formatting, and all tests pass cleanly.
