+++
title = "Database Schema Migrations and Query Scaling Policy"
paths = [
    "src/db.rs",
    "src/migrations/",
]
symbols = [
    "Database",
    "DatabaseTransaction",
    "FINDINGS_CHUNK_SIZE",
    "HYDRATE_CHUNK_SIZE",
    "SUB_BATCH_SIZE",
    "migrate",
]
+++

## Schema Migration Policy

- All SQL migrations under `src/migrations/` (`Database::migrate`) must be
  strictly additive and backward-compatible with existing production SQLite
  databases.
- Never edit or reorder an already-landed migration file; schema changes require
  a new numbered migration file and matching updates to fresh-install schema
  tests (`make check-db-invariants`).
- Adding a column via `ALTER TABLE ... ADD COLUMN` must include a `DEFAULT` (or
  allow `NULL`) so existing rows remain valid without table rewrites.
- Any new foreign-key or filter column used in daemon polling, dashboard pagination,
  or join paths must have a corresponding index (`CREATE INDEX IF NOT EXISTS`).

## Query Construction and Transaction Discipline

- Any query using `IN (...)` over a caller- or batch-supplied slice must chunk
  bind parameters (using bounded chunk constants such as `SUB_BATCH_SIZE`,
  `HYDRATE_CHUNK_SIZE`, and `FINDINGS_CHUNK_SIZE`) so large patchsets or
  backfills never hit SQLite's variable limit.
- Avoid N+1 query loops inside HTTP handlers or background workers when batch
  helpers exist on `Database`.
- Every multi-statement state transition in `src/db.rs` must execute inside an
  explicit transaction (`DatabaseTransaction`) and propagate errors with `?`
  rather than discarding partial write failures with `let _ =` or `.ok()`.
- Do not flag intentional best-effort telemetry or cache writes that explicitly
  document why write failures are non-fatal (such as `CachingAiProvider`).
