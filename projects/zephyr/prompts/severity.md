# Severity Levels

Assign a severity to every finding. Take it seriously: `Critical` must be
reserved for catastrophic failures, and `High` must mean severe functional
breakage. Use `Medium` as the default for real functional defects and `Low` for
defensive hardening, rare edge cases, and hygiene.

Zephyr runs on deployed embedded devices that are often hard to update.
Calibrate against *those* consequences, not against theoretical perfection.

## Calibrating the level (reason before you label)

State this reasoning at the start of `severity_explanation` so the label is
auditable.

- **Blast radius and irreversibility**: What actually happens when the bug
  triggers? Corruption of kernel memory from a network packet is catastrophic
  (`Critical`); a wrong sample rate in one sensor driver is contained (`Medium`).
- **Likelihood and preconditions**: Name the concrete path and preconditions
  required to trigger the bug, including the Kconfig options and Devicetree
  configuration it needs. A bug that only exists under an uncommon configuration
  or a narrow race window deserves a lower severity than one on the default path
  of a widely used subsystem.
- **Who controls the trigger**: Only escalate for untrusted input when an external
  peer or a user-mode thread can actually reach the code with values it controls.
- **Speculative findings**: If a finding rests on an unverified assumption, such
  as undocumented hardware behavior, mark it speculative and cap its severity at
  `Medium` (or `Low` if the impact is minor).

## Critical

- **Definition**: Memory corruption or code execution reachable by an external
  peer or by a user-mode thread, escape from the user-mode sandbox, or a defect
  that can permanently brick a device.
- **Question to ask**: Can an attacker or an unprivileged thread actually reach
  this, and does it corrupt kernel memory, leak protected memory, escalate
  privilege, or destroy the device's ability to boot? If no, it is **not**
  `Critical`.
- **Examples**:
  - A length field from a received packet used for a copy without a bounds check.
  - A system call verifier that omits the `K_SYSCALL_MEMORY_WRITE()` check on a
    buffer the implementation writes to.
  - A bug in a flash or update path that erases the only bootable image.

## High

- **Definition**: Crash, deadlock, or lost data on a realistic path, without the
  reach of a `Critical` finding.
- **Question to ask**: Would a user with a supported board and a normal
  configuration hit this during ordinary operation?
- **Examples**:
  - A sleeping call on a path that runs in an ISR.
  - Lock ordering that can deadlock the system workqueue.
  - A use-after-free between a work handler and a driver teardown path.
  - A change that stops a supported board or common configuration from booting.
  - A driver that silently returns corrupted data on its normal read path.

## Medium

- **Definition**: A real functional defect with contained impact. This is the
  default level for a verified bug.
- **Examples**:
  - A resource leak on an error path.
  - An unchecked return value that hides a failed bus transfer.
  - A race with a narrow window and a recoverable result.
  - A missing `device_is_ready()` check on a dependency.
  - Kconfig or Devicetree logic that breaks one specific configuration.
  - A power management reference imbalance that keeps a device powered.

## Low

- **Definition**: Defensive hardening, rare edge cases, and hygiene.
- **Examples**:
  - A missing check that cannot trigger in any configuration in the tree.
  - Documentation or binding descriptions that disagree with the code.
  - A commit message that restates the diff without saying why.
  - Missing test coverage for a change that the conventions expect to have it.
