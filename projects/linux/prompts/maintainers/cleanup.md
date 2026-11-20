+++
title = "Cleanup API"
paths = ["include/linux/cleanup.h"]
symbols = ["__free", "guard", "scoped_guard", "DEFINE_FREE", "DEFINE_GUARD", "no_free_ptr", "return_ptr"]
+++
# Cleanup API: maintainer notes

Conventions for using the scope-based cleanup helpers (__free, guard, scoped_guard) in kernel code.

- Do not mix goto-based error handling with cleanup helpers in the same function: for a given routine, convert all resources that need a goto cleanup to scope-based cleanup, or convert none of them.
- Define and initialize __free() variables in a single statement rather than declaring them "= NULL" at the top of the function and assigning them later.
- Report a violation of either convention as Low at most; many maintainers do not treat them as requirements. Do not report them when a bug fix only adds a guard(), scoped_guard() or __free() variable to a function that keeps its existing goto unwinding, since converting the rest of the function is out of scope for the fix, and do not report the "= NULL" form when a single-statement definition would give the variable the wrong scope (for example an allocation inside a loop or under a condition).
- In drivers/gpio/ the maintainers ask patches to follow both conventions; report violations there as Low unless a single-statement definition would give the variable the wrong scope.
- Concrete bugs caused by mixing are not style issues and are reported at the severity of their effect: a goto that jumps past the definition of a __free(), guard(), scoped_guard() or CLASS() variable into its scope, code under a goto label that uses an object a __free() variable has already freed, or label code that must not run under a lock a guard() still holds.
