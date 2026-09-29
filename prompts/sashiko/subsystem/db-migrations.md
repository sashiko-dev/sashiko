# Database, Schema, and Migration Invariants

This guide covers `src/db.rs` (~15,000 lines) and `src/migrations/*.sql`.
Sashiko uses `libsql` (SQLite-compatible embedded and remote database). Mistakes
in schema migrations, transaction boundaries, or series merge logic corrupt
historical review state and are difficult to recover from.

## 1. Migration Numbering and Tracking (`PRAGMA user_version`)

Migrations live in `src/migrations/NNN_name.sql` and are applied sequentially in
`Database::migrate()` (`src/db.rs`).
- **Tracking Mechanism**: Schema version is stored in SQLite's `PRAGMA user_version`
  integer, *not* in a schema migrations table.
- **Transactional Application**: Each migration step (`if current_version < N`)
  *must* open a transaction (`let tx = self.conn.transaction().await?`), run
  `tx.execute_batch(...)`, update `tx.execute("PRAGMA user_version = N", ())`,
  and commit atomically.
- **Defect to Catch**:
  - Executing migration DDL outside a transaction (leaving a half-applied schema
    if a statement fails while `user_version` remains `N-1`).
  - Editing an already-shipped migration file (`001`..`005`) instead of adding
    a new numbered migration (`006_...`). Existing databases with `user_version >= N`
    will never re-run modified SQL in migration `N`.
  - Forgetting to bump `PRAGMA user_version` at the end of a new migration block.

## 2. Backward Compatibility and Rolling/Multi-Tool Access

`sashiko` server, `sashiko-cli`, and background workers may access the database
across versions or inspect an existing database file.
- **Additive Changes Only**: New columns added via `ALTER TABLE ... ADD COLUMN`
  must be nullable or have a `DEFAULT` value so existing `INSERT` statements continue
  to succeed.
- **Index Coverage**: Any column added to a `WHERE`, `JOIN`, or `ORDER BY`
  clause on high-cardinality tables (`messages`, `patches`, `patchsets`,
  `reviews`, `findings`, `bugs`, `ai_interactions`) requires an explicit index.
  - *Historical Bug (`005_index_patches_message_id.sql`)*: Lookups by
    `patches.message_id` performed full table scans as the table grew until
    `idx_patches_message_id` was added.

## 3. Patchset and Series Merge Invariants

Email and Quilt series ingestion (`create_patchset`, `create_message_with_references`,
`ensure_thread_for_message`) has the highest historical bug density in `src/db.rs`
(see integration tests `merge_bug_*`, `singleton_*`, `cover_letter_*`,
`db_version_merge_test`).
- **Version Isolation**: Patchsets with different version tags (`v1` vs `v2`)
  must *never* merge into the same `patchsets` row, even if they share a thread
  or subject prefix (`db_version_merge_test`).
- **Subject Prefix & Author Alignment**: Two patches or loose messages should
  only merge into an existing series if their author identity and series subject
  prefixes (`[PATCH net-next 1/3]`) are compatible (`merge_bug_different_series`,
  `merge_bug_prefixes`).
- **Cover Letter (`0/N`) Late Arrival**: Cover letters may arrive *after* patch
  `1/N` (`cover_letter_late_merge_test`, `singleton_cover_merge_test`). Updating
  the patchset metadata from a late cover letter must not overwrite a valid
  series root message ID or clobber already-ingested patch parts.
- **Singleton Root Overwrite Prevention**: A single patch (`1/1` or unnumbered)
  must not have its `message_id` or subject overwritten by a reply message in
  the same thread (`singleton_root_overwrite`).

## 4. Outbox State Machines and Idempotency

Outbound side effects (`email_outbox`, `patchwork_outbox`) use database rows as
a durable queue polled by background workers (`lock_pending_email`,
`lock_pending_patchwork`).
- **Lock-Then-Process**: Workers must atomically claim rows (transitioning status
  or setting a lock/timestamp) before performing network I/O so concurrent
  ticks cannot send duplicate emails or checks.
- **No Secret Persistence**: API tokens (`patchwork`, `forge`) are resolved from
  in-memory config at delivery time and must *never* be stored in outbox table
  columns.

## Checklist for Diffs Touching `src/db.rs` or `src/migrations/`

1. **Migration Safety**: Is the new SQL wrapped in a transaction with a matching
   `PRAGMA user_version` increment?
2. **Index Check**: Does every new query on `patches`, `messages`, `reviews`,
   or `bugs` hit an indexed column in its `WHERE`/`JOIN`?
3. **Series Merge Regressions**: If touching `create_patchset` or thread
   resolution, does the change preserve version separation (`v1`/`v2`), cover
   letter ordering, and singleton root protection?
4. **Positional Row Indexing**: When changing `SELECT` column lists, are all
   `row.get::<T>(idx)` positional indices updated to match?
5. **`libsql` Transaction Handle Sharing (`with_connection((*tx).clone())`)**:
   In `libsql`, `Transaction` implements `Deref<Target = Connection>`, and
   `Connection::clone()` is an `Arc<dyn Conn>` clone of the *exact same*
   underlying SQLite connection handle where `BEGIN` is active. Therefore,
   `self.with_connection((*tx).clone())` executes all queries inside `tx` and
   participates in `tx.commit()` / `tx.rollback()`; do NOT flag it as spawning a
   separate connection outside the transaction.
