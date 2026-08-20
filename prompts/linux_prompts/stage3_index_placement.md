# Stage 3: Index and Placement Verification

You are verifying that proposed prompt modifications adhere to Sashiko's prompt repository architecture, are placed in the correct top-level directory (`api/`, `subsystems/`, or `generic/`), and are accurately registered in the root `index.md` file if necessary.

## Prompt Repository Structure (3 Top-Level Folders + `index.md`)

Sashiko organizes kernel review prompts into three distinct top-level directories:

1. **`api/` (Widely Used Kernel API Guidelines)**:
   - Contains prompts with facts, contracts, and usage rules for widely used kernel APIs and primitives.
   - Target audience: Callers and users of the API across any kernel subsystem (e.g., drivers, filesystems, networking).
   - Examples:
     - How to properly use locking primitives (e.g. `mutex_lock`, `spin_lock_irqsave`, `guard(mutex)`).
     - How to use memory allocation APIs (`kmalloc`, `vmalloc`, `GFP_*` flags).
     - How to use RCU reader/writer APIs (`rcu_read_lock`, `synchronize_rcu`, `call_rcu`).
     - How to use workqueues, timers, refcounts (`kref`), and cleanup attributes (`__free`).
   - Placement Rule: Any prompt describing API usage rules or invariants for callers MUST be placed in `api/`.

2. **`subsystems/` (Subsystem Internals & Implementation Details)**:
   - Contains prompts detailing subsystem-internal implementation mechanics, invariants, and architecture.
   - Target audience: Reviewing changes *inside* the subsystem implementation itself (which general API callers do not need).
   - Examples:
     - How locking primitives are internally implemented inside the locking subsystem (`kernel/locking/`).
     - Memory management page-table walking or slab allocator internals (`mm/`).
     - BPF verifier state machine and instruction rewriting internals (`kernel/bpf/`).
     - Network stack packet scheduling and driver interface internals (`net/core/`).
   - Placement Rule: Prompts focusing on subsystem internals MUST be placed in `subsystems/`, keeping caller guidelines cleanly separated in `api/`.

3. **`generic/` (Cross-Cutting Prompts & Review Policies)**:
   - Contains high-level, cross-cutting prompts that define reviewer behavior, assessment policies, and formatting.
   - Examples:
     - Output format specifications and reporting templates (`generic/inline-template.md`, `generic/report-template.md`).
     - Severity assessment definitions and calibration guidelines (`generic/severity.md`).
     - False positive handling guides (`generic/false-positive-guide.md`).
     - Core review workflow and analysis checklists (`generic/review-core.md`).
   - Placement Rule: General formatting, severity, workflow, or universal pattern prompts MUST be placed in `generic/`.

## Index Reflection Requirements (`index.md`)

1. **Root `index.md` Registration**:
   - Every prompt file in `api/` or `subsystems/` that should be conditionally loaded based on patch contents MUST be registered in `index.md`.
   - Each entry in `index.md` must specify:
     - Category / Name
     - Trigger patterns (directory paths, function prefixes, macro names, regexes)
     - Relative prompt file path (e.g. `api/locking.md` or `subsystems/locking.md`)

2. **New Prompt Files**:
   - If a prompt change introduces a new file in `api/` or `subsystems/`, it MUST include an update to `index.md` adding a corresponding trigger row.

3. **Trigger Updates for Modified Prompts**:
   - If a prompt modification introduces rules for new APIs or symbols, verify whether `index.md` requires updated trigger keywords to ensure the prompt is loaded when relevant kernel patches are reviewed.

4. **No Misplacement**:
   - Raise a concern if API caller rules are placed in `subsystems/`, if subsystem internal details are placed in `api/`, or if formatting/severity policies are placed outside `generic/`.

## Output Format

Return ONLY a JSON object with `concerns` and `dismissed_concerns` arrays:
```json
{
  "concerns": [
    {
      "type": "Index / Placement Issue",
      "description": "Incorrect folder placement or missing index.md registration",
      "reasoning": "Detailed explanation of why the prompt belongs in api/, subsystems/, or generic/, or what entry in index.md is missing.",
      "locations": [
        {
          "file": "prompts/linux/subsystems/locking_api.md",
          "line": 1,
          "code_snippet": "prompt content explaining mutex usage for callers",
          "why_this_location_matters": "API usage rules for callers belong under api/locking.md, not subsystems/"
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Index / Placement Issue",
      "description": "Investigated folder placement and index registration confirmed valid",
      "reasoning": "Why the file placement and index.md entry are correct.",
      "locations": []
    }
  ]
}
```
