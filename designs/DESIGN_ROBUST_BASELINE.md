# Design: Robust Baseline Fallback Strategy

## Goal
Maximize the patch application success rate by attempting multiple git baselines in a prioritized order. Instead of a single "best guess", the system will treat baseline selection as a series of experiments, stopping at the first successful application.

## Strategy: Cascading Fallback

We will define a priority list of baselines to try for each patchset:

1.  **Explicit Base Commit** (Highest Priority): If the patchset contains a `base-commit: <hash>` tag, this is the exact commit the author worked on. Using this guarantees context matching.
2.  **Subsystem Tree** (Smart Detection): The specific subsystem tree derived from `MAINTAINERS` (e.g., `bpf-next`, `net-next`). This is the likely integration target.
3.  **Linux Next** (Broad Fallback): `linux-next` aggregates most subsystem trees. If the specific subsystem tree is missing or incorrect, `linux-next` often has the required context.
4.  **Mainline Stable** (Safe Fallback): The latest stable tag in Linus's tree (e.g., `v6.12`). This provides a known stable point, useful if development trees are broken or too divergent.

## Architecture Changes

### 1. `BaselineRegistry` (`src/baseline.rs`)

Refactor `resolve_baseline` to `resolve_candidates` which returns a `Vec<BaselineResolution>`.

**Logic:**
- Input: `files`, `subject`, `body` (for base-commit).
- Output: Ordered list of unique candidates.

**Candidate Generation:**
1.  **Check `body`** for `base-commit: [0-9a-f]{40}`. If found -> `BaselineResolution::Commit(hash)`.
2.  **Run Heuristic** (existing logic) -> `BaselineResolution::RemoteTarget` (Subsystem).
3.  **Add Linux Next**: Hardcoded URL `https://git.kernel.org/pub/scm/linux/kernel/git/next/linux-next.git`.
4.  **Add Mainline Tag**: Detect local `origin/master` (or equivalent) and find latest tag via `git describe`.

### 2. `Reviewer` Service (`src/reviewer.rs`)

Update the processing loop to iterate through candidates.

**Execution Flow:**
```rust
let candidates = registry.resolve_candidates(...);

for candidate in candidates {
    // 1. Prepare Baseline
    let baseline_ref = match candidate {
        Commit(h) => h,
        LocalRef(r) => r,
        RemoteTarget(u, n) => ensure_remote(..., u) ? format!("{}/master", n) : continue,
    };

    // 2. Run Experiment
    // Record start in DB (provider, model, baseline).
    let result = run_review_tool(..., baseline_ref);
    
    // 3. Evaluate
    if result == "Applied" {
        // Success! Stop trying other candidates.
        update_patchset_status("Applied");
        break;
    } else {
        // Failed. Log result and continue to next candidate.
        // Update patchset status to "Failed" only if ALL candidates fail.
    }
}
```

### 3. Git Operations (`src/git_ops.rs`)

-   Add `get_latest_tag(repo_path) -> Result<String>`: Runs `git describe --tags --abbrev=0 origin/master` (or heuristic to find mainline remote).

## Data Model
No schema changes required. The `reviews` table already supports multiple entries per patchset. Each attempt will be recorded as a separate review row with its specific `baseline_id` and `result_description`.

## User Interface
The frontend already lists all reviews. It will now show a history like:
1.  Gemini/gemini-3-pro | net-next/master | Failed [Fetch error]
2.  Gemini/gemini-3-pro | linux-next/master | Failed [Patch conflict]
3.  Gemini/gemini-3-pro | v6.12 | Applied

This provides full transparency into the retry logic.
