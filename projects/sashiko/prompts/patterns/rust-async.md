# Sashiko Cross-Cutting Pattern: Async Rust and Tokio

Covers async execution across `src/main.rs`, `src/worker/`, `src/reviewer.rs`,
`src/ingestor.rs`, `src/fetcher.rs`, `src/ai/`, and `src/git_ops.rs`.

Sashiko runs on a multi-threaded Tokio runtime. Background workers, the HTTP
API server (`axum`), LLM session loops, and subprocess invocations share the
same executor pool. A bug in async discipline does not just slow one request:
it can starve the runtime, deadlock database access, or leave zombie worker
processes holding git worktree locks.

---

## 1. Blocking I/O on Tokio worker threads

Tokio's cooperative scheduler assumes tasks yield rapidly between `.await`
points. Blocking a worker thread stalls other tasks scheduled on that thread,
including HTTP health checks and LLM timeout timers.

### What counts as blocking in Sashiko

- **Synchronous `git` CLI calls**: `std::process::Command::output()` or
  `.status()` blocks until the child exits. `git clone`, `git fetch`, `git am`,
  `git repack`, and `git gc` on large repositories can block for seconds or
  minutes.
- **Synchronous filesystem walks and heavy I/O**: recursive directory scans
  (`walkdir` or `std::fs::read_dir`), reading large patch archives, or
  compressing logs (`zstd` in `src/worker/compressor.rs`).
- **CPU-bound parsing or hashing**: computing content digests over large trees
  or parsing multi-megabyte mbox archives synchronously.

### Invariants

1. **Subprocesses in async functions must use `tokio::process::Command`** (or be
   explicitly wrapped in `tokio::task::spawn_blocking` when calling synchronous
   helpers in `src/git_ops.rs`).
2. **Never call `std::thread::sleep` inside an `async fn`**. Always use
   `tokio::time::sleep`.
3. **Wrap unavoidable synchronous libraries or heavy `std::fs` work in
   `tokio::task::spawn_blocking`**, and `.await` the resulting `JoinHandle`.

---

## 2. Child process management and pipe deadlocks

Sashiko spawns external processes in two critical paths:
- `git` commands in `src/git_ops.rs` and `src/toolbox/`
- Isolated review worker subprocesses in `src/reviewer.rs` (`sashiko review`)

### Invariants

1. **`kill_on_drop(true)` on long-running child processes**: When spawning an
   async child (`tokio::process::Command`), set `.kill_on_drop(true)` whenever
   the parent task may be cancelled by a timeout (`tokio::time::timeout`) or
   `select!`. Without `kill_on_drop(true)`, dropping the `Child` future leaves
   the OS process running in the background, holding worktree locks or
   consuming CPU/API quota.
2. **Pipe buffer deadlocks**: If a child process writes more than the OS pipe
   buffer (typically 64 KB on Linux) to `stdout` or `stderr`, calling
   `child.wait().await` *before* draining `stdout` and `stderr` deadlocks: the
   child blocks waiting for the parent to read the pipe, and the parent blocks
   waiting for the child to exit.
   - Always use `child.wait_with_output().await`, or read `stdout` and `stderr`
     concurrently with `tokio::join!` before awaiting `.wait()`.
3. **Explicit timeouts on external commands**: Any subprocess whose runtime
   depends on network I/O (`git fetch`, `git clone`) or untrusted input must be
   bounded by `tokio::time::timeout`.

---

## 3. Lock guards across `.await` points

Holding a synchronous lock (`std::sync::MutexGuard` or `std::sync::RwLockGuard`)
across an `.await` point is a critical bug:

- If task A holds a `std::sync::MutexGuard`, yields at `.await`, and task B on
  the same OS thread tries to acquire the same `std::sync::Mutex`, the runtime
  thread deadlocks permanently.
- Even if the future is `Send`, holding a lock across network or disk `.await`
  points serializes unrelated tasks behind slow I/O.

### Invariants

1. **Scope synchronous lock guards tightly**: Drop `std::sync::MutexGuard`
   before any `.await` (e.g., clone the needed value or extract state inside an
   inner `{ ... }` block).
2. **Use `tokio::sync::Mutex` or `tokio::sync::RwLock` only when the lock must
   genuinely be held across `.await` points** (such as serializing multi-step
   async initialization or rate-limiter state). For fast in-memory state
   updates without `.await`, prefer `std::sync::Mutex`.

---

## 4. Cancellation safety in `tokio::select!` and `timeout`

In Rust, cancelling an async operation drops its `Future` at its current
`.await` point. Any local state not yet persisted or cleaned up is lost unless
handled explicitly via RAII (`Drop`).

### High-risk patterns in Sashiko

- **Worktree leaks**: If an async review task creates a temporary git worktree
  (`GitWorktree` in `src/git_ops.rs`) and is cancelled by a timeout or shutdown
  signal before reaching its cleanup call, the worktree directory and git
  metadata remain on disk unless cleanup is tied to a `Drop` guard or explicit
  `finally`-style error path.
- **Database state mismatch**: If a worker claims a patchset or review row in
  SQLite (`status = 'in_progress'`), then gets cancelled during an `.await`
  before updating the status to `'completed'` or `'failed'`, that row remains
  stuck in `'in_progress'` unless recovery or timeout-reclaim logic exists.
- **`select!` loop branches**: In `loop { tokio::select! { ... } }`, ensure
  that the futures polled in each branch are cancellation-safe or re-created
  cleanly on each iteration.

---

## 5. Detached tasks (`tokio::spawn`) and panic propagation

Spawning a background task with `tokio::spawn` detaches it from the caller's
error-handling flow unless its `JoinHandle` is awaited.

### Invariants

1. **Never silently ignore `JoinHandle` on critical workers**: If a background
   daemon loop (`Ingestor`, `Reviewer`, `EmailWorker`, `Compressor`) panics or
   returns an unexpected `Err`, silently dropping the `JoinHandle` hides the
   failure while the server continues running in a degraded state. Log task
   termination explicitly or monitor handles in `src/main.rs`.
2. **Propagate shutdown cleanly**: Long-running background loops must observe
   shutdown signals or cancellation tokens rather than spinning indefinitely
   when the main server exits.
