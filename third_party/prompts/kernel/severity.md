# Severity Levels

When identifying issues, you must assign a severity level to each finding.
Treat this task seriously, it's very important. Don't unnecessarily raise the priority,
critical issues must be critical, high issues must be very damaging.
Use Medium as default and lower/raise depending on the "Question to ask" answer and examples.
Use the following definitions and examples:

## Calibrating the level (reason before you label)

State this reasoning at the start of the severity_explanation so the label is
auditable.

- Consequence: what actually happens if the bug triggers (memory or data
  corruption, crash, info leak, resource leak, incorrect result, performance,
  or other). This is the starting point for the level.
- Triggering path: lay out the concrete path that reaches the bug, naming the
  preconditions a caller or input must satisfy. If you cannot, because it rests
  on an unproven assumption or on an ABI, register, or convention you might be
  misreading, still report the finding and mark it speculative.
- Reachability & Linux Kernel Threat Model (`Documentation/process/threat-model.rst`): evaluate whether
  the triggering path crosses an actual trust boundary defined by the Linux
  Kernel threat model:
  - **Exploitable across a threat-model boundary (Raise Severity to `Critical` or `High`)**:
    If the bug is reachable by an unprivileged local user, an unprivileged user
    namespace/container (`CONFIG_USER_NS`) escaping into the initial namespace,
    an untrusted remote network peer, unprivileged syscall/ioctl/netlink input,
    or an untrusted external USB/PCIe peripheral where IOMMU or driver hardening
    applies—and triggers memory corruption, privilege escalation, unauthorized
    cross-user data/IPC access, or kernel panic/DoS—**raise** the severity to
    `Critical` or `High`.
  - **Not exploitable under the threat model (Dismiss or Lower Severity to `Medium` / `Low`)**:
    If a candidate issue is excluded from being a vulnerability by the Linux
    Kernel threat model—for example, it requires trusted hardware to violate its
    specification (when the driver is not hardened against hostile hardware),
    requires `root` or initial capabilities (`CAP_SYS_ADMIN`, `CAP_NET_ADMIN`,
    `CAP_SYS_RAWIO`, `CAP_SYS_MODULE`) to abuse privileged interfaces (`sysfs`,
    `debugfs`, `procfs`, module parameters, or mounting corrupted block
    filesystem images), or is a hardening weakness / kernel pointer or small
    structure padding leak with no cross-boundary exploit path—do **not** classify
    it as a `Critical` security vulnerability. **Dismiss** it if it is framed
    solely as a hypothetical security attack where no bug exists under
    conforming operation, or **lower** its severity (`Medium` or `Low`) if a real
    functional or hardening defect exists without untrusted exploitability.
  - Do not lower a genuine functional bug merely because you are uncertain
    whether an unprivileged caller can reach it from the diff alone: if
    reachability is unknown, leave the level calibrated on functional
    consequence.

A speculative finding is capped at Medium because the open question is whether
the bug is real at all. Issues that are provably outside the Linux Kernel threat
model must be dismissed (when no functional bug exists) or lowered in severity
(when only a non-security defect or hardening weakness exists).

## Critical
- **Definition**: Issues that cause data loss, memory corruptions, or security vulnerabilities exploitable across a Linux Kernel threat model trust boundary.
- **Question to ask**: Is it actually better for system to crash rather then keep working, or can an unprivileged user / untrusted remote peer cross a kernel security boundary? If yes, it's a critical issue.
- **Examples**:
    - Security vulnerability exploitable across a Linux Kernel threat model boundary (e.g., unprivileged local privilege escalation, cross-user isolation breach, or remote exploit).
    - Data corruption.
    - Memory corruption (e.g., buffer overflow, use-after-free) reachable in normal operation or via untrusted input.
    - Kernel panic or oops on hot path or which can be triggered by an unprivileged userspace program or remotely.
    - ABI breakage without proper deprecation.

## High
- **Definition**: Serious issues that can bring the system down or make it fully unusable.
- **Question to ask**: Can the system go down or become totally unusable with a non-trivial probability? If yes, it's a high issue.
- **Examples**:
    - Kernel panic or oops.
    - Logic errors leading to incorrect functional behavior.
    - Resource leaks (memory, locks).
    - Significant performance regression.
    - Violation of core kernel locking rules.

## Medium
- **Definition**: Recoverable issues or non-critical performance regressions.
- **Examples**:
    - Memory or resource leaks on cold paths.
    - Inefficient locking.
    - Incorrect statistics.
    - Meaningful code and commit message mismatch.
    - Non-critical performance regressions.
	- Issues in kselftests, perf and other userspace applications.

## Low
- **Definition**: Naming, style and coding style issues.
- **Question to ask**: Is there any visible real life effect? If no, it's a low issue. Otherwise it's a medium issue.
- **Examples**:
    - Build issues (because there are better ways to find them).
    - Typos in comments.
    - Formatting issues.
    - Confusing variable naming or comments.
    - Negligible performance regressions.
    - Unnecessary code complexity.
    - Missing documentation or comments.
