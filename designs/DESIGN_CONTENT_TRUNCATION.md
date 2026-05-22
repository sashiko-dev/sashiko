# Design: Content Truncation & Token Management

## Overview
This document defines the strategy for managing context windows within `sashiko`'s LLM review workflow. Since the Linux kernel code base and patch sets can be massive, effective truncation is critical to preventing token limit errors while preserving the information necessary for high-quality reviews.

## 1. Context Budgeting Strategy
We view the context window (e.g., 32k, 1M, 2M tokens) as a **strict budget** that must be allocated dynamically.

### Allocation Tiers
1.  **System Prompts & Persona** (Immutable, High Priority)
    *   *Allocation*: Fixed (~1k - 2k tokens).
    *   *Policy*: Never truncate. Critical for agent behavior.
2.  **Patch Metadata & Commit Message** (High Priority)
    *   *Allocation*: Variable (~500 - 2k tokens).
    *   *Policy*: Truncate very long commit logs only if absolutely necessary, keeping the subject and sign-offs.
3.  **Active Patch Diffs** (High Priority)
    *   *Allocation*: Up to 40% of remaining budget.
    *   *Policy*: The code *being changed* is the most important. If a patch is too large (e.g., 5k lines), we may need to review it in chunks or reject it as "too large to review atomically".
4.  **Surrounding Context (File Content)** (Medium Priority)
    *   *Allocation*: Remaining budget (Elastic).
    *   *Policy*: This is the primary target for truncation. We use "Smart Pruning" to keep relevant definitions while dropping implementation details of unrelated functions.
5.  **Previous Conversation/Tool Output** (Low Priority)
    *   *Allocation*: Rolling window.
    *   *Policy*: Aggressively summarize or drop old tool outputs.

## 2. Truncation Algorithms

### A. The "Key-Hole" View (For Source Files)
When the agent requests `read_files`, we rarely want the *entire* file if it's 10k lines long.
*   **Default**: Return +/- 50 lines around the requested area (if line numbers specified).
*   **Symbol-Aware Pruning**:
    *   Parse the AST (or use regex/`tree-sitter` for C/Rust).
    *   Keep: Function signatures, struct definitions, global variables.
    *   Collapse: Bodies of functions *not* modified by the patch.
    *   *Visual*:
        ```c
        struct task_struct { ... }; // Kept
        
        void scheduler_tick(void) {
           // ... [150 lines collapsed] ...
        }
        
        // This function is modified by the patch, so we keep the body
        void __schedule(bool preempt) {
           // ... full content ...
        }
        ```

### B. Diff Truncation
If a patch is huge (e.g., automated refactoring):
*   **Head/Tail Truncation**: Keep the first N chunks and the last M chunks.
*   **File Filtering**: Prioritize `.c` and `.h` files over documentation or generated files.
*   **Warning**: Inject a system note: `[System: Context truncated. This patch contains 50 files; showing first 10.]`

### C. Log/Output Truncation
For tool outputs (e.g., `grep` results, build logs):
*   **Start + End**: Keep the first 20 lines (command + headers) and the last 50 lines (errors/summary).
*   **Match Limits**: `grep` should default to `head -n 20` to preventing flooding.

## 3. Implementation Specification

### `TokenCounter` Utility
A shared utility to estimate usage. Since exact tokenizer logic varies, we use a conservative heuristic (e.g., 1 token ≈ 4 chars).

```rust
struct TokenBudget {
    max_tokens: usize,
    current: usize,
}

impl TokenBudget {
    fn remaining(&self) -> usize { ... }
    fn can_afford(&self, text: &str) -> bool { ... }
}
```

### `ContextWindow` Struct
Manages the message history and strictly enforces limits before sending to the API.

```rust
struct ContextWindow {
    system_prompt: String,
    messages: Vec<Message>,
}

impl ContextWindow {
    fn add_message(&mut self, msg: Message) {
        // If adding msg exceeds limit:
        // 1. Try to prune 'old' tool outputs.
        // 2. Try to summarize older user messages.
        // 3. Error if still impossible.
    }
}
```

### Smart File Reader (Tool Enhancement)
Update the `read_files` tool to support "smart mode":
*   `read_files(files=[{path, start_line, end_line}], mode="smart")`
*   If `mode="smart"`, it parses the file and collapses irrelevant sections before returning.

## 4. Integration Plan

1.  **Step 1**: Implement `src/ai/token_budget.rs` for basic counting.
2.  **Step 2**: Create `src/ai/truncator.rs` with `truncate_file_content` and `truncate_diff` functions.
3.  **Step 3**: Integrate into `src/agent/` to sanitize inputs before API calls.
4.  **Step 4**: (Future) Add AST-based pruning for C/Rust files.

## 5. Security & Safety
*   **Never** truncate the System Prompt (Instruction Injection risk if rules are dropped).
*   **Always** indicate truncation occurred (e.g., `... [truncated] ...`) so the model doesn't assume the file is naturally empty.
