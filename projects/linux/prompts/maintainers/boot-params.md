+++
title = "Boot Parameters"
paths = ["Documentation/admin-guide/kernel-parameters.txt"]
symbols = ["__setup", "early_param", "module_param", "module_param_named", "core_param"]
+++
# Boot Parameters: maintainer notes

Documentation policy for patches that add or change kernel command-line, early, core or module parameters.

- Every boot parameter added or changed (via __setup(), early_param(), core_param(), module_param(), module_param_named() or any other option parsed from the command line at boot) must be documented in Documentation/admin-guide/kernel-parameters.txt in the same patch.
- Module parameters settable at boot as `<module>.<param>=` also belong in kernel-parameters.txt.
- Treat a patch that adds or changes a command-line parameter without updating kernel-parameters.txt as a regression to report.
- The kernel-parameters.txt entry should give the parameter name, its format (e.g. `tlbi=off` or `tlbi=[off]`), what it controls, and the architectures it applies to.
- Exception: debug-only parameters gated by CONFIG_DEBUG_KERNEL may instead be documented in a comment near the definition; flag them, but do not require a kernel-parameters.txt entry.
- Exception: parameters defined and consumed entirely within a single driver's experimental-gated code path with no user-facing documentation expectation do not require an entry, but still flag them for reviewer awareness.
