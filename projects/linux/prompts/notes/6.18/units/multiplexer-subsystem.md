# MULTIPLEXER SUBSYSTEM

The Multiplexer (mux) subsystem provides a generic framework for managing hardware multiplexers shared across multiple consumer devices. Provider drivers register `struct mux_chip` instances with `struct mux_control_ops` callbacks (such as I2C ADG792A, GPIO, and MMIO/regmap bitfield muxes), while consumer drivers acquire `struct mux_control` or `struct mux_state` handles via Device Tree phandles and serialize state selection using per-controller semaphores.

## Architecture & Data Flow

- Every hardware state transition must go through `mux_control_set()`, which updates `mux->cached_state` in lockstep with `mux->chip->ops->set(mux, state)`—setting `mux->cached_state = state` and `mux->last_change = ktime_get()` on success (`ret >= 0`), or invalidating `mux->cached_state` to `MUX_CACHE_UNKNOWN` (`MUX_IDLE_AS_IS`) on error (`ret < 0`). [F29]
- `mux_control_set()` and `mux_control_ops.set` must never be called with `MUX_IDLE_AS_IS` (`-1`), which aliases `MUX_CACHE_UNKNOWN`; `mux_chip_register()`, `__mux_control_select()`, and `mux_control_deselect()` must skip `mux_control_set()` whenever the target idle state is `MUX_IDLE_AS_IS`. [F16]
- `mux_control_select_delay()` and `mux_control_try_select_delay()` must call `mux_control_delay(mux, delay_us)` whenever `__mux_control_select()` returns `>= 0`—even when `mux->cached_state == state` skipped `mux_control_set()`—so any remaining settling time since `mux->last_change` is enforced. [F32]
- Consumer-specific target state parsed from `#mux-state-cells` must be stored in a per-consumer `struct mux_state` allocated in `mux_state_get()`, never on the shared `struct mux_control` (`mux_chip->mux[controller]`) returned by `mux_get()`. [F34]
- In `mux_chip_alloc()`, total allocation size for `struct mux_chip`, its `mux[]` flexible array (`__counted_by(controllers)`), and trailing private data must be computed with `size_add(struct_size(mux_chip, mux, controllers), sizeof_priv)`, and `mux_chip->controllers` must be assigned before indexing `mux_chip->mux[]` or calling `mux_chip_priv()`. [F38, F51]
- Only `struct device` instances embedded as `dev` in `struct mux_chip` may be registered on `mux_class`, because `of_find_mux_chip_by_node()` unconditionally applies `to_mux_chip()` to matches from `class_find_device_by_of_node(&mux_class, np)`. [F60]
- In `adg792a_set()`, global commands (`ADG792A_DISABLE_ALL`, `ADG792A_MUX_ALL(state)`) may only be used when `mux->chip->controllers == 1`; when `mux->chip->controllers == 3`, per-controller commands (`ADG792A_DISABLE(controller)`, `ADG792A_MUX(controller, state)`) indexed by `mux_control_get_index(mux)` must be used. [F36]
- In `mux_gpio_probe()`, the GPIO count from `gpiod_count()` must be checked for negative errno values (`pins < 0`) before computing `mux_chip->mux->states = BIT(pins)`. [F52]
- In `drivers/mux/mmio.c`, each controller `mux_chip->mux[i]` must be backed by its own `struct regmap_field *fields[i]` validated to have a contiguous bitmask (`mask == GENMASK(field.msb, field.lsb)`) and written via `regmap_field_write(fields[mux_control_get_index(mux)], state)` in `mux_mmio_set()`, never full-register writes. [F35]

## Synchronization

- `mux_control_select_delay()` and `mux_control_try_select_delay()` (and their `mux_state_*` wrappers) return with `mux->lock` held if and only if they return `0`, calling `up(&mux->lock)` internally when `__mux_control_select()` fails; callers must call `mux_control_deselect()` or `mux_state_deselect()` only on success. [F4]
- `mux_control_deselect()` and `mux_state_deselect()` unconditionally release `mux->lock` via `up(&mux->lock)` even if restoring `mux->idle_state` via `mux_control_set()` fails. Broken by: returning early before `up(&mux->lock)` on error, or retrying deselect after a non-zero return. [F5]
- `__mux_control_select()` and `mux_control_delay()` access `mux->cached_state`, `mux->last_change`, and hardware state without internal locking and must only be called with `mux->lock` held (`down_killable()` or `down_trylock()`). [F11]
- `mux_control_ops.set` callbacks are invoked with `mux->lock` held during `mux_control_select_delay()`, `mux_control_try_select_delay()`, and `mux_control_deselect()`, so `.set()` implementations must never acquire `mux->lock` or call selection functions on the same `mux_control`. [F14]
- `mux_control_ops.set` callbacks execute in sleepable process context, and `mux_control_delay()` calls `fsleep()`, so consumer selection and deselection APIs must not be called from atomic context when the backing provider sleeps (e.g., GPIO cansleep or I2C) or `delay_us > 0`. [F70]
- Between `mux_chip_alloc()` and `mux_chip_register()` (or their `devm_` variants), provider drivers must initialize `mux_chip->ops`, each controller's `states` (and optional `idle_state`), and all private data and hardware resources (GPIOs, regulators, `regmap_field` handles) before registration calls `mux_control_set()` and `device_add()`, and must never write `mux->cached_state` directly. [F9, F20]

