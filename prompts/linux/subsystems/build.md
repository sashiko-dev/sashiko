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

## Quick Checks

- **Python 3**: Do not enforce Python 2 compatibility on scripts
- **Tool requirements**: Verify tool dependencies against `Documentation/process/changes.rst`
- **C standard and CFLAGS**: Respect `gnu11`, unsigned `char`, and domain-specific aliasing (`-fno-strict-aliasing` in kernel, `-fstrict-aliasing` in `tools/`)
