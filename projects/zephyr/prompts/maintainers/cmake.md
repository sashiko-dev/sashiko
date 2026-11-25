+++
title = "CMake Build Configuration"
paths = [
    "CMakeLists.txt",
    "**/CMakeLists.txt",
    "*.cmake",
    "**/*.cmake",
]
+++

# CMake Build Configuration

Applies to `CMakeLists.txt` and `*.cmake` files. Mechanical formatting checked
by `scripts/ci/check_compliance.py` (`CMakeStyle`) is excluded; focus on build
logic, layering, and portability defects that CI does not catch.

## Include path boundaries

Source: doc/contribute/guidelines.rst; CMakeLists.txt.

- Do not expose private kernel or internal architecture include directories
  (such as `${ZEPHYR_BASE}/kernel/include` or `${ARCH_DIR}/${ARCH}/include`) via
  `zephyr_include_directories()` or `include_directories()` in driver or
  subsystem `CMakeLists.txt` files.

## Host tools and configure-time side effects

Source: doc/hardware/porting/board_porting.rst.

- Do not invoke optional external host tools (such as `qemu-img`)
  unconditionally with `execute_process()` at CMake configure time when a
  missing tool would break build-only workflows. Attach host-tool invocations to
  run, flash, or debug targets instead, or guard them behind the target that
  requires them.

## Configuration and overlay discovery

Source: doc/build/dts/howtos.rst (Set devicetree overlays).

- Per `doc/build/dts/howtos.rst` (verify against the target tree's
  `cmake/modules/configuration_files.cmake`), setting `DTC_OVERLAY_FILE`
  replaces automatic discovery of application-local `socs/` and
  `boards/<board>.overlay` files (distinct from the top-level `soc/` and
  `boards/` trees), whereas `EXTRA_DTC_OVERLAY_FILE` appends overlays while
  preserving auto-discovery. Flag `DTC_OVERLAY_FILE` assignments that
  unintentionally drop board overlays instead of using
  `EXTRA_DTC_OVERLAY_FILE`.

## Diff hygiene

Source: doc/contribute/style/cmake.rst.

- Avoid redundant no-op variable updates, and do not mix unrelated CMake
  reformatting into functional commits (diff-scope hygiene only; leave
  mechanical formatting to `CMakeStyle`). Report at `Low` severity.