## Object Lifetimes

- In `mux_chip_alloc()`, once `device_initialize(&mux_chip->dev)` is called, the `mux_chip` allocation and its `mux_chip->id` in `mux_ida` are owned by `mux_chip->dev` (`mux_chip_release()`) and must be freed via `mux_chip_free()` (`put_device()`), whereas error paths before `device_initialize()` must free `mux_chip` via `kfree(mux_chip)` directly without leaking `mux_chip->id`. [F1]
- During teardown, `mux_chip_unregister()` must unpublish `mux_chip->dev` via `device_del()` before `mux_chip_free()` drops the device reference and before devres or remove paths release provider resources used by `mux_chip->ops->set`. Broken by: allocating `devm_mux_chip_alloc()` or `devm_*` hardware resources after `devm_mux_chip_register()`, or calling `mux_chip_free()` before `mux_chip_unregister()`. [F15]
- In `mux_get()`, `args.np` from `of_parse_phandle_with_args()` must be released with `of_node_put(args.np)`, and the `mux_chip->dev` reference from `of_find_mux_chip_by_node()` must be dropped via `put_device(&mux_chip->dev)` on every subsequent error path or transferred to the caller (`mux_control_put()`) on success. [F6]
- `mux_state_get()` must save `PTR_ERR(mstate->mux)` and call `kfree(mstate)` if `mux_get()` fails, and `mux_state_put()` must call `mux_control_put(mstate->mux)` before `kfree(mstate)`. [F7]
- In `devm_mux_chip_alloc()`, `devm_mux_chip_register()`, `devm_mux_control_get()`, and `devm_mux_state_get()`, the node from `devres_alloc()` must be freed with `devres_free(ptr)` if the underlying call fails and registered with `devres_add(dev, ptr)` only after it succeeds. [F8]

## API Contracts

- A `struct mux_chip` must never be passed to `mux_chip_register()` again after a failed `mux_chip_register()` call or after `mux_chip_unregister()`; the caller must release it with `mux_chip_free()` and allocate a new one with `mux_chip_alloc()`. [F2]
- `mux_control_ops.set` receives a signed `int state` in `[0, mux->states)` or `MUX_IDLE_DISCONNECT` (`-2`); `.set()` implementations supporting disconnect (such as `adg792a_set()`) must check `state == MUX_IDLE_DISCONNECT` before bitwise-encoding `state`. [F17]
- In `mux_get()`, `mux_name` may be `NULL` (leaving `index = 0` and logging `mux_name ?: ""`), and `state` is both an out-parameter and the mode selector choosing `"mux-states"`/`"#mux-state-cells"`/`"mux-state-names"` when non-`NULL` versus `"mux-controls"`/`"#mux-control-cells"`/`"mux-control-names"` when `NULL`. [F26]
- The `delay_us` parameter to `mux_control_select_delay()`, `mux_state_select_delay()`, `mux_control_try_select_delay()`, and `mux_state_try_select_delay()` is in microseconds and is measured elapsed from `mux->last_change`. [F80]

## Error Handling

