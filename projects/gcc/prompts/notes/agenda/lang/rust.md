# Rust Language and Runtime Semantics

This module covers invariants for Rust language components, focusing on how front-end semantic analysis traverses type hierarchies and propagates evaluation contexts.

Derived by `sashiko notes agenda` from 200 fixes reachable from 4990533b6a10301ff4681fbcc0bbe7c32ca819fe (115 code bugs, 23 themes).

## RSEM: Type Hierarchy Traversal and Constant Context Propagation

- RSEM-1: How must type hierarchy traversal resolve root or underlying types when inspecting component lists on derived or class-wide subtypes?
- RSEM-2: When semantic analysis delegates expression construction to helper builders, how are constant-expression evaluation flags propagated?
- RSEM-3: What preconditions ensure that type-checking queries operate on fully resolved structural definitions rather than unexpanded aliases?
