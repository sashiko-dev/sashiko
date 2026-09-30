# Build Subsystem Details

## Python Compatibility

Enforcing Python 2 compatibility creates false positives. The kernel build system and auxiliary scripts require Python 3.

- Assume Python 3 for all `.py` files (`scripts/`, `tools/`, `Documentation/`)
- Do not report Python 2 incompatibilities (type annotations, `print()`, f-strings) as defects

## Toolchain Requirements

Flagging missing or incompatible build tools without verifying against official documentation misleads developers.

- Minimal required tool versions (GCC, Clang, Make, flex, bison) are documented in `Documentation/process/changes.rst`
- Do not assume legacy tool versions are required unless dictated by architecture constraints

## C Standard and Compilation Flags

Evaluating kernel C code against ISO C or user-space compiler defaults causes false defect reports on GNU extensions, data types, and aliasing.

- **Language standard**: Written in GNU C11 (`gnu11`) per `Documentation/process/programming-language.rst`. Do not flag GNU extensions (statement expressions, `typeof`, zero-length arrays, case ranges) as non-standard.
- **Unsigned char (`-funsigned-char`)**: Top-level `Makefile` enforces unsigned `char` on all architectures. Checking `char < 0` is dead code or incorrect logic.
- **Strict aliasing (`-fno-strict-aliasing`)**: Core kernel code disables strict aliasing; do not report type punning or pointer casting as undefined behavior in kernel space. Conversely, files under `tools/` assume standard `-fstrict-aliasing`; warn about type punning or incompatible pointer casting in tools.
- **Boolean-to-pointer casts (`(void *)bool_val`)**: In C (`_Bool`), casting a `bool` directly to `void *` (e.g., passing a boolean flag through a `void *private` callback argument) does NOT trigger `-Wint-to-pointer-cast` in either GCC or Clang. Do not report `(void *)bool_val` as a compiler warning or `-Werror` build failure.

## Host Build Environment and POSIX Portability

The Linux kernel build system (`scripts/`, `tools/`, and Kbuild Makefiles) targets POSIX-compliant host environments (Linux/BSD with POSIX.1-2008 and standard coreutils).

**Do NOT report (false positives):**
- Opening binary files with `fopen(..., "w")` or `"r"` instead of `"wb"` / `"rb"` in host build utilities (`scripts/`, `tools/`): `"w"` and `"wb"` are identical on POSIX systems, and Windows CRLF newline translation (`0x0A` to `0x0D 0x0A`) or Windows open-file `unlink()` restrictions do not apply to Kbuild host tools.
- Using POSIX.1-2008 `struct stat` nanosecond fields (`st_mtim`, `st_atim`, `st_ctim`) in host programs (`scripts/`): do not report macOS/Darwin `st_mtimespec` incompatibility unless the file already maintains macOS `#ifdef` shims.
- Using standard coreutils/POSIX utilities (such as `seq`, `awk`, `sed`, `find`, `xargs`, `sort`) in Kbuild Makefiles or shell scripts, even if not individually listed in `Documentation/process/changes.rst`.

## Short-Lived Host Build Executables (`scripts/`, `tools/objtool/`)

Programs under `scripts/` (e.g., `kallsyms`, `modpost`, `sorttable`, `depcheck`, `fixdep`) and `tools/objtool/` are short-lived command-line build tools that run once per invocation and exit, allowing the OS to reclaim all memory and file descriptors.

**Do NOT report (false positives):**
- Memory leaks on fatal error paths or at process exit (such as `ptr = realloc(ptr, size)` losing the old pointer on OOM right before exiting, or early error returns in `main` / top-level command handlers skipping `free()`).
- File descriptor or `FILE *` leaks on fatal error paths that immediately terminate the process (such as `if (ferror(out) || fclose(out))` short-circuiting `fclose(out)` right before `unlink()` and `exit(1)`).
- Error-cleanup paths reachable only if an initial startup heap allocation (`malloc`/`calloc`) fails under OOM.
- Hypothetical malformed CLI inputs that Kbuild never generates (such as passing a filename literally equal to `".cmd"` when Kbuild only passes `.<target>.cmd` paths).

## GNU Make Rules, Kbuild Targets, and Linker Passes

**Do NOT report (false positives):**
- **Empty target variable before a single colon**: In GNU Make, a rule `$(var): prerequisite` where `$(var)` expands to empty (`: prerequisite`) is silently ignored as a no-op. It does NOT trigger `*** missing target pattern. Stop.` (which only applies to static pattern rules with two colons).
- **Multi-goal `MAKECMDGOALS` combinations**: Do not report build races or skipped sub-makes that only occur when a user combines multiple top-level goals on a single `make` command line (such as `make -j modules_prepare tar-pkg`, `make modules_prepare dtbs`, `make -j rustdoc rusttest`, or `make -j vmlinux rustdoc`) when each individual goal works as intended and `$(filter ...,$(MAKECMDGOALS))` is used as a heuristic for multi-goal invocations.
- **Linker relaxation with `--emit-relocs` on `vmlinux`**: Before claiming that omitting `--emit-relocs` on intermediate `.tmp_vmlinux*` kallsyms passes changes symbol offsets due to linker relaxation (e.g., on RISC-V), check the architecture Makefile (`arch/riscv/Makefile` passes `--no-relax` for `vmlinux`).

## Quick Checks

- **Python 3**: Do not enforce Python 2 compatibility on scripts
- **Tool requirements**: Verify tool dependencies against `Documentation/process/changes.rst`; standard coreutils (`seq`, etc.) and POSIX.1-2008 host APIs (`st_mtim`, `fopen("w")`) are always available
- **Short-lived build tools**: Do not flag OOM/exit-path memory or fd leaks in `scripts/` or `tools/objtool/`
- **C standard and CFLAGS**: Respect `gnu11`, unsigned `char`, `(void *)bool` casts, and domain-specific aliasing (`-fno-strict-aliasing` in kernel, `-fstrict-aliasing` in `tools/`)

