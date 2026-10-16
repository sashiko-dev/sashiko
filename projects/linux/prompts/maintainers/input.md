+++
title = "Input"
paths = ["drivers/input/", "include/linux/input.h", "include/linux/input/"]
symbols = ["input_dev", "input_handler", "input_register_*", "input_report_*"]
+++
# Input: maintainer notes

Style preferences and conventions of the input subsystem maintainer for drivers and core code under drivers/input/.

- The maintainer prioritizes explicit error handling, modern cleanup primitives and standard kernel coding style to reduce boilerplate and "gotcha" bugs.
- Prefer C-style comments `/* ... */` over C++ style `// ...`; multi-line comments use the standard kernel format (opening `/*` on its own line, ` * ` prefixed lines, closing ` */` on its own line).
- Suggest `input_set_capability(dev, type, code)` instead of direct bit manipulation (e.g. `__set_bit(code, dev->evbit)`) for single capabilities; direct bit manipulation is acceptable in loops or other repetitive constructs.
- Do not manually add the `EV_SYN`/`SYN_REPORT` capability; the core adds it, so doing it by hand is redundant.
- Suggest `guard(mutex)(&input_dev->mutex)` in new or significantly refactored code, and encourage `__free()` for local resource management (e.g. `u8 *buf __free(kfree) = ...`, `const struct firmware *fw __free(firmware) = NULL`).
- Name variables that only hold a negative error code or 0 `error` or `err`; functions using them should explicitly `return 0;` on the success path.
- In functions with multiple failure points, do not write `return action(...);` for an error-returning call; use the expanded form `error = action(...); if (error) { /* report if needed */ return error; } return 0;`.
- Use the specific reporting helpers (`input_report_key()`, `input_report_abs()`, `input_report_rel()`) instead of generic `input_event()` when the event type is known, and avoid reporting unnecessary/redundant events.
- With `devm_input_allocate_device(dev)`, do not manually assign `input_dev->dev.parent`; it is redundant and should be removed.
- Avoid mixing managed (devm) and regular resources in a driver.
- An explicit `input_unregister_device()` on a devm-allocated device is not a bug; when it is truly redundant, recommend removing it to simplify the code rather than reporting it as a defect.
