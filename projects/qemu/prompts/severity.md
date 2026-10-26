# Severity Levels

Assign a severity to every finding. Take it seriously: `Critical` must be
reserved for catastrophic failures, and `High` must mean severe functional or
guest-visible breakage. Use `Medium` as the default for real functional defects
and `Low` for defensive hardening, rare edge cases, and hygiene.

QEMU emulates hardware for untrusted guests and runs untrusted guest input on
the host. Calibrate against *those* consequences, not against theoretical
perfection.

## Calibrating the level (reason before you label)

State this reasoning at the start of `severity_explanation` so the label is
auditable.

- **Blast radius and irreversibility**: What actually happens when the bug
  triggers? A guest-triggerable out-of-bounds access or a migration stream that
  silently corrupts state is catastrophic (`Critical`); a wrong bit in an
  optional feature register is contained (`Medium`).
- **Likelihood and preconditions**: Name the concrete path and preconditions
  required to trigger the bug. Only escalate for guest-controlled input when an
  attacker or malformed payload can actively cross the guest/host boundary or
  cause a host crash. If a bug requires a narrow race window or an unlikely
  coincidence of register values, reflect that lower probability in the
  severity (`Medium` or `Low`).
- **Speculative findings**: If a finding rests on an unverified assumption, mark
  it speculative and cap its severity at `Medium` (or `Low` if the impact is
  minor).

## Critical

- **Definition**: Severe security vulnerability crossing the guest-to-host
  boundary, guest-triggerable host memory corruption or information leak,
  guest-triggerable host crash, or migration-stream corruption that silently
  destroys guest-visible state.
- **Question to ask**: Can a guest (or a malicious device model fed by a guest)
  actually reach this, and does crossing it corrupt, leak, or crash the host or
  destroy migrated state? If no, it is **not** `Critical`.
- **Examples**:
    - Out-of-bounds read or write driven by a guest-controlled index, size, or
      address, including integer overflow in address arithmetic feeding a
      copy or a ring index.
    - Use-after-free or double-free of device state reachable from a guest
      action (submitting a descriptor, unplugging a device).
    - `dma_memory_read`/`dma_memory_write` (or a descriptor walk) acting on a
      guest-controlled address or length without validation.
    - A host abort/assertion the guest can trigger by writing a register.

## High

- **Definition**: Major functional breakage: the emulated device or subsystem
  systematically fails on primary, realistic guest usage, or a contract
  (register semantics, config space, qapi/qmp) is broken for guests.
- **Question to ask**: Under realistic guest usage, does this break what the
  guest observes, deadlock threads, or wedge a common device path?
- **Examples**:
    - Wrong emulated behavior on a common access pattern (wrong register
      semantics, wrong endianness, wrong interrupt delivery).
    - A lock-ordering violation with a concrete interleaving that deadlocks
      the BQL against a per-device lock.
    - Error returns silently ignored on a realistic path, leaving the device
      in a state the guest can observe as wrong.
    - Missing vmstate field versioning or a subsection guard that breaks
      incoming migration for a supported machine type.

## Medium

- **Definition**: Contained functional bugs, localized races, incorrect state
  transitions for individual accesses, leaks on rare error paths, or issues in
  secondary device paths.
- **Examples**:
    - A resource leak on an error path that is hard for the guest to reach.
    - A boundary condition mishandled only for rare but valid values.
    - `Error **errp` set after a failure return on a path whose failure a
      caller cannot currently observe.
    - Commit hygiene: missing `Signed-off-by`, or a body that restates the
      diff without explaining why.

## Low

- **Definition**: Defensive coding improvements, improbable edge cases, minor
  cleanup omissions, or stylistic/documentation issues with negligible
  real-world impact.
- **Question to ask**: Does triggering this require extreme/unrealistic guest
  input, or is the consequence limited to a cosmetic or bookkeeping
  discrepancy? If yes, use `Low`.
- **Examples**:
    - Pure style, naming, or comment issues, including anything QEMU's
      `scripts/checkpatch.pl` would flag mechanically.
    - Defensive hardening where real-world inputs will not trigger the
      failure.
    - Confusing naming or stale comments in the touched area.
