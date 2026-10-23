# RENESAS RIIC DRIVER

The Renesas RIIC driver (`drivers/i2c/busses/i2c-riic.c`) implements an I2C bus controller adapter for Renesas RZ/A, RZ/V2H, and RZ/T2H SoCs. It drives I2C master transfers through an interrupt-driven state machine across transmit, receive, transmit-end, NACK, and stop interrupts, with support for runtime PM, system sleep suspend/resume, reset control, and SCL/SDA bus recovery.

## Architecture & Data Flow

- When receiving the final byte (`riic->bytes_left == 1`) in `riic_rdrf_isr()`, enabling `ICIER_SPIE` in `RIIC_ICIER` and writing `ICCR2_SP` to `RIIC_ICCR2` (when `riic->is_last` is true) must precede setting `ICMR3_ACKBT` in `RIIC_ICMR3`, and both must precede reading `RIIC_ICDRR`. [F60]
- `RIIC_ICMR3` must retain `ICMR3_ACKWP | ICMR3_RDRFS` set by `riic_init_hw()` across all updates; `riic_rdrf_isr()` relies on read-modify-write via `riic_clear_set_bit()` to toggle `ICMR3_ACKBT` while keeping `ICMR3_ACKWP` (write-enable for `ACKBT`) and `ICMR3_RDRFS` active. Broken by: overwriting `RIIC_ICMR3` with `riic_writeb()` or clearing `ICMR3_ACKWP`/`ICMR3_RDRFS`. [F27]
- `riic_dev.bytes_left` must be a signed `int` guarded by `!riic->bytes_left` (not `riic->bytes_left <= 0`) on entry to `riic_tdre_isr()` and `riic_rdrf_isr()`, because `riic_xfer()` initializes `riic->bytes_left` to `RIIC_INIT_MSG` (`-1`) for the address and dummy-read phase. [F36]
- In `riic_tdre_isr()`, `riic->bytes_left == 0` must be checked after the `RIIC_INIT_MSG` branch sets `riic->bytes_left = riic->msg->len` and before writing `RIIC_ICDRT`, so zero-length (`SMBUS_QUICK`) writes switch from `ICIER_TIE` to `ICIER_TEIE` without dereferencing `riic->buf` or underflowing `riic->bytes_left`. Broken by: moving `if (riic->bytes_left == 0)` into the `else` branch of `if (riic->bytes_left == RIIC_INIT_MSG)`. [F37]
- In `riic_tend_isr()`, `riic_readb(riic, RIIC_ICSR2) & ICSR2_NACKF` must be evaluated before the `riic->bytes_left` check that returns `IRQ_NONE`, because `riic_tend_isr()` handles NACK interrupts where `riic->bytes_left` is still non-zero (`RIIC_INIT_MSG` or remaining payload bytes). [F43]
- Changes to NACK or STOP handling in `riic_tend_isr()` and `riic_stop_isr()` or to `RIIC_ICSR2` status bits (`ICSR2_NACKF`, `ICSR2_STOP`) must keep `riic_eei_isr()` in sync, because `riic_irqs` routes NACK and STOP to separate IRQs while `riic_rzt2h_irqs` multiplexes both through `riic_eei_isr()`. [F33]
- In `riic_bus_barrier()`, after `readb_poll_timeout` confirms `ICCR2_BBSY` is cleared in `RIIC_ICCR2`, the driver must verify both `ICCR1_SDAI` and `ICCR1_SCLI` are set (`(riic_readb(riic, RIIC_ICCR1) & (ICCR1_SDAI | ICCR1_SCLI)) == (ICCR1_SDAI | ICCR1_SCLI)`) and call `i2c_recover_bus()` if either bit is 0. Broken by: testing `!(riic_readb(riic, RIIC_ICCR1) & (ICCR1_SDAI | ICCR1_SCLI))`, which only recovers when both lines are low. [F45]
- All MMIO register accesses must translate `enum riic_reg_list` indices through `riic->info->regs` (via `riic_readb()`, `riic_writeb()`, `riic_clear_set_bit()`, or `riic->info->regs[...]` in `riic_bus_barrier()`), and every entry up to `RIIC_REG_END` must be populated in both `riic_rz_a_regs` and `riic_rz_v2h_regs`. [F64]
- In `riic_init_hw()`, `riic->i2c_t.bus_freq_hz` must be validated against `I2C_MAX_FAST_MODE_PLUS_FREQ` when `riic->info->fast_mode_plus` is true and `I2C_MAX_FAST_MODE_FREQ` otherwise, and both the initial `DIV_ROUND_UP(rate, t->bus_freq_hz ?: 1)` (guarding against zero `bus_freq_hz`) and each `cks` prescaler halving step (`DIV_ROUND_UP(total_ticks, 2)`) must use ceiling division so the bus never runs faster than `t->bus_freq_hz`. [F38, F49]
- In `riic_init_hw()`, the prescaler search loop must iterate inclusively over `0 <= cks <= 7` (`ICMR1_CKS`) until `brl <= (0x1F + 3)` and return `-EINVAL` if `brl > (0x1F + 3)` after the loop, matching the 5-bit `RIIC_ICBRL` max (`0x1F`) plus 3 sync/noise ticks. [F39]

