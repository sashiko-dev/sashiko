# Sashiko Cross-Cutting Pattern: Error Handling and Panic Safety

Covers `Result` propagation, panic avoidance, error classification, and safe
string/slice manipulation across all Sashiko crates and binaries.

Because Sashiko ingests untrusted inputs—patches from public mailing lists,
GitHub webhook payloads, arbitrary git repository contents, and non-deterministic
LLM responses—**any panic in request handling, ingestion, or review execution
is a denial-of-service vector.**

---

## 1. Forbidden panics on untrusted or runtime data

Production code paths must return `Result<T, E>` (via `anyhow::Result` or
domain error enums) rather than crashing the process.

### `.unwrap()` and `.expect()`

- **Never use `.unwrap()` or `.expect()` on fallible operations involving
  external data**:
  - Parsing patches, email headers, or mbox streams (`src/patch.rs`,
    `src/ingestor.rs`)
  - Deserializing LLM output or tool arguments (`src/workflow/output.rs`,
    `src/toolbox/`)
  - Database queries or row extraction (`src/db.rs`)
  - Network responses, webhook payloads, or filesystem operations
- **Acceptable uses of `.expect()`**:
  - Static initialization of known-valid compile-time constants (e.g.,
    compiling a literal regular expression inside a `OnceLock`).
  - Even then, document why the invariant holds at compile time.

### Indexing and slice bounds (`[i]`, `[a..b]`)

- **Direct indexing (`vec[i]`, `slice[0]`) panics if out of bounds.** Use
  `.get(i)`, `.first()`, `.last()`, or pattern matching (`if let [first, ..] =
  slice`).
- **UTF-8 string slicing (`&s[..max_len]`) panics if `max_len` falls inside a
  multi-byte UTF-8 character.** Patches, commit messages, author names, and LLM
  responses regularly contain non-ASCII characters (UTF-8 box drawing,
  accented names, emoji, CJK).
  - Always check `s.is_char_boundary(idx)`, use `s.char_indices()`, or use
    `s.floor_char_boundary(max_len)` (if available in the target Rust version)
    before slicing strings for truncation or snippet extraction.

### `serde_json::Value` indexing is not map indexing

Sashiko carries LLM output around as `serde_json::Value`, so `value["key"]`
appears throughout `src/workflow/` and `src/workflows/`. It does *not* behave
like `HashMap` indexing, and reporting it as if it did is a false positive:

- **Reading a key that is absent returns `Value::Null`.** `Index` never panics;
  a missing key, or a string key on an array or a number, yields `Null`.
- **Assigning to a key that is absent inserts it.** `IndexMut` adds the key, and
  it first replaces a `Value::Null` receiver with an empty map. Building an
  object field by field with `v["a"] = ...` is the documented use.
- **`IndexMut` panics only where the receiver cannot hold the index at all**:
  a string key on a value that is neither an object nor null (the message is
  `cannot access key "a" in JSON string`), or a `usize` past the end of an
  array. To report one, name the value and show a path on which it arrives as
  some other type.
- `HashMap` and `BTreeMap` are the opposite: `map["key"]` panics with `no entry
  found for key` when the key is absent, and they implement no `IndexMut` at
  all. Do not carry that intuition across.

---

## 2. Swallowed errors (`let _ = ...`, `.ok()`, `.unwrap_or_default()`)

Silently discarding a `Result` hides broken invariants and leaves the system in
an inconsistent state.

### Dangerous patterns to flag

1. **Database state updates**: Ignoring the result of `db.update_review_status(...)`
   or `db.mark_patchset_failed(...)` can leave a patchset permanently stuck in
   an active state or hide database corruption/locking errors.
2. **Git worktree and file cleanup**: If cleanup fails (e.g., `std::fs::remove_dir_all`
   or `git worktree remove`), silently dropping the error with `let _ = ...`
   without at least a `tracing::warn!` masks disk leaks that eventually fill
   the volume.
3. **Fallback defaults on configuration or parsing**: Using `.unwrap_or_default()`
   when parsing structured LLM JSON schemas or settings can silently turn a
   malformed response into an empty findings list (`findings: []`), causing
   Sashiko to report a clean review when the LLM stage actually failed to parse.
   - Distinguish between "explicitly empty" and "failed to parse".

---

## 3. Transient vs. permanent error classification

In worker loops (`src/reviewer.rs`, `src/worker/`, `src/ai/`), how an error is
classified determines whether Sashiko retries with backoff, marks the review as
failed, or crashes the worker.

### Invariants

1. **AI Provider Errors (`ClassifyAiError`)**:
   - Rate limits (HTTP 429), transient network timeouts, and overloaded model
     endpoints (503/529) must be classified as **retryable** with exponential
     backoff.
   - Invalid API keys (401/403), context window exceeded errors that cannot be
     mitigated by truncation, or invalid request schemas (400) must be
     classified as **permanent** so the worker fails fast rather than burning
     time and quota in an endless retry loop.
2. **Workflow validation retries**:
   - When an LLM stage returns malformed JSON or violates a custom validator
     (`OutputFormat`), the error string is fed back to the LLM for self-correction
     up to `max_validation_retries`.
   - Ensure validator error messages are specific and actionable (e.g., naming
     the missing field or invalid enum variant) rather than generic parse
     failures.
3. **Terminal failure recording**:
   - When retries are exhausted or a permanent error occurs, the worker must
     catch the error at the job boundary, record the failure state in the
     database (`status = 'failed'`, with error details), and clean up temporary
     resources before moving to the next job.

---

## 4. Numeric casts and overflow

- Avoid raw `as` casts between integers of different widths or signedness
  (e.g., `u64 as usize`, `i64 as u32`, `usize as i32`) when values originate
  from database row counts, patch line numbers, token counts, or HTTP headers.
- Use `usize::try_from(...)`, `.saturating_add(...)`, or `.saturating_sub(...)`
  to prevent wrapping or sign-extension bugs on 32-bit vs. 64-bit targets or
  when subtracting line offsets.
