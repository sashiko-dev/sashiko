# Sashiko Cross-Cutting Pattern: Resource Limits and Bounds

Covers memory, disk, token budget, and I/O bounds across Sashiko: LLM token
budgets (`src/ai/`, `src/toolbox/`), worktree and log storage
(`src/worker/compressor.rs`, `src/worker/repack.rs`), database growth
(`src/db.rs`), and network/IPC channels.

Sashiko runs unattended as a long-lived daemon processing patches and pull
requests. Any unbounded growth in memory, disk, tokens, or file descriptors
will eventually degrade or crash the instance.

---

## 1. LLM Context and Token Budgets

LLM calls are Sashiko's primary cost and latency driver, and every model
enforces a hard context window limit.

### Invariants

1. **Tool outputs must be bounded**: Every tool in `src/toolbox/` (`git_grep`,
   `git_log`, `git_show`, `git_diff`, `git_read_files`, `git_find_files`) must
   cap both the number of returned items (matches, commits, files) and the byte
   length of its output string before returning it to `LlmSession`.
   - Never return an entire multi-megabyte file or an unpaginated `git log`
     into the LLM message history.
2. **UTF-8 safe truncation with explicit markers**: When truncating large diffs,
   commit messages, or tool outputs, truncate on a valid UTF-8 character
   boundary and append a clear marker (e.g., `\n...[truncated]...`) so the
   model knows the content is incomplete rather than assuming the file ends
   there.
3. **Bounded turn counts (`max_turns`)**: Every stage executing tool calls must
   configure a finite `max_turns` in `StagePolicy` (and a finite
   `max_validation_retries`). Infinite agent loops must be impossible by
   construction.

---

## 2. Memory and Unbounded Collections

### Invariants

1. **Prefer bounded channels (`tokio::sync::mpsc::channel(cap)`)**: Avoid
   `mpsc::unbounded_channel` between fast producers (e.g. NNTP/mbox ingestion or
   event emitters) and slower consumers (database writers or review workers).
   Backpressure prevents out-of-memory crashes during ingestion bursts.
2. **Pagination and query limits (`LIMIT`)**: Database queries that list
   patchsets, messages, reviews, or `ai_interactions` for API endpoints or UI
   views must include explicit `LIMIT` clauses (or cursor pagination). Never
   `SELECT *` without a limit on tables that grow continuously over time.
3. **HTTP body and webhook payload limits**: Axum routes accepting JSON,
   webhooks, or patch submissions (`src/api.rs`, `src/forge.rs`) must enforce
   request body size limits (e.g. via `DefaultBodyLimit`) so an oversized payload
   cannot exhaust heap memory.

---

## 3. Disk Space: Worktrees, Archives, and Logs

Sashiko creates high-churn disk artifacts during operation:
- Temporary git worktrees under `review_trees/`
- Raw email archives under `archives/`
- Full LLM interaction transcripts in `ai_interactions` (compressed by
  `src/worker/compressor.rs`)
- Git objects accumulated from fetched patch series or PR refs (`src/worker/repack.rs`)

### Invariants

1. **Deterministic worktree cleanup**: Every `GitWorktree` created for a review
   or baseline check must be removed when the review finishes, fails, or times
   out. Ensure error paths (`?` early returns and panics/timeouts) do not
   bypass cleanup.
2. **Idempotent housekeeping workers**: Background maintenance workers
   (`compressor.rs` for zstd compression of old `ai_interactions` payloads,
   `repack.rs` for git repository maintenance) must run in bounded batches per
   tick so a large backlog does not stall the daemon or hold disk locks
   indefinitely.
3. **Temporary files in tests and CLI**: Any temporary file or directory
   created during unit/integration tests or CLI commands must use RAII guards
   (`tempfile::TempDir` / `NamedTempFile`) or reside inside designated
   scratch/worktree directories that are cleaned up automatically.