## Synchronization

- In `riic_xfer()`, per-message state (`riic->bytes_left`, `riic->buf`, `riic->msg`, `riic->is_last`) and `riic->msg_done` must be initialized before enabling interrupts in `RIIC_ICIER` and issuing `ICCR2_ST` or `ICCR2_RS` in `RIIC_ICCR2`, and must not be modified by process context until `wait_for_completion_timeout()` returns. [F8]
- In `riic_tend_isr()`, `ICIER_TEIE` must be cleared in `RIIC_ICIER` on both the STOP path and the non-final message path, and when `!riic->is_last && !riic->err`, `complete(&riic->msg_done)` must be called immediately without issuing `ICCR2_SP`. [F10]
- Whenever `riic_tend_isr()` or `riic_rdrf_isr()` requests a STOP condition (`ICCR2_SP` in `RIIC_ICCR2`), it must enable `ICIER_SPIE` in `RIIC_ICIER` and defer `complete(&riic->msg_done)` to `riic_stop_isr()`, which must clear and read back (`riic_readb()`) `RIIC_ICSR2` and `RIIC_ICIER` before completing. [F62]

## Object Lifetimes

- In `riic_i2c_probe()`, `riic->clk`, `riic->info`, `adap->dev.parent`, `riic->i2c_t` (via `i2c_parse_fw_timings()`), `pm_runtime_use_autosuspend(dev)`, and `pm_runtime_enable(dev)` must all be initialized before calling `riic_init_hw(riic)` and `i2c_add_adapter(adap)`; any subsequent failure in `riic_i2c_probe()` as well as teardown in `riic_i2c_remove()` must call `pm_runtime_disable(dev)` and `pm_runtime_dont_use_autosuspend(dev)`. [F1, F2]
- In `riic_i2c_remove()`, `i2c_del_adapter(&riic->adapter)` must be called before `pm_runtime_disable(dev)` so that any in-flight or client-teardown transfers on the adapter can complete while runtime PM remains enabled. [F16]
- `riic_i2c_probe()` acquires `riic->rstc` via `devm_reset_control_get_optional_exclusive_deasserted(dev, NULL)`, which transfers reset deassertion and release ownership to devres, so probe error paths and `riic_i2c_remove()` must not manually assert or put `riic->rstc`. [F7]

## API Contracts

