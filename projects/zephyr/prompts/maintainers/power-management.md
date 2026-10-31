+++
title = "Device Power Management"
paths = [
    "subsys/pm/",
    "include/zephyr/pm/",
]
symbols = [
    "PM_DEVICE_*",
    "pm_device_*",
]
+++

# Device Power Management

Applies to drivers with PM support and to `subsys/pm/`.

Source: doc/services/pm/device_runtime.rst; include/zephyr/pm/device_runtime.h.

- With device runtime PM, drivers say when a device is needed with
  `pm_device_runtime_get()` and when it is not with `pm_device_runtime_put()`.
  These are reference-counted: every successful get needs one put, including on
  error paths. A leaked get keeps the device powered, and an extra put can suspend
  it during use.
- Check the return value of `pm_device_runtime_get()`. The device is not usable if
  the resume failed, and the call returns `-EWOULDBLOCK` when it would have to
  block in an ISR.
- `pm_device_runtime_put()` can suspend synchronously, which is a problem when
  suspending involves a slow bus. `pm_device_runtime_put_async()` schedules it
  instead, and is the usual choice where the caller must not wait.
- Applications do not suspend or resume devices manually while runtime PM is
  enabled on them. Flag code that does.
