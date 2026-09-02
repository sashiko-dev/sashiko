# QEMU Severity Levels

When identifying issues in QEMU patches, you must assign a severity level to each finding.
Don't unnecessarily raise priority; Critical must be critical, High must be very damaging.
Use Medium as default and calibrate using consequence, reachability, and triggering conditions.

## Calibrating the level (reason before you label)

State this reasoning at the start of the `severity_explanation` so the label is auditable.

- Consequence: What actually happens if the bug triggers (guest escape, host memory corruption,
  host denial of service/crash, silent guest data corruption, migration failure, resource leak).
- Triggering path: Name the preconditions a guest, monitor command, or migration stream must satisfy.
  If resting on unproven assumptions, report the finding and mark it speculative (capped at Medium).
- Reachability: If reachable from an unprivileged guest via MMIO, PIO, DMA, virtqueues, or
  network packets, raise the level. Never lower because you believe the guest OS will be well-behaved.

## Critical
- **Definition**: Hypervisor breakout, arbitrary host memory corruption, or remote host execution.
- **Question to ask**: Can an unprivileged or malicious guest escape confinement, corrupt host memory, or gain control of the host QEMU process? If yes, it's Critical.
- **Examples**:
    - Guest-to-host breakout (buffer overflow in MMIO/DMA emulation with controlled host writes).
    - Arbitrary host memory read/write via unchecked guest address or descriptor chain.
    - Host code execution from guest or network input.
    - Unchecked deserialization in migration stream leading to host code execution.
    - Host-level data loss or disk corruption.

## High
- **Definition**: Host crash/DoS triggerable by guest, device lockup, or guest kernel panic.
- **Question to ask**: Can a guest reliably crash the host QEMU process (abort/segfault), trigger an infinite loop/deadlock (BQL starvation), or corrupt guest state across migration? If yes, it's High.
- **Examples**:
    - Host crash via `abort()` or `g_assert()` reachable from untrusted guest MMIO/PIO writes.
    - NULL pointer dereference in hot path device emulation.
    - Out-of-bounds read leaking host memory to the guest.
    - Deadlock or BQL deadlock between main thread, vCPU threads, and I/O threads.
    - Coroutine reentrancy bug causing use-after-free.
    - VMState post-load validation omission causing guest corruption or host crash on migration.
    - Uncontrolled host memory leak on packet/request processing hot path.

## Medium
- **Definition**: Recoverable issues, cold-path memory leaks, minor emulation inaccuracies.
- **Examples**:
    - Memory leak on device teardown, reset, or cold error paths (`realize`/`unrealize`).
    - Device emulation deviation from hardware specification that may affect picky OS drivers.
    - Missing or improper register write mask (`wmask`) without immediate exploitability.
    - Missing `ERRP_GUARD()` or improper `error_setg()` propagation.
    - QMP/HMP interface error or minor schema inconsistency.
    - Speculative findings where triggering path is unproven.

## Low
- **Definition**: Naming, style, dead code, or documentation defects.
- **Examples**:
    - QOM naming inconsistencies.
    - Formatting and style issues not conforming to QEMU CODING_STYLE.rst.
    - Confusing comments or outdated docstrings.
    - Unnecessary type casts or redundant macros.
