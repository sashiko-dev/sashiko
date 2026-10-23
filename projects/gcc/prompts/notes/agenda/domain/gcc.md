# GCC Front-End, Middle-End, and Back-End Architecture

This module addresses core compiler invariants across GCC's language front ends, GIMPLE/SSA middle-end transformations and analyses, and RTL target code generation.

Derived by `sashiko notes agenda` from 200 fixes reachable from 4990533b6a10301ff4681fbcc0bbe7c32ca819fe (115 code bugs, 23 themes).

## FEAST: Front-End Semantics, Diagnostics, and AST Lowering

- FEAST-1: In Fortran, what attribute, presence, and reference-chain rules govern C interoperability, associate variables, virtual tables, coarrays, and spans?
- FEAST-2: In C++, how do template deduction, partial substitution, overload resolution, and constant evaluation handle arity, context state, qualifiers, and heap allocations?
- FEAST-3: How is diagnostic emission decoupled from functional state resets, and how are warnings scoped in unevaluated contexts or on compiler built-ins?
- FEAST-4: What nullability, node-kind, and union-layout preconditions must be verified on AST and IR nodes, especially during error recovery or late compilation?
- FEAST-5: During gimplification and OpenMP lowering, how are tree nodes unshared, variable-length bounds replaced, and declaration attributes ordered?
- FEAST-6: What rules govern deferred emission of inline variable templates, coroutine frame reference-count decrements, and stack storage partitioning?

## MEOPT: Middle-End IR Semantics, Analysis State, and Vectorization

- MEOPT-1: In SLP vectorization, how is per-lane state separated from shared scalar statement info, validated across grouped statements, and rolled back on failure?
- MEOPT-2: How do jump threading, switch clustering, scheduling, and inlining bound search complexity, check profitability, and respect register pressure and profiles?
- MEOPT-3: When mutating the CFG or GIMPLE, how are profile counts partitioned, PHI nodes and addressable labels preserved, and trapping operations kept non-speculative?
- MEOPT-4: When must cached flow-sensitive ranges, value relations, per-function sections, and precompiled header tables be invalidated or avoided?
- MEOPT-5: How do middle-end type predicates and conversions handle vector element precision, signed multi-bit boolean masks, switch label types, and large bit-precise integers?
- MEOPT-6: How do pointer analysis, object size queries, and the static analyzer represent negative subobject offsets, non-local targets, zeroed memory, and macro loops?

## BERTL: RTL Invariants, Target ISA Constraints, and ABI Boundaries

- BERTL-1: How are machine modes validated across basic blocks, hard-register subregs, spills, vector broadcasts, and dead-store extractions?
- BERTL-2: Where are auto-increment register expressions restricted, and how must block-move size operands relate to proven constant bounds?
- BERTL-3: How do ABI boundaries enforce explicit sub-word extensions for untrusted callers and guarantee the presence of runtime conversion helpers?
- BERTL-4: How do target instruction emitters account for multi-word operand sizes, register-only constraints, and hardware vector lengths?
- BERTL-5: What rules preserve floating-point trapping semantics and excess precision requirements during target code generation and folding?
