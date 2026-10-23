# Universal Software Engineering Invariants

Core software invariants that apply across all codebases regardless of language or operating environment. These topics cover object ownership, asynchronous lifecycle ordering, error propagation, state synchronization, input and bounds validation, and control-flow completeness.

Derived by `sashiko notes agenda` from 99995 fixes reachable from 3a8660878839faadb4f1a6dd72c3179c1df56787 (81580 code bugs, 104 themes).

## LIFE: Resource Lifecycles, Ownership, and Rollback

- LIFE-1: Which functions transfer ownership of allocations, locks, or reference counts, which return borrowed references, and which consume their arguments on failure?
- LIFE-2: What is the exact acquisition order of resources, locks, and references during setup, and what reverse (LIFO) teardown sequence is required across all error and exit paths?
- LIFE-3: Which pointers or handles must be cleared after release to prevent dangling references or double-free conditions across retries and re-entrancy?
- LIFE-4: How are reference counts, memory quotas, and active work counters kept symmetric across normal completion, early exits, and self-assignment edge cases?
- LIFE-5: Which caller-held locks, task states, or reservations must remain untouched by callees, and how is lock and state balance preserved across every exit path?

## ASYNC: Asynchronous Publication, Concurrency, and Locking

- ASYNC-1: Which fields, locks, completions, and reference counts must be fully initialized before an object is published to concurrent readers, timers, interrupts, or workers?
- ASYNC-2: During teardown, suspend, or reset, in what order must external interfaces be unregistered and background work, timers, or callbacks synchronously drained?
- ASYNC-3: Which exact lock or read-side critical section protects each shared structure, list, or state flag, and where must it be held continuously across check-and-update sequences?
- ASYNC-4: What lock acquisition hierarchy governs nested locks and callbacks, and which locks must never be held while synchronously flushing work or invoking external hooks?
- ASYNC-5: When locks are dropped for blocking calls or acquired after an optimistic check, which references must be pinned beforehand and which invariants revalidated upon relocking?
- ASYNC-6: Where state, ring indices, or pointers are accessed without mutual exclusion, what single-load/store primitives, sequence counters, and paired memory barriers order publication?

## ERR: Return Conventions, Initialization, and Parameters

- ERR-1: What is the exact return and failure convention (negative status, null, error-encoded pointer, boolean, or residual count) of each fallible call, and what signedness is required to store it?
- ERR-2: Which pointers, optional resources, or union variants can be null or unpopulated, and what runtime discriminator or mode check is required before dereferencing or downcasting?
- ERR-3: How are error statuses preserved across cleanup jumps so that teardown steps do not overwrite the original failure code or store error-encoded pointers into nullable fields?
- ERR-4: Which local variables, status codes, out-parameters, heap structures, and alignment padding must be explicitly initialized or zeroed across all control-flow paths before use or export?
- ERR-5: What parameter contracts—including argument order, coordinate bases, bit indices versus shifted masks, and dimensional units—must be satisfied at every API boundary?

## STATE: Coupled State Synchronization and Instance Isolation

- STATE-1: When primary state, geometry, or configuration mutates, resets, or aborts, which coupled caches, shadow fields, companion tables, counters, and timers must update in lockstep?
- STATE-2: Which interior pointers or cursors become stale after buffer reallocations, collection mutations, or lock drops, and how must they be refreshed?
- STATE-3: How is per-instance or per-session state isolated from shared default tables, and how are shared registers or buffers updated without disturbing sibling instances?
- STATE-4: When a context is reset, recycled, or cloned across retries or suspend/resume cycles, which transient flags and cursors must be cleared while preserving user configuration?
- STATE-5: Where buffers or registers are mutated in place, what preconditions must be checked first, and how are original values sampled to prevent aliasing or partial updates?
- STATE-6: When modifying signatures, enums, or data layouts, which parallel fast/slow paths, callback tables, lookup arrays, and build stubs must be updated in lockstep?

## BOUNDS: Input Validation, Indexing, and Binary Layouts

- BOUNDS-1: Which inputs originate from untrusted sources (userspace, wire, disk, firmware, or descriptors), and how are their lengths, offsets, and counts validated before allocation or arithmetic?
- BOUNDS-2: Which arrays, bitmaps, and ring buffers use 0-based versus 1-based indexing, dense versus sparse IDs, or inclusive versus exclusive bounds, and how is circular wrapping performed?
- BOUNDS-3: How are reverse loops and remaining-length calculations guarded against unsigned underflow, and which lookup tables require sentinel terminators?
- BOUNDS-4: What endianness, alignment, and 32/64-bit layout rules govern binary structures exchanged across boundaries, and when are unaligned accessors or width-matched conversions required?
- BOUNDS-5: How must external interfaces, virtual file formats, and unused structure fields be validated and formatted to preserve backward compatibility with existing tools?

## FLOW: Control Flow, Loop Progress, and Collection Iteration

- FLOW-1: How are multi-bit mask checks, boolean polarities, and operator precedences structured, and how are overlapping branch predicates ordered from most- to least-specific?
- FLOW-2: In every switch statement and conditional chain, how are all valid state and enum variants covered without unintended fallthrough, shadowing, or rejection of valid edge cases?
- FLOW-3: When iterating over collections, which removal-safe iterators are required to unlink or free entries, and how are cursors prevented from being dereferenced after loop termination?
- FLOW-4: How are loop induction variables, parallel source/destination indices, and per-iteration status flags initialized and reset across passes and retry jumps?
- FLOW-5: How is forward progress guaranteed across all loop paths—including `continue` branches, zero-length items, and bounded polling—without nesting linear scans in hot or locked paths?
- FLOW-6: When multiple similarly named variables, parent/child structs, or iterators exist in scope, which exact target identifier is intended for each validation and assignment?
