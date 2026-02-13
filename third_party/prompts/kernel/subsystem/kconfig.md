# Kconfig Subsystem Details

## Config Symbol References

Referencing a non-existent config symbol in `select` or `depends on` causes
silent build failures: the dependency is never satisfied, or selecting a
non-existent symbol has no effect, leaving required drivers or features
disabled when the user expects them to be built.

- All config names in `select FOO` and `depends on FOO` must correspond to a
  `config FOO` definition somewhere in the kernel tree
- Watch for typos between similar prefixes: `QCM_` vs `QCS_`, `IMX8M_` vs
  `IMX8MM_`, etc.
- When adding a new config that selects an existing one, verify the selected
  symbol's name by searching for its `config` definition

## Dependency Propagation

Selecting a config symbol without inheriting its dependencies causes Kconfig
warnings at build time (`sym_warn_unmet_dep()` in
`scripts/kconfig/symbol.c`). Config A can be enabled on platforms where
config B (which A selects) cannot be enabled, producing unmet dependency
warnings and potential build failures.

- When `config A` uses `select B`, config A must have dependencies compatible
  with (same or more restrictive than) config B's `depends on` line
- Common case: if B has `depends on ARM64 || COMPILE_TEST`, then A must also
  have `depends on ARM64 || COMPILE_TEST`

```
// WRONG: Missing architecture dependency
config QCS_DISPCC_615
    tristate "QCS615 Display Clock Controller"
    select QCS_GCC_615  // QCS_GCC_615 depends on ARM64 || COMPILE_TEST
    // Missing: depends on ARM64 || COMPILE_TEST

// CORRECT: Proper dependency inheritance (drivers/clk/qcom/Kconfig)
config QCS_DISPCC_615
    tristate "QCS615 Display Clock Controller"
    depends on ARM64 || COMPILE_TEST
    select QCS_GCC_615
```

The resulting Kconfig warning looks like:
```
WARNING: unmet direct dependencies detected for QCS_GCC_615
  Depends on [n]: ARM64 || COMPILE_TEST
  Selected by [m]:
  - QCS_DISPCC_615 [=m]
```

## Cross-Config Consistency

When multiple related configs are added together (e.g., clock controllers
for the same SoC family), inconsistencies between them often indicate
copy-paste errors or typos.

- Compare new configs with existing similar configs in the same file
- Check that related configs (e.g., `QCS_DISPCC_615`, `QCS_GPUCC_615`,
  `QCS_VIDEOCC_615`) follow the same dependency and select patterns
- If one config in a series differs from the others, verify the difference
  is intentional

## Architecture-Specific Symbols in COMPILE_TEST Drivers

Using `select` for architecture-specific symbols in drivers that support
`COMPILE_TEST` can cause unmet dependency warnings or build failures on
unsupported architectures. The `select` statement forces the symbol on
regardless of its own dependencies, and if the selected symbol pulls in
arch-specific infrastructure, compilation may fail.

- `COMPILE_TEST` (defined in `init/Kconfig`) allows drivers to be compiled
  on any architecture for build coverage testing, even when the hardware
  only exists on one arch
- When a Kconfig has `depends on ARCH_FOO || COMPILE_TEST`, the driver can
  be built on architectures other than `ARCH_FOO`
- Using `select ARCH_SPECIFIC_SYMBOL` in such a driver triggers unmet
  dependency warnings on architectures where that symbol's own dependencies
  are not met

When a `select` target has arch-specific dependencies, prefer one of these
approaches:

- Use conditional selection:
  ```
  select SOME_SUBSYSTEM if ARM64
  ```
- Change `select` to `depends on` so the driver is only available when the
  infrastructure exists:
  ```
  depends on SOME_SUBSYSTEM
  ```

## Quick Checks

- **Selected symbol existence**: Verify every `select FOO` references a
  config that actually exists
- **Dependency inheritance**: When selecting a config with `depends on`,
  ensure the selector has compatible dependencies
- **Naming consistency**: Check for typos by comparing with related configs
  in the same subsystem
- **COMPILE_TEST with arch-specific select**: When a driver uses
  `depends on ... || COMPILE_TEST`, verify that all `select` statements
  reference symbols available on all architectures
