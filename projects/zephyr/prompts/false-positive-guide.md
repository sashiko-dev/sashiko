# False Positive Prevention Guide

This guide is used where avoiding false positives matters most. Shift bias away
from fast processing and follow it carefully.

## Core principle

**If you cannot prove an issue exists with concrete evidence from this
codebase, do not report it.**

Evidence means code you have read. Not "RTOS code usually", not "this looks like
it could", not "a caller might". Name the function, name the context it runs in,
name the concrete execution path that reaches it.

The corollary matters as much: proving a path is *structurally possible* is
enough. You do not have to prove it happens on every boot. A length taken from a
received packet and used for a copy without a bounds check is a bug even if no
known peer sends that length yet.

## Patterns that produce false positives here

### 1. Speculation without a scenario

If you cannot construct a specific input or sequence that triggers the issue,
demote it to `Low` or drop it entirely. "This value is not validated" is not a
finding until you name who controls the value and what it then corrupts.

### 2. Restating obviously correct code

Do not report that code does what it says. A finding states a defect.

### 3. Issues CI already catches

`scripts/ci/check_compliance.py` runs on every pull request: checkpatch,
clang-format, Kconfig and Devicetree checks, binding checks, gitlint, and more.
Twister builds and runs the affected tests. Do not report formatting, suspected
compile errors, undefined Kconfig symbols, or binding schema problems unless you
have proved that the check would miss them.

### 4. Code that cannot be built in the configuration you assumed

Most of the tree is conditional. Before reporting that a symbol can be missing,
that a feature can be disabled, or that a guard is absent, read the Kconfig
`depends on` of the file's option and the CMake condition that compiles it. A
driver file built only when `DT_HAS_<COMPAT>_ENABLED` holds does not need to guard
against its own devicetree node being absent.

### 5. Devicetree values resolved at compile time

`DT_*` macros expand at compile time and fail the build when a required property
is missing. Do not ask for a runtime NULL check on a value produced by
`DT_INST_PROP()` for a property the binding marks `required: true`. This does not
extend to device readiness: `DEVICE_DT_GET()` yields a pointer at build time, but
the device may still have failed to initialize, so `device_is_ready()` remains a
valid request.

### 6. ISR-safe APIs reported as blocking

Many kernel APIs are documented as isr-ok, such as `k_sem_give()`,
`k_work_submit()`, and `k_timer_stop()`. Several APIs that take a timeout, such
as `k_sem_take()`, are safe in an ISR when the timeout is `K_NO_WAIT`. Check the
API's doxygen in `include/zephyr/kernel.h` before reporting a context violation.

### 7. Races that the configuration or the scheduling model rules out

Before reporting a race, confirm that both sides can actually interleave. Code
may run only in one thread, or in a single workqueue that serializes its handlers.
On a uniprocessor build, cooperative priority or `k_sched_lock()` prevents
ordinary thread preemption, not all interleaving. Check for blocking or yielding
within the sequence, and for possible ISR or meta-IRQ access to the same state.
See doc/kernel/services/scheduling/index.rst and the Meta-IRQ Priorities section
of doc/kernel/services/threads/index.rst. A uniprocessor-only file does not need
SMP protection when Kconfig rules out `CONFIG_SMP`.

### 8. Hardware behavior and specifications you cannot verify

The datasheet and architecture reference manuals are usually not in the tree. Do
not invent or cite specific register bitfield positions, reserved-bit ranges, or
architecture manual section numbers from memory without verifiable evidence from
the tree (such as architecture or driver headers, a binding, or a sibling
driver). For other hardware register sequences that look unusual without a
datasheet in the tree, remember a vendor sequence can still be required: state
what you based the concern on and mark it speculative (capped per
`severity.md`), or drop it when there is no concrete in-tree basis.

### 9. External modules

Vendor HALs and other modules live outside this repository and are pinned in
`west.yml`. Do not report a defect in code you cannot see, and do not claim a HAL
function misbehaves without having read it.

### 10. Tests and samples held to production standards

Code in `tests/` and `samples/` may abbreviate production boilerplate. Report an
omission when concrete evidence shows a functional or safety failure, a false
test pass, flakiness, or incorrect API use. Short examples alone are not defects.

### 11. A policy the commit states

A commit can change a default or a behavior on purpose. When the message states
the new policy, the behavior that follows from it is not a regression, even if it
differs from the parent. Report it only if the code does not do what the message
says, or if you can show the policy breaks something the message does not account
for. Asking the author to restore the old behavior is not a finding. This covers
defaults and configuration policy. A stated policy never excuses a memory safety,
locking, or system call verification defect.

### 12. Style preferences

Follow `doc/contribute/style/code.rst`, which says to match nearby code where the
guidelines leave a choice. Do not report a preference the guidelines do not state.

### 13. Auto-discovered board config and overlay files

Zephyr's build system automatically discovers application-local board and SoC
configuration and overlay files under `boards/` and `socs/` (for example
`boards/<board>.conf`, `boards/<board>.overlay`, and
`socs/<SOC>_<BOARD_QUALIFIERS>.overlay`, distinct from the top-level `boards/`
and `soc/` trees) unless `CONF_FILE` or `DTC_OVERLAY_FILE` overrides discovery
(verify the discovery rules in `doc/build/dts/howtos.rst` and
`doc/build/kconfig/setting.rst` in the target tree). Do not report a
`boards/<board>.conf` or `boards/<board>.overlay` file in `tests/` or `samples/`
as unused or orphaned merely because `testcase.yaml` or `sample.yaml` does not
reference it via `EXTRA_CONF_FILE` or `EXTRA_DTC_OVERLAY_FILE` or list the board
in `platform_allow`.

### 14. Invalid or unbuildable remediation suggestions

Verify that every suggested fix obeys Zephyr's build and runtime rules before
reporting it:

- Do not suggest sleeping, mutexes, or workqueue waits inside `PRE_KERNEL_1` or
  `PRE_KERNEL_2` init paths.
- Do not suggest overriding `enum:` from an `include:`d Devicetree binding YAML;
  `edtlib` forbids overriding keys from included bindings with conflicting
  values (`doc/build/dts/bindings-syntax.rst`).
- Do not demand test Kconfig values that exceed target architecture or linker
  limits without checking architecture Kconfig ranges in the tree.
- Do not ask a driver to accept invalid inputs to work around a buggy caller
  when the subsystem API contract requires rejecting them.

### 15. Existing API signatures and non-functional coding-guideline noise

Consistent with Rule 11.8 in `maintainers/general.md`, do not report a
`(void *)` cast that drops `const` when it is forced by an unchanged existing
Zephyr API parameter type (such as `k_mem_phys_addr(void *)`), unless the
callee actually mutates read-only memory (which remains a `High` defect). Do not
report stylistic MISRA preferences with no functional or safety defect (such as
omitting an empty trailing `else` branch or testing `if (ret)` on an `int`
status code).
