+++
title = "Boot Parameters"
paths = ["Documentation/admin-guide/kernel-parameters.txt"]
symbols = ["__setup", "early_param", "module_param", "module_param_named", "core_param"]
+++
# Boot Parameters: maintainer notes

Documentation policy for patches that add or change kernel command-line, early, core or module parameters.

- Every boot parameter added or changed via __setup(), early_param(), core_param() or any other option parsed from the command line at boot, and every module_param()/module_param_named() in code that cannot be built as a loadable module (core kernel code, or objects whose Kconfig symbol is bool), must be documented in Documentation/admin-guide/kernel-parameters.txt in the same patch.
- Treat a patch that adds or changes such a parameter without updating kernel-parameters.txt as a regression to report.
- A module_param() in a driver whose Kconfig symbol is tristate does not need a kernel-parameters.txt entry, even though it can be passed as `<module>.<param>=` when built in; per-driver parameters are documented by their subsystem (for example Documentation/sound/alsa-configuration.rst for snd-* modules, Documentation/watchdog/watchdog-parameters.rst for watchdog drivers). Ask for a kernel-parameters.txt entry only when that module already has `<module>.` entries there (check with git_grep). When the subsystem's parameter document already lists the driver, report a missing entry for the new parameter there as Low.
- The kernel-parameters.txt entry should give the parameter name, its format (e.g. `tlbi=off` or `tlbi=[off]`), what it controls, and the architectures it applies to.
- Exception: debug-only parameters gated by CONFIG_DEBUG_KERNEL may instead be documented in a comment near the definition; flag them, but do not require a kernel-parameters.txt entry.
- Exception: parameters defined and consumed entirely within a single driver's experimental-gated code path with no user-facing documentation expectation do not require an entry, but still flag them for reviewer awareness.
