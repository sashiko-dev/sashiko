# Perf Tools Subsystem Details

## Tool API Callbacks

Omitting event callbacks in `struct perf_tool` causes incoming events to be
silently dropped. In pipe mode, dropping `perf_event_header_attr` events
prevents the creation of evlists/evsels, breaking event processing entirely.

- Unregistered event types are silently ignored
- Any tool registering `.mmap` must also register `.mmap2` (and vice versa)
- In pipe mode, verify tools correctly register attribute and feature callbacks
  to populate evsels and `struct perf_env`

## Build Feature Detection and Conditional Compilation

Inconsistent feature detection flags cause build failures or missing
functionality when optional libraries are omitted. When a feature test succeeds,
`Makefile.config` defines `-DHAVE_*_SUPPORT` flags; omitting these defines or
failing to provide header fallback stubs breaks compilation on systems lacking
the library.

- Feature checks (`tools/build/feature/test-*.c`) verify optional library
  availability during build
- `tools/perf/Makefile.config` evaluates feature test results and sets compiler
  flags (e.g., `CFLAGS += -DHAVE_LIBELF_SUPPORT` or `CONFIG_*` defines)
- C code must guard feature-dependent logic with `#ifdef HAVE_*_SUPPORT` or
  using `CONFIG_*` values in the Build or Makefile
- Header files must provide compatible dummy inline stubs (e.g., returning
  `-ENOTSUPP` or `NULL`) when the feature define is absent
- When adding or modifying a feature, ensure `Makefile.config`, feature
  makefiles, and header guards remain strictly synchronized

## perf.data Header Validation

A `perf.data` file may be a regular file or come from a pipe. When accessing
events in pipe mode, the stream doesn't support seek. A regular file contains
sections for attributes and for features; in pipe mode, these must be handled as
synthesized events.  New features will be unknown and unsupported by old perf
tools, whilst `perf.data` files from old perf tools won't contain the new
features. The loaded features are put in `struct perf_env`, which is typically
populated by `perf_session__new()`, but in pipe mode, events need processing to
fill in the `perf_env`. In live mode (like `perf top`), the host `perf_env` is
explicitly created. Accessing `perf_env` fields without first verifying those
fields are initialized is a bug.

## Architecture-Specific Code and Cross-Platform Analysis

Placing analysis or decoding logic in `tools/perf/arch/` restricts that
functionality to host binaries compiled for that specific architecture. This
breaks cross-platform analysis, preventing a perf binary on x86 from inspecting
or reporting on a `perf.data` file recorded on ARM or RISC-V.

- The `tools/perf/arch/` directory must only contain code strictly tied to host
  execution (such as native PMU probing or hardware registers)
- Discourage adding new logic to `tools/perf/arch/`; prefer cross-platform
  implementations
- To handle architectural variations during recording or analysis, inspect the
  ELF machine constant (`e_machine`) dynamically available via `struct
  perf_env`, session, machine, thread, or evsel structures

## Reference Count Checking and Pointer Handles

Failing to balance reference counts in perf tools causes memory leaks or
use-after-free defects. When built with `REFCNT_CHECKING` (enabled by
ASAN/LSAN), perf wraps reference-counted structs (e.g., `thread`, `maps`, `dso`)
into intermediary pointer handles (`DECLARE_RC_STRUCT`). Accessing a handle
after calling `_put()` triggers immediate ASAN heap-use-after-free traps, while
missing `_put()` calls trigger LSAN leaks at the exact `_get()` call site.

- Every reference handle acquired via `_get()` (e.g., `thread__get()`,
  `maps__get()`) or allocated via `_new()` must be strictly paired with a
  matching `_put()` (e.g., `thread__put()`)
- When a struct is passed to `_put()`, its pointer handle is invalidated and
  freed; never access struct fields after calling `_put()`
- Avoid raw pointer assignment for reference-counted structs; use explicit
  `_get()` and `_put()` lifecycle helpers

## POSIX libc Header Inclusions and musl Compatibility

The perf tool is compiled with both glibc and musl. While glibc suffers from namespace pollution (implicit inclusion of headers through others), musl strictly separates declarations. Code that compiles under glibc may fail to compile under musl due to missing explicit header inclusions. To ensure musl build compatibility, all files using libc functions, variables, or constants must directly include the headers where those symbols are declared as per the POSIX standard.

- Do not rely on header files implicitly including other headers.
- Always explicitly include the POSIX-specified header for any libc function, variable, or constant used (e.g., `<unistd.h>` for `read`/`write`/`close`, `<stdio.h>` for `printf`/`fopen`, `<stdlib.h>` for `malloc`/`free`, `<string.h>` for `strcmp`/`strlen`, `<limits.h>` for `PATH_MAX`).
- Prefer forward declarations (e.g., `struct evlist;`) in header files instead of full header inclusions when only structure pointer handles are referenced. Do not manually forward declare standard libc functions or types; these must always be included via appropriate POSIX standard headers.
- Verify that all required system and POSIX standard header files are explicitly placed at the top of the file.

## Error Handling and `ERR_PTR` Avoidance

Using `ERR_PTR` in user-space `perf` tools is highly discouraged. While `ERR_PTR` is common in kernel space, its use in user-space `perf` code frequently leads to bugs where `ERR_PTR` values are incorrectly compared to `NULL` instead of being checked with `IS_ERR()`.

- **Avoid `ERR_PTR` in New Code**: Prefer standard user-space paradigms. Functions returning pointers should return `NULL` on failure.
- **Propagating Error Codes**: If a specific error code must be communicated to the caller:
  - Return an `int` (negative POSIX errno, e.g., `-ENOMEM`) and pass the allocated object back via a double pointer argument (e.g., `struct foo **out`).
  - Alternatively, set `errno` and return `NULL`.
- **Audit Existing `ERR_PTR` Usage**: If `ERR_PTR` must be used (e.g., when interfacing with legacy APIs that return them), verify that all callers use `IS_ERR()` and `PTR_ERR()` rather than `NULL` checks.

## Quick Checks

- **Callback error paths**: When a function takes a callback and iterates
  over a directory, verify that callback errors trigger full cleanup before
  return.
- **Nested `openat`/`fdopendir`**: When iterating nested directories (e.g.,
  `/proc/pid/fd` then `/proc/pid/fdinfo`), track each resource separately
  and verify cleanup ordering.
- **Tool API callbacks**: Verify subcommands register complete event callbacks (pairing `.mmap`/`.mmap2` and handling `.attr` in pipe mode).
- **Feature detection guards**: Verify optional feature logic is correctly guarded with `HAVE_*_SUPPORT` or `CONFIG_*` defines and accompanied by header fallback stubs.
- **`perf_env` validation**: Verify `perf_env` fields are checked for initialization before access.
- **Cross-platform analysis**: Verify architecture-specific logic queries `e_machine` dynamically rather than relying on hardcoded `tools/perf/arch/` host binaries.
- **Reference count balancing**: Verify every `_new` and `_get` pointer handle is paired with a matching `_put` before pointer scope ends.
- **musl Compatibility**: Verify all POSIX libc functions, constants, and variables have explicit, direct header inclusions (e.g. `<unistd.h>`, `<string.h>`) to prevent musl compilation failures. Encourage forward declarations of internal structures in header files where possible to avoid heavy header inclusions.
- **`ERR_PTR` usage**: Verify that `ERR_PTR` is not used in new code. For existing usage, ensure returned pointers are checked with `IS_ERR()` rather than `NULL` comparisons.
