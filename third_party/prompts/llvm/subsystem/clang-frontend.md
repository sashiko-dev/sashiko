# Clang Frontend Review Guidelines

## Core Principles
Clang parses C, C++, and Objective-C into AST representations, performs semantic analysis (Sema), and generates LLVM IR.

## Critical Invariants to Verify

1. **State Persistence Across Overload Lookup Failures**:
   - In overload resolution and builder helpers (e.g. `CoroutineStmtBuilder`), if a secondary lookup (e.g. For aligned `operator new`) fails after a primary lookup succeeded, the intermediate candidate state must be reset.
   - Retaining partially resolved state while modifying flags produces spurious compile-time errors or crashes.

2. **Null AST Nodes and Type Locations**:
   - AST visitors walking `TypeLoc` or `Stmt` hierarchies must check for null children before recursive traversal.
   - Builtin intrinsic lowerings must handle corner-case argument types (e.g. 1-bit integers or unusual vector sizes) gracefully without asserting.
