# Rust Language and Runtime Invariants

Rules specific to Rust code and its type system, focusing on memory initialization soundness and uninhabited types.

Derived by `sashiko notes agenda` from 99995 fixes reachable from 3a8660878839faadb4f1a6dd72c3179c1df56787 (81580 code bugs, 104 themes).

## RUST: Type Soundness and Initialization Traits

- RUST-1: Which types can be safely instantiated via raw memory or zero-initialization traits, and how are uninhabited types (such as empty enums or the never type) excluded?
- RUST-2: What invariants must unsafe trait implementations uphold to guarantee that constructing or transmuting values never produces an invalid bit pattern or uninhabited type?
- RUST-3: Where raw pointers or uninitialized buffers cross FFI or abstraction boundaries, how does the type system enforce initialization and lifetime validity before safe references are created?