- `riic_i2c_probe()` must store the `struct riic_dev *` pointer in all three `void *` context slots (`devm_request_irq()` `dev_id`, `i2c_set_adapdata(&riic->adapter, riic)`, and `platform_set_drvdata(pdev, riic)`), because ISRs, I2C adapter/recovery callbacks, and PM/remove callbacks (`riic_i2c_remove`, `riic_i2c_suspend`, `riic_i2c_resume`) each cast their `void *` data directly to `struct riic_dev *`. [F54]
- In `riic_init_hw()`, `brl` and `brh` must be computed as signed `int` values, clamped to at least `1` after subtracting sync/noise and rise/fall clock ticks, and ORed with `ICBR_RESERVED` (`GENMASK(7, 5)`) when written to `RIIC_ICBRH` and `RIIC_ICBRL`; writes to `RIIC_ICMR1` must use `ICMR1_CKS(cks)`, which sets the write-protect bit `ICMR1_BCWP` (`BIT(3)`). [F23, F40]
- `riic_clear_set_bit(riic, clear, set, reg)` takes the shifted bitmask to clear as its second argument and the shifted bitmask to set as its third argument (`(riic_readb(riic, reg) & ~clear) | set`). Broken by: swapping `clear` and `set` or passing bit numbers instead of `BIT(...)` masks. [F20]

## Error Handling

- In `riic_xfer()`, `riic_init_hw()`, `riic_i2c_remove()`, and `riic_i2c_suspend()`, `pm_runtime_resume_and_get(dev)` does not hold a runtime PM reference when it returns a non-zero error, so `pm_runtime_put_autosuspend(dev)`, `pm_runtime_put(dev)`, and `pm_runtime_put_sync(dev)` must only be called when `pm_runtime_resume_and_get(dev)` returns 0. Broken by: jumping to `out` in `riic_xfer()` when `pm_runtime_resume_and_get()` fails, or calling `pm_runtime_put*` unconditionally. [F3]
- In `riic_i2c_probe()`, each IRQ in `riic_irqs` (`struct riic_irq_desc`) must be fetched via `platform_get_irq(pdev, irq_desc->res_num)` into a signed `int` and checked with `if (irq < 0)` before passing it to `devm_request_irq()`. Broken by: passing the loop counter `i` instead of `irq_desc->res_num`, or storing the return value in an unsigned variable. [F21]

## Known Pitfalls

- In `riic_tdre_isr()`, updates to `RIIC_ICIER` (switching from `ICIER_TIE` to `ICIER_RIE` on read or to `ICIER_TEIE` when `riic->bytes_left == 0`) must occur before writing `RIIC_ICDRT`, because writing `RIIC_ICDRT` acknowledges TIE and can immediately re-trigger TIE if the byte moves to the shift register. [F25]
- Modifying `ICCR1_SCLO` or `ICCR1_SDAO` in `RIIC_ICCR1` (as in `riic_set_scl()` and `riic_set_sda()`) requires clearing `ICCR1_SOWP` in the same write and re-setting `ICCR1_SOWP` immediately afterward, whereas non-pin-output writes to `RIIC_ICCR1` must keep `ICCR1_SOWP` set. [F28]
- In `riic_init_hw()`, `RIIC_ICCR1` must first be written with `ICCR1_IICRST | ICCR1_SOWP`, then `ICCR1_ICE` set while `ICCR1_IICRST` remains asserted, and `ICCR1_IICRST` cleared only after programming `RIIC_ICMR1`, `RIIC_ICBRH`, `RIIC_ICBRL`, `RIIC_ICSER`, `RIIC_ICMR3`, and `RIIC_ICFER`. [F29]
- `riic_init_hw()` and `riic_xfer()` must pass `riic->adapter.dev.parent` (`adap->dev.parent`, the platform device) rather than `&riic->adapter.dev` to `pm_runtime_*` and `dev_err_probe()`, because runtime PM is enabled on the platform device and `riic_init_hw()` runs in `riic_i2c_probe()` before `i2c_add_adapter()` initializes `adap->dev`. [F80]
