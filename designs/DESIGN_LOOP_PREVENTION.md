# Design: Loop Prevention and Resource Budgeting

## 1. Problem Statement
The current worker implementation relies on a simple, hard-coded loop limit (`MAX_TURNS = 10`). This is insufficient for complex tasks like "deep dive regression analysis," which may require:
- Exploring multiple files in the kernel tree.
- Tracing function calls across different modules.
- Reading large patch histories.

However, simply increasing `MAX_TURNS` risks:
1.  **Infinite Loops**: The worker getting stuck repeating the same action (e.g., `ls` -> `ls` -> `ls`).
2.  **Cost Overruns**: Excessive token usage if the worker "hallucinates" a never-ending investigation.
3.  **Context Saturation**: Filling the context window with irrelevant data, degrading model performance.

We need a "smart" execution model that allows for deep investigation (high step count) while strictly preventing non-productive loops and resource waste.

## 2. Proposed Solution: Resource-Aware Execution Orchestrator

Instead of a simple `while turns < MAX` loop, we introduce a `Budget` system and a `TurnOrchestrator`.

### 2.1. The `Budget` Structure
The budget defines the upper bounds for a session. It is configurable per-task (e.g., a "Quick Check" vs. "Deep Audit").

```rust
pub struct Budget {
    pub max_turns: usize,           // e.g., 50 for deep dives
    pub max_total_tokens: u32,      // e.g., 1,000,000
    pub max_stalled_turns: usize,   // Max turns without "progress" (see below)
    pub max_time_seconds: u64,      // Wall-clock timeout
}
```

### 2.2. Loop Detection & "Stall" Logic

We must distinguish between **productive iteration** (reading file A, then file B, then file C) and **unproductive looping** (reading file A, then file A, then file A).

**Mechanism: Action History Hashing**
We maintain a sliding window of the last $N$ tool calls (Name + Args).
- **Exact Repetition**: If `Tool(Args)` is called identical to the immediate previous turn, it's blocked (unless it's a pagination request).
- **Cyclic Repetition**: If the sequence `A -> B -> A -> B` is detected, we intervene.

**Mechanism: Progress Heuristics**
The worker must generate "Findings" or "New Information".
- If the worker performs 5 tool calls but the "Findings" list hasn't grown (or the internal "Thought" trace doesn't indicate a new hypothesis), we increment a `stalled_turns` counter.
- If `stalled_turns > max_stalled_turns`, we inject a **System Probe**.

### 2.3. System Probe (The "Are you stuck?" Check)
When a potential stall or loop is detected, instead of killing the process, the System (Orchestrator) injects a high-priority user message:

> "System Alert: You have performed 5 actions without reporting new findings. You recently read 'mm/mmap.c' twice.
> 1. Briefly state what you are looking for.
> 2. If you are stuck, call the `terminate` tool with a report of what you found so far.
> 3. Otherwise, propose a DIFFERENT approach."

### 2.4. Smart Tools to Reduce Steps
One major cause of high step counts is inefficient tools. We will add "Macro-Tools" that combine operations.

1.  **`search_code` (ripgrep)**: Instead of `list_dir` + `read_file` loops to find a symbol, the worker can grep the entire tree in 1 step.
    *   *Constraint*: Must return strictly limited output (e.g., top 20 matches) to prevent context flooding.
2.  **`glob_find`**: To locate files without walking the tree manually.
3.  **`batch_read`**: Allow `read_file` to accept a list of paths, returning a JSON map of contents. This allows reading 5 relevant files in 1 turn instead of 5 turns.

## 3. Implementation Plan

### 3.1. Modify `Worker` Struct
Refactor `src/worker/mod.rs`:

```rust
pub struct Worker {
    // ... existing fields
    budget: Budget,
    stats: SessionStats,
    action_history: Vec<ToolCallHash>,
}
```

### 3.2. Implement `TurnOrchestrator`
The main loop in `run()` will change from a simple `loop` to a managed state machine:

1.  **Pre-Turn Check**:
    *   Check `budget.max_turns`, `budget.max_total_tokens`.
    *   Check `action_history` for cycles.
2.  **Execution**:
    *   Call LLM.
    *   Parse Tool Calls.
3.  **Post-Turn Validation**:
    *   If `ToolCall` is a duplicate of previous:
        *   **Soft Block**: Don't execute. Return Tool Output: "Error: You just performed this exact action. Please change parameters or move to the next step."
    *   Update `budget` usage.

### 3.3. New Tools
Add to `src/worker/tools.rs`:
- `search_file_content` (wraps `rg` or `grep`).
- `find_files` (wraps `find` or `glob`).

## 4. Usage Examples

### Scenario A: The "Grep" Loop
*Bad Worker*:
1. `list_dir src/`
2. `read_file src/main.rs` (No match)
3. `read_file src/lib.rs` (No match)
... (20 steps) ...

*Smart Worker*:
1. `search_file_content "loops"` -> Returns matches in `src/worker/mod.rs` and `DESIGN_LOOP_PREVENTION.md`.
2. `read_file src/worker/mod.rs`
3. Done. (3 steps)

### Scenario B: The "Stuck" Worker
*Worker*:
1. `git_diff HEAD`
2. `git_diff HEAD` (Accidental repeat)
*Orchestrator*:
- Intercepts step 2.
- Returns Tool Error: "Duplicate action detected. You already checked HEAD diffs."
*Worker*:
3. `git_blame ...` (Corrects course)

## 5. Summary
By combining **efficient tools** (search/glob) with **budget enforcement** and **repetition detection**, we can safely increase the step limit (e.g., to 50) to allow for deep analysis without risking infinite loops or resource exhaustion.
