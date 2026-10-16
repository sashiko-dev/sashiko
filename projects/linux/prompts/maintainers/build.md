+++
title = "Build System"
paths = ["Makefile", "Kbuild", "scripts/", "tools/", "*/Makefile", "*/Kbuild"]
symbols = []
+++
# Build System: maintainer notes

Review conventions and false-positive policies for Kbuild, Makefiles, and host build tools under scripts/ and tools/.

- Python scripts (scripts/, tools/, Documentation/) are Python 3 only; do not report Python 2 incompatibilities such as type annotations, print() or f-strings.
- Minimal required tool versions are the ones documented in Documentation/process/changes.rst; do not assume older legacy tool versions must be supported unless an architecture constraint requires it.
- Kernel C is written in GNU C11 (gnu11) per Documentation/process/programming-language.rst; do not flag GNU extensions (statement expressions, typeof, zero-length arrays, case ranges) as non-standard.
- Host build tooling targets POSIX-compliant Linux/BSD hosts with POSIX.1-2008 and standard coreutils; do not raise Windows portability concerns (binary vs text fopen modes, CRLF translation, unlink of open files).
- Do not report macOS/Darwin incompatibilities (e.g. st_mtim vs st_mtimespec) in host programs unless the file already maintains macOS #ifdef shims.
- Using standard coreutils/POSIX utilities (seq, awk, sed, find, xargs, sort) in Kbuild Makefiles or shell scripts is acceptable even if they are not individually listed in changes.rst.
- Programs under scripts/ and tools/objtool/ are short-lived build tools that exit after one run; do not report memory or file descriptor leaks on fatal error paths or at process exit, or cleanup issues reachable only when an initial startup allocation fails.
- Do not report problems that require hypothetical malformed command-line inputs that Kbuild never generates.
- Do not report build races or skipped sub-makes that only occur when a user combines multiple top-level goals on one make command line, as long as each goal works on its own and $(filter ...,$(MAKECMDGOALS)) is used as a heuristic for multi-goal invocations.
