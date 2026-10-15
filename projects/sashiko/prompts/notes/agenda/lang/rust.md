# Rust Research Agenda

Rust-specific questions, added to the universal agenda for Rust code.

## RUST: Rust-Specific Invariants

- RUST-1: Where does the code rely on operations that can panic (unwrap, expect, indexing, slicing, arithmetic overflow, explicit panics), and what guarantees each cannot fire on the paths that reach it?
- RUST-2: Where does async code perform blocking work, hold a lock across an await point, or spawn a task whose failure or panic goes unobserved?
- RUST-3: Which unsafe blocks, raw pointers or manual Send/Sync assumptions exist, and which invariants justify them?
- RUST-4: Which states are encoded in types so invalid values are unrepresentable, and where is state still carried as strings, integers or loosely typed values that code must interpret consistently?
