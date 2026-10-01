# Sashiko Cross-Cutting Pattern: Concurrency and Shared State

Covers shared state across threads, tasks, and OS processes in Sashiko:
in-memory synchronization (`Arc`, `Mutex`, `RwLock`, `Semaphore`, `OnceLock`),
SQLite/libsql concurrent access (`src/db.rs`), and shared git repository
operations (`src/git_ops.rs`).

Sashiko's concurrency model spans **two levels**:
1. **In-process concurrency**: Multiple Tokio tasks running inside the single
   main daemon (`Ingestor`, `Reviewer` dispatcher, `BugWorker`, HTTP API
   handlers, background housekeeping workers). Because `BugWorker`
   (`src/worker/bug_worker.rs`) executes bug-pipeline tasks (`linux_bug` /
   `sashiko_bug`) as in-process Tokio tasks inside the single `sashiko` server
   daemon (recording `HOSTNAME:pid` in `bugs.locked_by` solely for crash
   recovery across daemon restarts), process-wide async locks such as
   `BUG_DEDUP_LOCK` (`tokio::sync::Mutex`) serialize all concurrent bug
   deduplication checks without cross-process contention.
2. **Cross-process concurrency**: The `Reviewer` spawns isolated child
   processes (`sashiko review <id>`) that concurrently access the same SQLite
   database (`sashiko.db`) and the same base git repository (`review_trees/`).

---

## 1. Cross-process database contention (`sashiko.db`)

Because child review workers open their own connections to `sashiko.db` while
the main daemon is simultaneously ingesting patches and serving API requests:

### Invariants

1. **Keep write transactions short**: Never hold a SQLite transaction open
   across network I/O, LLM API calls, or subprocess execution. SQLite (even in
   WAL mode) allows only one writer at a time; holding a write lock during an
   LLM turn blocks all other workers and the API server with `SQLITE_BUSY`.
2. **Busy timeout and WAL mode**: Every database connection opened (in both the
   daemon and CLI/worker subprocesses) must configure `PRAGMA journal_mode=WAL`
   and a non-zero `busy_timeout` so concurrent writers wait briefly rather than
   failing immediately.
3. **Atomic state claims (`UPDATE ... WHERE status = ...`)**: When claiming a
   pending review or outbox item, use conditional updates or `RETURNING` clauses
   to prevent two workers from claiming the same task concurrently. A separate
   `SELECT` followed by an unconditional `UPDATE` is a TOCTOU race.

---

## 2. Shared Git repository and worktree concurrency

Multiple review workers operate concurrently on the same underlying git
repository via `git worktree` (`src/git_ops.rs`).

### Invariants

1. **Never mutate the main repository worktree or `HEAD` during reviews**: All
   patch application (`git am`, `git apply`, `git checkout`) must happen inside
   an isolated, uniquely named worktree under `review_trees/`.
2. **Serialize repository-wide maintenance against active worktrees**: Operations
   that rewrite packfiles, prune objects, or lock refs globally (`git gc`,
   `git repack`, `git prune`, `git worktree prune`) can corrupt or race with
   concurrent `git worktree add` or `git show` commands if objects are deleted
   while a worker is reading them.
   - Ensure maintenance workers (`src/worker/repack.rs`) respect repository
     locks or coordinate with active review semaphores.
3. **Unique worktree identifiers**: Worktree directory names and temporary
   branch names must incorporate unique identifiers (such as review ID or UUID)
   so concurrent reviews of the same patchset (e.g. re-reviews or parallel runs)
   do not collide on filesystem paths or git ref names.

---

## 3. Semaphores and bounded concurrency (`tokio::sync::Semaphore`)

Sashiko bounds concurrent reviews and LLM calls using `Semaphore` permits
(configured via `review.concurrency`).

### Invariants

1. **RAII Permit Guards (`SemaphorePermit` / `OwnedSemaphorePermit`)**: Always
   bind acquired permits to a named local variable whose lifetime matches the
   bounded operation (`let _permit = sem.acquire().await?;`).
   - Beware of `let _ = sem.acquire().await?;` — assigning to bare `_`
     **immediately drops the permit**, defeating the concurrency limit!
2. **No hierarchical deadlock on the same semaphore**: If a parent task
   acquires a permit from semaphore `S` and then spawns child sub-tasks that
   also attempt to acquire permits from the *same* semaphore `S` before the
   parent can finish, the system will deadlock as soon as `concurrency` parent
   tasks fill all permits.
   - Use separate semaphores for different hierarchy levels (e.g., review-level
     concurrency vs. stage-level tool concurrency).

---

## 4. Process-global statics (`OnceLock`, `LazyLock`)

Global singletons (`std::sync::OnceLock`) persist for the lifetime of the
process.

### Invariants

1. **No project-specific or mutable configuration in global statics without
   validation**: If a helper caches paths or settings in a `OnceLock`, ensure
   it cannot silently return stale data when unit tests run in parallel within
   the same test binary, or if configuration varies by CLI flags (`--project`).
2. **Prefer explicit dependency injection (`Arc<Settings>`, `WorkflowEnv`)**
   over hidden global statics so state flow remains visible and testable.
