+++
title = "PM Runtime"
paths = ["include/linux/pm_runtime.h"]
symbols = ["pm_runtime_*", "__pm_runtime_*", "rpm_idle", "rpm_suspend", "rpm_resume"]
+++
# PM Runtime: maintainer notes

These notes cover the preferred idioms for wiring power management callbacks into drivers.

- Wrap PM callback pointers with the helper macros from include/linux/pm.h rather than assigning them unconditionally, so the callbacks become dead code that can be dropped when PM is disabled: pm_sleep_ptr() for system sleep callbacks (suspend/resume/freeze/thaw/poweroff/restore) and pm_ptr() for the dev_pm_ops structure pointer (e.g. .driver.pm = pm_ptr(&foo_pm_ops)).
- When a patch adds PM sleep callbacks to a dev_pm_ops, check that the structure uses pm_sleep_ptr() for those callbacks and pm_ptr() for the structure pointer.
