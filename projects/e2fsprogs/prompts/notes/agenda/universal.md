# Universal Software Engineering Invariants

Core software invariants that apply across all codebases regardless of language
or operating environment. These topics cover object ownership, concurrency
ordering, error propagation, state synchronization, input and bounds validation,
and control-flow completeness.

## LIFE: Resource Lifecycles, Ownership, and Rollback

- LIFE-1: Which functions transfer ownership of allocations, handles, or reference counts, which return borrowed pointers, and which consume or release their arguments on failure?
- LIFE-2: What is the exact acquisition order of buffers, handles, and locks during setup, and what reverse teardown sequence is required across all error and exit paths?
- LIFE-3: Which pointers or handles must be initialized to null before early cleanup jumps and cleared after release to prevent wild frees or double-free bugs?
- LIFE-4: How are reference counts, block/inode accounting counters, and reservation totals kept symmetric across normal completion, early aborts, and retry paths?

## ASYNC: Concurrency, Locking, and Multi-Process Coordination

- ASYNC-1: Which shared structures, caches, or handles are accessed concurrently across worker threads or external processes, and which lock or lease protocol serializes access?
- ASYNC-2: What lock acquisition order governs nested operations, and how is lock release guaranteed on every error or early-return branch?
- ASYNC-3: When multiple processes or mounts can access an underlying device or file, what advisory locks, mount checks, or heartbeat protocols prevent concurrent corruption?

## ERR: Return Conventions, Initialization, and Parameters

- ERR-1: What is the exact return and failure convention (status code, negative errno, null pointer, or boolean) of each fallible call, and how are errors translated across subsystem boundaries?
- ERR-2: How are primary failure codes preserved across cleanup labels so teardown operations do not mask or overwrite the original error?
- ERR-3: Which local variables, output parameters, and heap buffers must be explicitly initialized or zeroed across all control-flow paths before use or serialization?
- ERR-4: What parameter contracts—including unit bases (bytes, blocks, clusters, sectors), inclusive versus exclusive bounds, and flag masks—must hold at each API boundary?

## STATE: Coupled State Synchronization and Dirty Tracking

- STATE-1: When primary metadata or configuration mutates, which coupled checksums, summary counters, bitmaps, shadow copies, and dirty flags must update in lockstep?
- STATE-2: Which interior pointers or iterators become invalid after buffer reallocation, cache eviction, or tree splitting/merging, and how must callers refresh them?
- STATE-3: Under dry-run or read-only modes, how are in-memory repairs or simulations isolated so nothing is marked dirty or flushed to persistent storage?

## BOUNDS: Untrusted Input Validation, Indexing, and Binary Layouts

- BOUNDS-1: Which inputs originate from untrusted or potentially damaged persistent images, and how are lengths, offsets, counts, and header sizes validated before allocation or pointer arithmetic?
- BOUNDS-2: Which tables, bitmaps, and identifiers use 0-based versus 1-based numbering, and how are sentinel or reserved indices handled at boundary conditions?
- BOUNDS-3: How are backward scans and remaining-space subtractions guarded against unsigned underflow when encountering malformed lengths?

## FLOW: Control Flow, Loop Termination, and Tree/Graph Traversal

- FLOW-1: When traversing persistent trees, hash directories, or linked lists that may contain corruption cycles, what depth limits or visited-node bitmaps guarantee termination?
- FLOW-2: In multi-flag predicates and switch statements, how are mutually exclusive or dependent feature flags checked without unintended fallthrough or partial handling?