- When `mux_control_set(mux, state)` fails in `__mux_control_select()` and `mux->idle_state != MUX_IDLE_AS_IS`, `__mux_control_select()` must attempt to revert to `mux->idle_state` via `mux_control_set(mux, mux->idle_state)` without overwriting the original negative error return `ret`. [F31]
- `mux_chip_alloc()`, `mux_get()`, `mux_control_get()`, `mux_state_get()`, and their `devm_` variants always return `ERR_PTR(-errno)` on error and never `NULL`, whereas `of_find_mux_chip_by_node()` returns `NULL` when not found (converted to `ERR_PTR(-EPROBE_DEFER)` by `mux_get()`). [F23]
- In `mux_chip_alloc()` and `mux_state_get()`, error codes stored inside the struct (`mux_chip->id` or `PTR_ERR(mstate->mux)`) must be copied to a local variable before `kfree(mux_chip)` or `kfree(mstate)`. [F27]
- When reading `"idle-state"` or `"idle-states"` via `device_property_read_u32*()` or `of_property_read_u32_index()`, `adg792a_probe()`, `mux_gpio_probe()`, and `mux_mmio_probe()` must store the value in a signed `s32` so `MUX_IDLE_AS_IS` (`-1`), `MUX_IDLE_DISCONNECT` (`-2`), and `idle_state < 0` checks remain signed. [F18]
- In `mux_mmio_probe()`, when not compatible with `"mmio-mux"` and `regmap_init_mmio()` fails, the fallback `dev_get_regmap(dev->parent, NULL)` must only run when `regmap != ERR_PTR(-EPROBE_DEFER)` (preserving `-EPROBE_DEFER`), and a `NULL` return from `dev_get_regmap()` must be converted to `ERR_PTR(-ENODEV)` before checking `IS_ERR(regmap)`. [F24, F49]
- In `mux_gpio_probe()`, `devm_regulator_get_enable_optional(dev, "mux")` returns `-ENODEV` when the optional supply is absent, so the check must be `if (ret && ret != -ENODEV)`. [F25]
- `WARN_ON()` is reserved for kernel API violations in `mux_chip_alloc()`, `__mux_control_select()`, and `mux_gpio_probe()`; Device Tree errors, `-EPROBE_DEFER`, and hardware I/O failures must return negative errnos or `ERR_PTR` without `WARN_ON()`. [F81]

## External Contracts

- In `mux_get()`, `#mux-control-cells` must be 0 or 1 and `#mux-state-cells` must be 1 or 2, with the controller index cell required whenever `mux_chip->controllers > 1` and checked to be `< mux_chip->controllers` before indexing `mux_chip->mux[]`. [F39]
- In `mux_mmio_probe()`, `mux-reg-masks` must have a positive even number of `u32` elements (`2 * num_fields`), and each shifted mask at `2 * i + 1` must be converted to 0-based `struct reg_field` bit indices via `field.msb = fls(mask) - 1` and `field.lsb = ffs(mask) - 1` before checking `mask == GENMASK(field.msb, field.lsb)`. [F28, F41]

## Known Pitfalls

- Because `mux_chip_alloc()` sets `mux_chip->dev.parent = dev` on the `mux_class` device, provider callbacks must recover the bus device via `mux->chip->dev.parent` (as in `adg792a_set()`) and register devres on `dev`, while passing `&mux_chip->dev` (not `dev`) to `put_device()`. [F48]
- `devm_mux_chip_release()`, `devm_mux_chip_reg_release()`, `devm_mux_control_release()`, and `devm_mux_state_release()` must dereference `res` as a double pointer (e.g., `*(struct mux_chip **)res`) because `devres_alloc()` allocates a `sizeof(*ptr)` pointer slot. [F58]
- In `__mux_control_select()`, `state` is a signed `int` and must be checked for both `state < 0` and `state >= mux->states` so unsigned consumer states exceeding `INT_MAX` and out-of-range states are rejected. [F55]
- In `mux_control_delay()`, remaining settling time must be computed as a signed `s64` via `ktime_us_delta(delayend, ktime_get())` and guarded by `remaining > 0` before calling `fsleep(remaining)`. [F50]
- In `mux_gpio_probe()` and `mux_mmio_probe()`, `idle_state != MUX_IDLE_AS_IS` must be checked before `idle_state < 0 || idle_state >= states`, so `MUX_IDLE_AS_IS` (`-1`) is accepted while `MUX_IDLE_DISCONNECT` (`-2`) and other negative values are rejected. [F45]
- In `mux_mmio_probe()`, `s32 idle_state = MUX_IDLE_AS_IS` must be re-initialized on every iteration of the `for (i = 0; i < num_fields; i++)` loop because `of_property_read_u32_index(np, "idle-states", i, (u32 *)&idle_state)` leaves `idle_state` untouched on failure. [F19]
- In `mux_gpio_set()`, the 32-bit `state` must be converted to an `unsigned long` bitmap via `bitmap_from_arr32()` before calling `gpiod_multi_set_value_cansleep()`, never cast directly to `(unsigned long *)`. [F42]
- In `adg792a_set()`, the 0-based `controller` from `mux_control_get_index(mux)` is used directly in `ADG792A_DISABLE(controller)` (`0x50 | controller`) but needs `+ 1` in `ADG792A_MUX(controller, state)` (`0xc0 | (((controller) + 1) << 2) | (state)`) to avoid encoding `ADG792A_MUX_ALL`. [F43]

## Other

- In `adg792a_probe()`, `i2c_check_functionality(i2c->adapter, I2C_FUNC_SMBUS_BYTE_DATA)` must be verified before `adg792a_write_cmd()`, and `adg792a_write_cmd(i2c, ADG792A_DISABLE_ALL, 1)` must reset the chip before `devm_mux_chip_register()` programs any controller `idle_state`. [F69]
