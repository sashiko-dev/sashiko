# Design: Review Prompts Integration

## Goal
Integrate the `masoncl/review-prompts` repository into `sashiko-review`. The worker must dynamically select and apply specific prompt modules based on the code being reviewed (e.g., specific subsystem rules, language-specific guidelines) and allow the LLM to access this knowledge base.

## 1. Prompt Repository Structure (Assumed/Standardized)
We expect the external repository to follow a hierarchical structure:

```text
review-prompts/
├── core/
│   ├── identity.md          # "You are a Linux Kernel Maintainer..."
│   └── review_workflow.md   # Steps to follow (Check patch, check styles...)
├── subsystems/
│   ├── net/
│   │   ├── common.md        # General networking rules
│   │   └── bpf.md           # BPF specific rules
│   ├── drivers/
│   │   └── gpu.md
│   └── ...
├── languages/
│   ├── rust.md              # Rust-for-linux guidelines
│   └── c.md
└── checklist.md             # Final verification list
```

## 2. Configuration
The `sashiko-review` binary will accept a path to this repository:
-   Flag: `--prompts <PATH>`
-   Env: `SASHIKO_PROMPTS_DIR`
-   Default: `./review-prompts`

## 3. Dynamic Prompt Resolution (`src/worker/prompts.rs`)

### A. Context Mapping
The worker will analyze the `Patchset` to generate a `ContextProfile`:
-   **Touched Paths**: List of all modified files.
-   **Detected Subsystems**: (e.g., if `net/core/dev.c` is touched -> `net`).
-   **Detected Languages**: (e.g., `.rs` -> `Rust`, `.c` -> `C`).

### B. Prompt Selection Strategy (Refined)
1.  **Core**: Always load `review-core.md` and `technical-patterns.md`.
2.  **Subsystem Deltas (Conditional)**:
    -   **Networking**: If path contains `net/`, `drivers/net`, or content has `skb_`, `sockets` -> `networking.md`.
    -   **Memory Management**: If path contains `mm/` or content has `page_`, `folio_`, `kmalloc`, `kmem_cache_`, `vmalloc`, `alloc_pages`, `__GFP_` -> `mm.md`.
    -   **VFS**: If path contains `fs/` (except specific ones), content has `inode`, `dentry`, `vfs_` -> `vfs.md`.
    -   **Locking**: If content has `spin_lock*`, `mutex_*` -> `locking.md`.
    -   **Scheduler**: If path contains `kernel/sched/` or content has `sched_`, `schedule` -> `scheduler.md`.
    -   **BPF**: If path contains `kernel/bpf/` or content has `bpf`, `verifier` -> `bpf.md`.
    -   **RCU**: If content has `rcu*`, `call_rcu` -> `rcu.md`.
    -   **Cleanup**: If content has `__free`, `guard(`, `scoped_guard`, etc. -> `cleanup.md`.
    -   **Other specific modules**: `btrfs.md`, `dax.md`, `block.md`, `nfsd.md`, `io_uring.md`, `fscrypt.md`, `tracing.md`, `workqueue.md`, `syscall.md`.
3.  **Mandatory Patterns**: For any non-trivial patch, load `patterns/CS-001.md`.
4.  **Verification**: Load `false-positive-guide.md` before finalizing.
5.  **Reporting**: Load `inline-template.md` for formatting.


### C. Injection Mechanism
Instead of one giant prompt, we will structure the chat request:
1.  **System**: `core/identity.md` + `core/review_workflow.md`
2.  **User (Context)**:
    > "I am reviewing a patchset affecting the following subsystems: `net`, `bpf`.
    > The following specific guidelines apply:"
    > [Inject content of `subsystems/net/common.md`]
    > [Inject content of `subsystems/net/bpf.md`]
3.  **User (Task)**: "Here is the patch content..."

## 4. Tool Access (Read-Only)
To allow the LLM to "consult" the manual if the context is too large or if it needs to verify a specific rule:
-   **Tool**: `read_prompt(path: str)`
    -   Validates `path` is within `SASHIKO_PROMPTS_DIR`.
    -   Returns content of the markdown file.
-   **Tool**: `list_guidelines()`
    -   Returns a directory listing of available prompt modules.

## 5. Implementation Plan (Addendum)

### Update `src/worker/mod.rs`
-   Add `PromptRegistry` struct.
-   Method `load_context(patchset) -> String`.

### Update `src/bin/review.rs`
-   Check for `review-prompts` directory existence on startup.
-   Error if missing (since we rely on it).

### Update `src/worker/tools.rs`
-   Add `PromptTool` to the toolbox.
