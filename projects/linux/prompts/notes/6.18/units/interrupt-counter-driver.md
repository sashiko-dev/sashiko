# INTERRUPT COUNTER DRIVER

The interrupt counter driver (`drivers/counter/interrupt-cnt.c`) implements a generic Linux Counter subsystem device that counts rising-edge hardware interrupts (either directly provided as a platform IRQ or derived from an input GPIO via `gpiod_to_irq()`). It exposes a single counter channel (`Channel 0 Count`), signal, and synapse through the Counter framework (`devm_counter_alloc` / `devm_counter_add`), allowing userspace to enable/disable IRQ counting, read/write the count value, read optional GPIO signal levels, and receive `COUNTER_EVENT_CHANGE_OF_STATE` events on each interrupt.

## Architecture & Data Flow
- In `interrupt_cnt_probe()`, `priv->synapses.signal` must point into the `counter->signals` array (`&priv->signals`) because `counter_sysfs_synapses_add()` computes the 0-based signal index via pointer subtraction `synapse->signal - counter->signals`. Broken by: pointing `priv->synapses.signal` and `counter->signals` to distinct `struct counter_signal` instances. [F21]
- In `interrupt_cnt_probe()`, `gpiod_to_irq(priv->gpio)` is only called when `!priv->irq`, so a platform IRQ takes precedence over the GPIO's IRQ while `priv->gpio` remains populated for `interrupt_cnt_signal_read()`. Broken by: unconditionally overwriting `priv->irq` with `gpiod_to_irq(priv->gpio)` when `priv->gpio` is non-NULL, or skipping `devm_gpiod_get_optional()` when `priv->irq` is already set. [F24]
- Because `struct interrupt_cnt_priv` embeds single scalar instances of `signals` (`struct counter_signal`), `synapses` (`struct counter_synapse`), and `cnts` (`struct counter_count`), `counter->num_signals`, `priv->cnts.num_synapses`, and `counter->num_counts` in `interrupt_cnt_probe()` must not exceed 1 unless those fields are converted to arrays. [F30]

## Synchronization
- `interrupt_cnt_isr()` runs in hardirq context (`IRQF_NO_THREAD`) and calls `counter_push_event()`, which invokes watched `counter_ops` read callbacks under `spin_lock_irqsave(&counter->events_list_lock, flags)`. Therefore, `interrupt_cnt_isr()`, `interrupt_cnt_read()`, `interrupt_cnt_function_read()`, `interrupt_cnt_action_read()`, and `interrupt_cnt_signal_read()` must never sleep or acquire `priv->lock`; `interrupt_cnt_signal_read()` must use `gpiod_get_value()` rather than `gpiod_get_value_cansleep()`. [F38, F39, F40]
- `interrupt_cnt_isr()` must increment `priv->count` via `atomic_long_inc(&priv->count)` before calling `counter_push_event(counter, COUNTER_EVENT_CHANGE_OF_STATE, 0)`, paired with `atomic_long_read()` in `interrupt_cnt_read()` and `atomic_long_set()` in `interrupt_cnt_write()`. Broken by: calling `counter_push_event()` before `atomic_long_inc()`, or accessing `priv->count` with non-atomic loads or stores. [F9]
- `interrupt_cnt_probe()` must call `irq_set_status_flags(priv->irq, IRQ_NOAUTOEN)` before `devm_request_irq()` so the IRQ starts disabled to match `priv->enabled == false`, and must complete `devm_request_irq()`, `mutex_init(&priv->lock)`, and descriptor setup before publishing `counter` via `devm_counter_add()`. [F7, F14]
- In `interrupt_cnt_enable_write()` and `interrupt_cnt_enable_read()`, all accesses to `priv->enabled` and calls to `enable_irq(priv->irq)` or `disable_irq(priv->irq)` must be serialized under `priv->lock` and skipped when `priv->enabled == enable`. [F34]

## Object Lifetimes
- In `interrupt_cnt_probe()`, `devm_counter_alloc()` must precede `devm_request_irq()`, and `devm_counter_add()` must follow `devm_gpiod_get_optional()`, `devm_kasprintf()`, and `devm_request_irq()` so LIFO devres teardown unregisters `counter` and frees the IRQ before releasing `priv` and `counter`. [F1]
- In `interrupt_cnt_probe()`, the mutable per-device descriptors `priv->signals`, `priv->synapses`, and `priv->cnts` must live inside `struct interrupt_cnt_priv` (allocated via `devm_counter_alloc()`) and `priv->signals.name` via `devm_kasprintf()`, because `counter` retains borrowed pointers to them. Broken by: moving them into stack variables in `interrupt_cnt_probe()` or into shared `static` globals mutated during probe. [F4]
- In `interrupt_cnt_probe()`, `struct counter_device` and `struct interrupt_cnt_priv` must be allocated via `devm_counter_alloc(dev, sizeof(*priv))` (accessed via `counter_priv()`) and registered via `devm_counter_add()` rather than embedding `struct counter_device` in a `devm_kzalloc()` buffer; passing less than `sizeof(struct interrupt_cnt_priv)` (e.g., `sizeof(priv)`) is a bug because all driver callbacks interpret the `void *` from `counter_priv()` as `struct interrupt_cnt_priv *`. [F29, F31]

## API Contracts
- `devm_request_irq()` in `interrupt_cnt_probe()` must pass `counter` (`struct counter_device *`), not `priv` or `dev`, as the `dev_id` cookie because `interrupt_cnt_isr()` casts `dev_id` to `struct counter_device *` for both `counter_priv(counter)` and `counter_push_event()`. [F13]

## Error Handling
- `devm_counter_alloc()` returns `NULL` (not an `ERR_PTR`) on allocation or devres registration failure, so `interrupt_cnt_probe()` must check `!counter` and return `-ENOMEM` rather than using `IS_ERR()`/`PTR_ERR()`. [F10]
- In `interrupt_cnt_probe()`, `platform_get_irq_optional(pdev, 0)` returns `-ENXIO` when no platform interrupt is specified and other negative errno values on failure; only `-ENXIO` may be normalized to `priv->irq = 0` to trigger the `gpiod_to_irq()` fallback, while any other `priv->irq < 0` must abort probe. [F11]
- `interrupt_cnt_signal_read()` must return `-EINVAL` when `priv->gpio` is NULL, propagate negative error codes from `gpiod_get_value()`, and map non-negative returns strictly to `COUNTER_SIGNAL_LEVEL_LOW` or `COUNTER_SIGNAL_LEVEL_HIGH` because `counter_comp_u32_show()` indexes `counter_signal_value_str` directly. [F46]

## External Contracts
- `interrupt_cnt_watch_validate()` must reject any `struct counter_watch` whose `(event, channel)` pair is not emitted by `interrupt_cnt_isr()` via `counter_push_event()` (currently `channel == 0` and `event == COUNTER_EVENT_CHANGE_OF_STATE`) with `-EINVAL`, because `counter_add_watch()` does not validate `watch.event` or `watch.channel`. [F44]
- `interrupt_cnt_write()` must check `val != (typeof(priv->count.counter))val` and return `-ERANGE` when the Counter subsystem `u64` value exceeds the range representable by `priv->count` (`atomic_long_t`) before calling `atomic_long_set()`. [F45]

## Known Pitfalls
- Unlike `interrupt_cnt_of_match[]`, which must end with a zeroed `{}` sentinel entry, `interrupt_cnt_ext[]`, `interrupt_cnt_synapse_actions[]`, and `interrupt_cnt_functions[]` are sized with `ARRAY_SIZE()` in `interrupt_cnt_probe()` and must not include a trailing `{}` sentinel. [F22]
