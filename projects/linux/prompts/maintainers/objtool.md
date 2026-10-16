+++
title = "Objtool"
paths = ["tools/objtool/"]
symbols = ["INSN_BUG", "INSN_TRAP"]
+++
# Objtool: maintainer notes

These notes cover what not to flag when reviewing objtool, a short-lived build tool that runs once per object file and exits.

- Do not report memory leaks on OOM or error-exit paths in tools/objtool/ (for example realloc overwriting the old pointer on failure, early error returns skipping free(), or allocations never freed before exit); objtool relies on the OS to reclaim memory at process exit.
- Do not report cleanup-path bugs that can only be reached when a startup heap allocation fails under OOM.
- Do not flag concurrent writes to a plain (non-bitfield) bool status flag from parallel worker passes when every thread only stores the same constant true value and the flag is read only after pthread_join().
