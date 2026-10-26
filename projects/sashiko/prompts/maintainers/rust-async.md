+++
title = "Rust Async, Concurrency, and Resource Limits Policy"
paths = [
    "src/",
]
symbols = [
    "Mutex",
    "RwLock",
    "Semaphore",
    "kill_on_drop",
    "spawn_blocking",
]
+++

## Tokio Async and Lock Discipline

- Never hold a `std::sync::MutexGuard` or `std::sync::RwLockReadGuard` /
  `RwLockWriteGuard` across an `.await` point; use `tokio::sync` primitives when
  a lock must span `.await`, or scope the synchronous guard to a block that
  drops before `.await`.
- Offload CPU-heavy parsing, synchronous compression, or blocking filesystem
  sweeps on hot daemon paths to `tokio::task::spawn_blocking`.
- Do not flag `std::fs` calls during CLI startup, configuration loading, build
  scripts, or single-threaded test setup where async runtime starvation does not
  apply.
- Ensure spawned background tasks or child processes have clear cancellation or
  shutdown paths (`kill_on_drop(true)` for child processes, bounded loops or
  channel closure for background workers).

## Bounded Resources and UTF-8 Slicing

- Every string truncation or byte-window slice on externally supplied text (diff
  hunks, commit messages, tool outputs, email bodies, LLM responses) must split
  on a valid UTF-8 character boundary (`is_char_boundary`, `floor_char_boundary`,
  or `.char_indices()`). Flag any raw `&s[..n]` byte slice on arbitrary text
  where `n` is not proven to be ASCII or a char boundary.
- Network responses, archive extractions, mbox streams, and subprocess stdout/
  stderr captures must enforce explicit byte or item bounds so malformed or
  hostile inputs cannot exhaust daemon memory.
