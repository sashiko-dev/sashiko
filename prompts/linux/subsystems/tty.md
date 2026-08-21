# TTY/Serial Subsystem Details

## UART Device Registration and Callback Timing

Calling runtime PM APIs from callbacks before runtime PM is enabled causes
circular dependencies during device probe, resulting in blocked tasks and
hung worker threads. The probe function blocks indefinitely waiting for
runtime PM operations that cannot complete because runtime PM infrastructure
is not yet initialized.

`uart_add_one_port()` may synchronously invoke driver callbacks during
registration, including the `uart_ops->pm()` callback via `uart_change_pm()`.
If these callbacks call `pm_runtime_resume_and_get()` or
`pm_runtime_put_sync()` before runtime PM is enabled on the device, the
runtime PM core attempts operations on an uninitialized device, leading to
circular wait conditions.

When converting a serial driver to use runtime PM, `pm_runtime_enable()` or
`devm_pm_runtime_enable()` must be called BEFORE `uart_add_one_port()` if the
driver's `uart_ops->pm` callback invokes any runtime PM APIs.

```c
// WRONG - runtime PM enabled after port registration
ret = uart_add_one_port(drv, uport);    // May call ops->pm()
if (ret)
    return ret;
devm_pm_runtime_enable(dev);            // Too late - callback already invoked

// CORRECT - runtime PM enabled before port registration
devm_pm_runtime_enable(dev);            // Enable first
ret = uart_add_one_port(drv, uport);    // Now safe for ops->pm() to use runtime PM
if (ret)
    return ret;
```

**Callbacks invoked during `uart_add_one_port()`:**

These are called from `uart_configure_port()` in `drivers/tty/serial/serial_core.c`,
which is reached via `uart_add_one_port()` -> `serial_ctrl_register_port()` ->
`serial_core_register_port()` -> `serial_core_add_one_port()`:

- `uart_ops->config_port()` - called when `UPF_BOOT_AUTOCONF` is set in `port->flags`
- `uart_ops->pm()` - via `uart_change_pm()` to power on the port, and to power it off again for non-console ports
- `uart_ops->set_mctrl()` - to de-activate modem control lines after power-on
- `port->rs485_config()` - via `uart_rs485_config()` if `SER_RS485_ENABLED` is set

## 8250 Module Architecture

Moving code between files in the 8250 serial driver without understanding the
module structure causes undefined symbol errors at link time or circular module
dependencies that break depmod. These errors only manifest when building with
`CONFIG_SERIAL_8250=m` (modular configuration), not when built-in.

The 8250 driver is split across two kernel modules with a unidirectional
dependency. The composition is defined in `drivers/tty/serial/8250/Makefile`:

| Module | Object files | Role |
|--------|--------------|------|
| `8250_base.ko` | `8250_port.o` (always); `8250_dma.o`, `8250_dwlib.o`, `8250_fintek.o`, `8250_pcilib.o`, `8250_rsa.o` (conditional on CONFIG) | Shared base functionality |
| `8250.ko` | `8250_core.o`, `8250_platform.o` (always); `8250_pnp.o` (conditional on `CONFIG_SERIAL_8250_PNP`) | Main driver |

**Dependency direction:** `8250.ko` depends on `8250_base.ko` (unidirectional).
`8250_port.c` exports symbols consumed by `8250_core.c`. Code in `8250_base.ko`
MUST NOT call symbols from `8250.ko`; doing so creates a circular dependency.

When reviewing patches that move functions between `.c` files in
`drivers/tty/serial/8250/`, consult the `Makefile` to determine which `.o`
files belong to which `.ko` module (look for `8250-y +=` vs `8250_base-y +=`
assignments). If a function moves from one module to the other but is still
called from code in the original module, it must be exported with
`EXPORT_SYMBOL()` or `EXPORT_SYMBOL_GPL()`. If it moves from `8250_base.ko` to
`8250.ko` while still being called from `8250_base.ko`, this introduces a
reverse dependency that breaks the unidirectional design.

Architectural solutions for reverse-dependency problems:
- Move the code back into `8250_base.ko` to preserve the unidirectional dependency
- Use function pointer indirection: pass callbacks at initialization instead of
  direct symbol references
- Create a registration function in `8250_base.ko` that stores pointers for later use

## Quick Checks

- **Runtime PM in `uart_ops` callbacks**: verify `pm_runtime_enable()` is called
  before `uart_add_one_port()` if any callback uses runtime PM APIs
- **Callback invocation during registration**: `uart_add_one_port()` may invoke
  `pm()`, `config_port()`, `set_mctrl()`, and `rs485_config()` synchronously
  during port registration
- **8250 cross-module code motion**: when code moves between files in
  `drivers/tty/serial/8250/`, check the `Makefile` to verify module boundaries
  are not violated
