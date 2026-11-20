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

### 8. Hardware behavior you cannot verify

The datasheet is usually not in the tree. Do not report a register sequence as
wrong because it looks unusual. A vendor sequence can look redundant and still be
required. Report it only with evidence from the tree, such as a sibling driver,
a binding, or a vendor HAL header, or mark it speculative.

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
