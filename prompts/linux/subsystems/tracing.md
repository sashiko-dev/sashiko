# Tracing Subsystem Details

## Trace Event Definition and String Handling

Incorrect use of `TRACE_EVENT` macros causes data corruption in the ring
buffer, truncated strings, or kernel panics from buffer overflows. Misusing
`TP_fast_assign()` with side effects leads to behavioral differences
depending on whether tracing is enabled, creating hard-to-diagnose bugs.

`TRACE_EVENT` declarations use these macros in `TP_STRUCT__entry()`:

| Macro | Purpose |
|-------|---------|
| `__field(type, name)` | Fixed-size scalar field |
| `__array(type, name, len)` | Fixed-size array embedded in the trace record |
| `__string(name, src)` | Dynamic-length string; source pointer is captured automatically |
| `__vstring(name, fmt, ap)` | Dynamic-length string from `va_list` format arguments |

In `TP_fast_assign()`:
- `__assign_str(name)` copies the string whose source was declared in
  `__string(name, src)`. It takes only the field name; the source is
  captured automatically from the `__string()` declaration via
  `__data_offsets` (see `include/trace/stages/stage5_get_offsets.h` and
  `include/trace/stages/stage6_event_callback.h`).
- `__assign_vstr(name, fmt, va)` formats a string from a `va_list` into
  a field declared with `__vstring()`.
- `TP_fast_assign()` must not contain side effects. It only executes when
  a tracepoint is active, so side effects create behavior that differs
  based on tracing state.

`TRACE_EVENT_CONDITION` and `TP_CONDITION` skip the trace record entirely
when the condition is false, avoiding the cost of `TP_fast_assign()` and
ring buffer allocation. Use `TRACE_EVENT_CONDITION` when trace arguments
require expensive dereferences that should be avoided when the condition
is false. These are defined in `include/linux/tracepoint.h`.

## Tracepoint Probe Registration and RCU

Accessing freed probe data or calling an unregistered probe function
causes use-after-free or NULL pointer dereferences. Tracepoint callbacks
execute in RCU read-side critical sections: non-faultable tracepoints
use `preempt_disable_notrace()` (via `guard(preempt_notrace)`), while faultable
syscall tracepoints use RCU Tasks Trace (via `guard(rcu_tasks_trace)`).

- Probes registered via `tracepoint_probe_register()` or
  `rv_attach_trace_probe()` (defined in `include/rv/instrumentation.h`)
  must remain valid until after `tracepoint_synchronize_unregister()` is
  called. This function issues both `synchronize_rcu_tasks_trace()` and
  `synchronize_rcu()` to ensure all in-flight probe calls complete.
- Tracepoints are gated by `static_branch_unlikely()` on the tracepoint
  key (a static key initialized to `STATIC_KEY_FALSE_INIT`). When no
  probes are registered, the branch is a NOP, giving near-zero overhead.
- Probe functions must not sleep in non-faultable tracepoint context.
  Faultable tracepoints (declared via `DECLARE_TRACE_SYSCALL` /
  `TRACE_EVENT_SYSCALL`) call `might_fault()` and use RCU Tasks Trace
  protection, allowing sleepable operations.

## Tracepoint Kconfig Dependencies

Using tracepoints that are conditionally compiled causes link failures
when the Kconfig option gating the tracepoint object file is disabled.
Architecture dependencies (e.g., `depends on X86 || RISCV`) do not
guarantee that subsystem-specific tracepoints are available because
subsystem features can be independently disabled.

The `page_fault_kernel` and `page_fault_user` tracepoints are declared
in `include/trace/events/exceptions.h`, but their `CREATE_TRACE_POINTS`
instantiation lives in architecture fault handlers (`arch/x86/mm/fault.c`,
`arch/riscv/mm/fault.c`). On RISC-V, `fault.o` is only built when
`CONFIG_MMU` is enabled (`arch/riscv/mm/Makefile`). On x86, `CONFIG_MMU`
is unconditionally `y` (`arch/x86/Kconfig`).

When code attaches to tracepoints via `rv_attach_trace_probe()` or direct
`tracepoint_probe_register()` calls, the Kconfig must include dependencies
matching the tracepoint's build-time availability. Check the Makefile and
Kconfig guards for the file containing `CREATE_TRACE_POINTS` for the
tracepoint header.

```kconfig
// WRONG: X86 || RISCV can have !MMU configurations (RISC-V NOMMU)
config RV_MON_PAGEFAULT
    depends on X86 || RISCV

// CORRECT: explicitly require MMU for page fault tracepoints
config RV_MON_PAGEFAULT
    depends on X86 || RISCV
    depends on MMU
```

See `kernel/trace/rv/monitors/pagefault/Kconfig` for the actual example.

## Quick Checks
- No blocking operations in non-faultable trace context
- `__assign_str()` takes only the field name (single argument); the source is implicit from `__string()`
- Tracepoint names follow `subsystem_event` convention
- Exactly one `.c` file per tracepoint header defines `CREATE_TRACE_POINTS` before including it
- Kconfig dependencies match the build-time availability of any tracepoints consumed
