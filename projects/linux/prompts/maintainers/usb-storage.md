+++
title = "USB Storage"
paths = ["drivers/usb/storage/", "drivers/usb/storage/unusual_devs.h"]
symbols = ["UNUSUAL_DEV", "USB_SC_*", "USB_PR_*"]
+++
# USB Storage: maintainer notes

These notes cover conventions for patches to the USB mass-storage driver, especially new or changed unusual_devs.h entries.

- New UNUSUAL_DEV() entries should use USB_SC_DEVICE, USB_PR_DEVICE for subclass and protocol by default. Use explicit USB_SC_* / USB_PR_* overrides only when the device actually needs them, and avoid redundant overrides.
- When a patch adds an UNUSUAL_DEV() entry with explicit USB_SC_* / USB_PR_* values instead of USB_SC_DEVICE / USB_PR_DEVICE, the commit message should explain why the override is needed.
