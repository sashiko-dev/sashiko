# Rust Subsystem Details

Read the general coding guidelines in `Documentation/rust/coding-guidelines.rst`.

## Building

  - Assume Rust code compiles successfully and is lint-clean (including Clippy lints) -- we have other bots and CI systems that ensure this.

    However, CI builds only a limited set of kernel configurations, so still **REPORT as bugs**: conditional compilation issues, such as compilation errors or dead code, that arise under valid Kconfig symbol combinations (i.e. `CONFIG_*`).

  - Assume Rust unstable features are available (the kernel uses `RUSTC_BOOTSTRAP=1`).

## Bindings and helpers

Files under `rust/helpers/` are functions that export inline functions or function macros for Rust code to link to.
They're all prefixed with `rust_helper_` and functions defined there will be exposed in `bindings` with the prefix stripped.
All helpers should be annotated with the `__rust_helper` attribute. **REPORT as bugs**: if new helpers are added without the attribute.

Constants defined in `rust/bindings/bindings_helper.h` re-define complex macro constants using `const` so that bindgen can convert them.
They're all prefixed with `RUST_CONST_HELPER_` and the constants defined there will be exposed in `bindings` with the prefix stripped.

If you have a concern about how a C API is used (i.e. a `bindings::*` call), read the corresponding C source to confirm its actual requirements rather than relying on your recollection of the API.

## FFI types

In the kernel, `unsigned long` is always identical to `uintptr_t` and `size_t`.
Therefore, `ffi::c_ulong` is always mapped to `usize` unlike userspace Rust.

## Inline annotations

Functions using `build_assert!()` that depend on function parameters need to be annotated with `#[inline(always)]`.

For abstractions *ONLY*: Functions that are small or forwarding to a binding call should be annotated with `#[inline]`. Leaf crates like drivers are exempt.

**REPORT as nits**: if used incorrectly.

## Pin initialization

`try_pin_init!(Struct { field: expr })` (or `pin_init!` if infallible) is used to initialize structs that requires pinning.
Fields that are initialized in-place use `field <- expr` rather than `field: expr`.
Fields that are already initialized can be referred to by name in later initialization.
`_: { /* any code */}` can be used to run arbitrary code in between fields.

## Common problems

### Import formatting

If the commit touches imports, it should follow the kernel vertical import style documented in the general coding guidelines. Vendored crates (e.g. syn, pin-init) are exempt.

**REPORT as nits**: if used incorrectly.

### Missing invariant comments

When a struct with an `# Invariants` documentation section is constructed, the code should have an `// INVARIANT:` comment explaining why the invariants are satisfied, similar to `// SAFETY:`.
