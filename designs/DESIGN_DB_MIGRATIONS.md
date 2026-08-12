# Database Migrations Redesign

## 1. Background & Motivation
Sashiko's database currently relies on a single block of commands (`src/schema.sql`) populated with `CREATE TABLE IF NOT EXISTS` clauses, coupled with manual `ALTER TABLE` operations directly implemented in `src/db.rs`. 
This paradigm creates the following issues:
1. **Inefficient Startups**: The application attempts to load and execute structural statements and multiple manual column verifications on every boot. This slows down startup sequences as the database has to repetitively evaluate conditions on large datasets.
2. **Fragility**: Migrations involving row backfilling or index creation can fail or perform redundantly if not properly guarded.
3. **Traceability**: Without an explicit version number for the application database, investigating production issues relative to structural changes is convoluted.

## 2. Proposed Solution
We propose shifting to **versioned, incremental migrations using `PRAGMA user_version`**. `user_version` is an integer stored directly in the SQLite database header specifically designed for an application to define its database version state.

### 2.1 Architecture
1. **Migrations Directory (`src/migrations/`)**: All DDL and DML operations that mutate the database schema or data states between versions will be separated into strict SQL scripts.
   - `001_initial.sql`: The base schema.
   - `002_severity_explanation_add.sql`: Subsequent alterations.
   - etc.

2. **Migration Runner logic (`src/db.rs`)**:
   Instead of blanket-executing `schema.sql`, `Database::migrate(&self)` will:
   - Query `PRAGMA user_version` to fetch the current `$version`.
   - Iterate chronologically through embedded migration SQL scripts starting from index `$version`.
   - Run the script as a single batch execution.
   - Immediately bump `PRAGMA user_version` inside the transaction.

## 3. Implementation Plan
### Step 1: Migration Scripts Restructuring
1. Break down `schema.sql` into standard base `001_initial.sql`.
2. Extract the unversioned schema adaptations (like `add_column("findings", ...)` or table creations inside `db.rs` such as `tool_usages` and `messages_mailing_lists`) into subsequent numbered scripts (`002_*.sql`, `003_*.sql`).

### Step 2: Implementation of the Runner
1. Add `rust-embed` or use `include_str!` array/macro logic to bundle migration paths statically if there are few, or dynamically via macro. Since we don't want to bloat dependencies, manual `include_str!` arrays map nicely to SQLite arrays.
2. Replace the contents of `Database::migrate` in `db.rs` with the `PRAGMA user_version` sequential loop.

### Step 3: Cleanup and Testing
1. Remove all manual ad-hoc `.execute_batch()` or `try_add_column()` schema manipulation logic scattered within `db.rs`.
2. Ensure integration tests start with an empty database, successfully run migrations down the chain to the latest version, and yield a fully structured instance without failures.

## 4. Alternatives Considered
* **`sqlx` or `refinery` Crates**: Migrating `libsql`-backed databases via third-party crates like `sqlx` adds tremendous overhead without significant returns given our specific usage of `libsql`. `user_version` is significantly natively lightweight.

