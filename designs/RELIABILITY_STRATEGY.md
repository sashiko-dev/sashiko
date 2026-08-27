# Persistent State & Async Reliability Strategy

**Objective:** To transition the `sashiko` infrastructure from reactive bug-fixing to a proactive, resilient architecture. The recent cascading failures in bug deduplication were not simple logical errors—they were symptoms of architectural gaps in how we handle asynchronous state, concurrency, and relational data. 

To eliminate constant regressions, we must adopt the following systemic engineering practices.

---

## 1. System Design: Making Invalid States Unrepresentable

The fundamental flaw in our recent bug was that the database and the type system *allowed* a review to point to a "duplicate" tombstone without raising an alarm. 

### A. Typestate Pattern for Lifecycle Management
Currently, state is managed by strings (`"raw"`, `"processing"`, `"duplicate"`, `"open"`). This pushes state validation to runtime checks.
*   **Action:** Implement Rust's Typestate pattern for our core entities. A `RawBug`, `DuplicateBug`, and `VerifiedBug` should be distinct structs. 
*   **Benefit:** A function that links a review to a bug (e.g., `link_review_to_bug`) will statically require a `&VerifiedBug` or `&OpenBug`. It becomes a compile-time error to pass a `RawBug` or `DuplicateBug` to a review link operation, physically preventing the tombstone bug we just fixed.

### B. Transactional Boundaries & The Outbox Pattern
Right now, workers perform multiple discrete `await` calls that mutate the database. If a worker crashes mid-run, we are left in a torn state.
*   **Action:** Adopt the **Transactional Outbox** or **Saga** pattern for async workflows. Instead of workers directly mutating `review_bugs` or `bugs` individually, the worker should yield a strongly-typed `TransitionEvent` (e.g., `BugDeduplicated { old_id, new_id }`).
*   **Benefit:** A single database transaction consumes the event, updates the bug status, and migrates the foreign keys atomically. This centralizes state mutations into single, highly-testable SQL transactions.

---

## 2. Concurrency: Designing for Data Races

The vector deduplication issue occurred because multiple workers evaluated "raw" bugs simultaneously against each other. In an async system, we must assume everything happens concurrently.

### A. Strict Row-Level Locking & Idempotency
*   **Action:** When picking up candidate bugs for LLM deduplication, we must employ explicit locking mechanisms or conditional processing. For SQLite, since it employs database-level or WAL-level locks, we should use state-transitions with `UPDATE ... WHERE status = 'raw' RETURNING id` to safely dequeue work, ensuring no two workers process overlapping raw data contexts blindly.
*   **Action:** Vector searches *must* be gated to immutable or terminal states (`verified`, `open`, `fixed`). Intermediate states (`raw`, `processing`) must be strictly excluded from read-heavy similarity queries.

---

## 3. Testing: Shifting Quality Left

Standard unit tests didn't catch the deduplication race condition because they ran sequentially and deterministically. 

### A. Property-Based Testing (Fuzzing)
*   **Action:** Introduce `proptest` to generate random sequences of incoming patches. 
*   **Scenario:** Fire 5 identical patches, 3 slightly modified patches, and 2 unrelated patches into the ingestor concurrently.
*   **Assertion:** No matter the ingestion order or async timing, assert that the system *always* converges to exactly 2 active open bugs and 10 accurately mapped `review_bugs` links.

### B. Deterministic Async Simulation
*   **Action:** Do not use `tokio::time::sleep` in tests to wait for workers. Introduce a mockable `TimeProvider` or use Tokio's `test` utilities to pause and step through async executor ticks. We need to artificially pause `Worker A` right before deduplication, let `Worker B` finish, and resume `Worker A` to explicitly test race conditions in CI.

### C. Invariant Anomaly Detection checks
*   **Action:** Add a `make check-db-invariants` script that runs SQL queries designed to find impossible states (e.g., `SELECT * FROM review_bugs rb JOIN bugs b ON rb.bug_id = b.id WHERE b.status = 'duplicate'`). Run this at the end of all integration tests.

---

## 4. Code Review & Engineering Culture

To guide junior engineers away from building fragile async systems, we need structural friction before code is written.

### A. Architecture Decision Records (ADRs)
*   For any feature involving an asynchronous worker, external API (LLMs), or new relational tables, require a lightweight markdown ADR in `designs/` before coding begins.
*   **Required ADR Section:** *"Concurrency & Failure Modes"*. The author must explicitly answer: 
    1. *What happens if this worker crashes on line 50?*
    2. *What happens if two instances of this event fire in the exact same millisecond?*

### B. "State First" Code Reviews
*   Reviewers (like myself) will stop looking just at the logic (`extract_bug_vector`) and first validate the State Machine. If the state machine is vague or untyped, the PR is rejected before we look at the algorithmic correctness.
