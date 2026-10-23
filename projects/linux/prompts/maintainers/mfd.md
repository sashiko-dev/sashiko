+++
title = "Multi-Function Devices (MFD)"
paths = ["drivers/mfd/", "include/linux/mfd/"]
symbols = ["mfd_add_devices", "devm_mfd_add_devices", "mfd_cell", "mfd_remove_devices"]
+++
# Multi-Function Devices (MFD): maintainer notes

These notes cover the MFD maintainers' preferences for commit messages, naming, driver placement and driver structure.

- Commit subjects use the prefix format "mfd: <driver>: <Capitalized description>" (e.g. "mfd: max77650: Add support for MAX77650"); the description after the prefix is always capitalized (the same applies to the LED and Backlight subsystems).
- Do not hard-code implementation details in driver, struct or device names; avoid putting the string "mfd" or the driver's own filename in names.
- Name the private data structure after the device (e.g. struct max77650) and call the instance variable ddata, rather than generic names like info or priv.
- MFD is treated as an API, not just a physical layout: do not use it for simple single-function devices; use it only for devices registering multiple children in different subsystems (via the MFD API or of_platform_populate()).
- New core MFD drivers, and new calls to mfd_add_devices()/devm_mfd_add_devices(), belong in drivers/mfd/; existing callers elsewhere are not to be flagged.
- For simple MFDs, consider whether standard DT compatibles such as "simple-mfd" or "simple-pm-bus" can be used instead of writing a custom driver.
- The core MFD driver should only handle core resources (IRQs, regmap); child drivers should get parent data via standard APIs (e.g. dev_get_drvdata(pdev->dev.parent)) rather than bespoke parent-provided accessors or helper functions.
- Initialize private resources (regmap, clock handles, etc.) in the child driver that consumes them, not in the parent, unless the resource is genuinely shared by multiple children.
- In new code, do not pass child platform data such as mfd_cell arrays through the .data field of of_device_id, spi_device_id or similar match tables; store an enum or integer ID in .data and select the right static const mfd_cell array with a switch in probe. This is a preference for new code; existing drivers doing otherwise are not bugs.
- Define mfd_cell arrays as static const where possible, and do not create local copies of cells for dynamic amendments; use static references.
- Avoid inter-driver or driver-level callbacks between sibling child devices (e.g. RTC calling into the regulator driver under the same parent), and do not expose driver-level callbacks that bypass standard kernel subsystem APIs.
- Prefer PLATFORM_DEVID_AUTO for cell indexing; if a subdevice needs explicit numbering (e.g. serial/tty lines), use platform_device->id rather than the mfd_cell ID.
- Do not place driver-specific headers in the global include/linux/mfd/ directory if they are only used by the parent and its immediate children; keep them local to drivers/mfd/.
