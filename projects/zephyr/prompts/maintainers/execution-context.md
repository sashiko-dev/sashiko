+++
title = "Execution Context: ISRs, Pre-Kernel Code, and Callbacks"
paths = [
    "arch/",
    "drivers/",
    "kernel/",
    "soc/",
]
symbols = [
    "IRQ_CONNECT",
    "IRQ_DIRECT_CONNECT",
    "ISR_DIRECT_DECLARE",
    "K_TIMER_DEFINE",
    "SYS_INIT",
    "gpio_add_callback*",
    "gpio_init_callback",
    "irq_connect_dynamic",
    "irq_lock",
    "k_is_in_isr",
    "k_is_pre_kernel",
    "k_timer_*",
    "k_timer_init",
    "uart_callback_set",
    "uart_irq_callback*",
]
+++

# Execution Context

Applies to any code that runs in an ISR, a timer expiry function, a driver
callback, or an init function. Establish the context before judging an API call:
walk up from the modified function to `IRQ_CONNECT()`, `k_timer_init()`, or the
callback registration. Many driver callbacks run in the driver's ISR, so read the
driver before assuming a callback runs in a thread.

## Calling kernel APIs from an ISR

Source: doc/develop/api/terminology.rst (sleep, no-wait, isr-ok);
doc/kernel/services/interrupts.rst; include/zephyr/kernel.h.

- An ISR must not sleep. Only functions marked isr-ok may be called; the
  `@isr_ok` tag in doxygen records this. Check the API's doxygen rather than
  relying on its name.
- Where `include/zephyr/kernel.h` says the timeout "must be set to K_NO_WAIT if
  called from ISR" (for example `k_sem_take()`, `k_msgq_get()`, and
  `k_heap_alloc()`), any other timeout in an ISR is a defect, even though the API
  carries `@isr_ok`.
- That rule may be enforced only by an assertion, and `__ASSERT()` does nothing
  without `CONFIG_ASSERT`. Absence of a runtime error is not evidence that the call
  is safe.
- Mutexes must not be used in an ISR
  (doc/kernel/services/synchronization/mutexes.rst).
- When a routine can run in both contexts, it uses `k_is_in_isr()` to pick the
  behavior (doc/kernel/services/interrupts.rst). Check that both branches are
  correct.

## Timers and other callbacks

Source: doc/kernel/services/timing/timers.rst.

- A `k_timer` expiry function runs in interrupt context. Apply every ISR rule to
  it and keep it short. Blocking work belongs in a work item or a thread.
- ISRs are not permitted to synchronize with timers, since ISRs cannot block.

## Keeping ISRs short

Source: doc/kernel/services/interrupts.rst (Offloading ISR Work).

- An ISR should run quickly. Time-consuming processing is offloaded to a thread
  or a work item. Report bus transactions or long polling loops added to an ISR
  when the path is real and the delay is unbounded or long for that bus.

## Interrupt locks

Source: doc/kernel/services/interrupts.rst.

- A thread must not sleep while holding an `irq_lock()`; only isr-ok functions
  may be called, and each lock must be matched by an unlock. Check every early
  return between them.
- A zero-latency interrupt must treat all kernel APIs as undefined behavior, must
  be declared as a direct ISR, and must not modify data inspected by kernel APIs
  from normal contexts.
- When power management keeps interrupts locked across resume, a zero-latency
  interrupt can still run before resume completes. Its ISR must be safe at that
  point, or its source must be masked while the system cannot handle it.

## Pre-kernel initialization

Source: doc/kernel/drivers/index.rst (initialization levels);
doc/develop/api/terminology.rst (pre-kernel-ok).

- Code at `PRE_KERNEL_1` and `PRE_KERNEL_2` cannot use kernel services, because
  they are not available yet, and runs on the interrupt stack. Only functions
  documented as pre-kernel-ok are safe there. Use `POST_KERNEL` for init code that
  needs kernel services.
- Init priorities must be decimal integer literals or symbolic names for one.
  Expressions such as `CONFIG_KERNEL_INIT_PRIORITY_DEFAULT + 5` are not permitted.
- A device that depends on another device must initialize after it. Check the
  relative level and priority when the change moves an init or adds a dependency.
