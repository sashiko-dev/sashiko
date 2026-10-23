+++
title = "Cleanup API"
paths = ["include/linux/cleanup.h"]
symbols = ["__free", "guard", "scoped_guard", "DEFINE_FREE", "DEFINE_GUARD", "no_free_ptr", "return_ptr"]
+++
# Cleanup API: maintainer notes

Conventions for using the scope-based cleanup helpers (__free, guard, scoped_guard) in kernel code.

- Do not mix goto-based error handling with cleanup helpers in the same function: for a given routine, convert all resources that need a goto cleanup to scope-based cleanup, or convert none of them.
- Define and initialize __free() variables in a single statement rather than declaring them "= NULL" at the top of the function and assigning them later.
