+++
title = "LEDs"
paths = ["drivers/leds/", "include/linux/leds.h"]
symbols = ["led_classdev_register", "devm_led_classdev_register"]
+++
# LEDs: maintainer notes

Maintainer preferences and conventions for patches to the LED subsystem and LED drivers.

- Commit subjects use the form "leds: <driver>: <Capitalized description>" (e.g. "leds: qwerty: Add support for Qwerty LED"); the description after the prefix is always capitalized, as in the MFD and Backlight subsystems.
- Name a driver's private data structure after the device (e.g. struct qwerty_led) and call the variable instance ddata; avoid generic names such as info or priv.
- Prefer managed registration (devm_led_classdev_register() / devm_led_classdev_register_ext()) over unmanaged led_classdev_register().
- Prefer devm_led_trigger_register() for custom triggers where possible.
- In Device Tree, describe LEDs with the color and function properties rather than label; label is deprecated and only acceptable for legacy drivers or backwards compatibility.
- Drivers should not print success messages (e.g. "LED registered successfully", "Probe success"); only log errors or warnings.
- Report probe failures with dev_err_probe().
