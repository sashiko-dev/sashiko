+++
title = "Rust"
paths = ["rust/", "*.rs"]
symbols = []
+++
# Rust: maintainer notes

Conventions and review priorities of the kernel Rust community for any Rust code in the kernel.

- Rust code is expected to follow the general coding guidelines in Documentation/rust/coding-guidelines.rst.
- Do not flag ordinary build failures or lint issues (including Clippy style lints) in the default kernel configuration; other bots and CI already ensure default-config Rust code compiles and is lint-clean.
- CI builds only a limited set of kernel configurations, so do report conditional-compilation problems (compilation errors, dead code, or broken rustdoc intra-doc links to CONFIG_*-gated types and modules in unguarded doc comments) that appear when optional CONFIG_* options are disabled or under non-default valid CONFIG_* combinations, as bugs.
- Use of Rust unstable features is accepted; the kernel builds with RUSTC_BOOTSTRAP=1, so do not flag them.
- New helpers under rust/helpers/ use the rust_helper_ prefix and must be annotated with the __rust_helper attribute; a new helper missing the attribute is reported as a bug.
- In abstractions (e.g. the kernel crate), small functions and functions that forward to a C binding call should be marked #[inline]; a missing #[inline] there is reported as Low severity.
- Functions using build_assert!() with a condition that depends on their parameters must be marked #[inline(always)] (a plain #[inline] is insufficient because the compiler may decline to inline it, leaving the build_assert!() unresolved and breaking the build); a missing #[inline(always)] there is reported as a bug.
- Do not report #[inline] or #[inline(always)] on other small or performance-critical functions merely because they do not use build_assert!().
- Leaf crates such as drivers are free to use or omit #[inline]; do not report its presence or absence in driver or leaf-crate code, not even at Low severity.
- When a commit touches imports, they should follow the kernel vertical import style from the coding guidelines; deviations are reported as Low severity. Vendored crates (e.g. syn, pin-init) are exempt.
- When constructing a struct whose documentation has an "# Invariants" section, the code should carry an "// INVARIANT:" comment explaining why the invariants hold, analogous to "// SAFETY:" comments.
