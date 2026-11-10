# Severity Levels

Assign a severity level (`Critical`, `High`, `Medium`, or `Low`) to every
finding. Calibrate seriously against how Linux kernel maintainers triage
production defects:
- `Critical` is reserved for catastrophic production failures and genuine
  kernel security vulnerabilities.
- `High` means a high-confidence kernel defect that breaks real workloads,
  hardware support, or normal administration and clearly warrants immediate
  maintainer attention.
- `Medium` is a real, non-negligible bug with contained impact or uncommon/cold
  trigger conditions.
- `Low` covers minor non-production issues (including minor `kselftests` and
  tooling bugs), improbable edge cases, debug-only paths, and code/commit
  hygiene.

## Calibrating the level (reason before you label)

Evaluate all three dimensions below and state them at the start of
`severity_explanation` so the label is auditable:

- **Consequence and blast radius**: What concretely happens when the bug
  triggers (silent persistent data/filesystem corruption, arbitrary kernel
  memory corruption, whole-system panic/deadlock, broken subsystem or driver
  feature, bounded leak on an error path, non-fatal `WARN_ON_ONCE`, wrong errno
  or counter, or a failure inside a userspace test/tool)?
- **Actor, privilege, and kernel threat-model boundary**: Who can trigger the
  bug, what capabilities or configuration are required, and does it cross a
  trust boundary guaranteed by the Linux kernel threat model
  (`Documentation/process/threat-model.rst`, supplied in `<kernel_threat_model>`
  when available)?
  - **Security boundary crossed**: An unprivileged local user, an unprivileged
    user namespace escaping initial-namespace restrictions, a remote network
    peer, an untrusted VM guest crossing guest-to-host or guest-to-hypervisor
    isolation, or hostile hardware on an IOMMU/driver path explicitly hardened
    against untrusted devices.
  - **Outside the vulnerability boundary (regular bugs, NOT security
    vulnerabilities)**: Per the kernel threat model, issues requiring `root` or
    elevated administrative capabilities (`CAP_SYS_ADMIN`, `CAP_NET_ADMIN`,
    `CAP_SYS_RAWIO`, `CAP_SYS_MODULE`) with no further boundary crossed;
    exposure only via `debugfs`, `CONFIG_STAGING`, `EXPERIMENTAL`, or
    developer/debug Kconfigs (`LOCKDEP`, `KASAN`, `FAULT_INJECTION`,
    `CONFIG_NOMMU`); mounting a corrupted or crafted filesystem image
    (privileged admin action); physically modified or emulated non-conforming
    devices; lab-only probabilistic attacks; hardening weaknesses without an
    exploit path; and non-steerable structure padding or kernel pointer leaks.
    Never escalate these to `Critical` on security grounds, and never drop them
    — judge them as regular bugs by consequence and likelihood.
- **Likelihood, trigger preconditions, and confidence**: Lay out the concrete
  path and preconditions needed to hit the bug.
  - When reviewing a diff where callers are outside the hunk and you cannot
    establish caller privileges, do not invent unproven restrictions or bury a
    real bug.
  - When concrete preconditions **are** established in the code (for example,
    `capable(CAP_SYS_ADMIN)`, `debugfs` file operations, one-shot `probe()` or
    module-init unwind, `CONFIG_STAGING`, or a narrow multi-CPU race window
    without attacker timing control), weigh those preconditions directly into
    the severity level. Compounding restrictions (such as `root` + `debugfs` +
    a subtle race window) lower real-world risk and belong at `Medium` (or
    `Low` for debug/test-only trivia), never `Critical` or `High`.
  - **Speculative findings**: If a finding rests on an unproven assumption that
    could not be verified from the code, mark it speculative and cap its
    severity at `Medium` (or `Low` when the consequence itself is minor). A
    speculative finding is still reported, never silently dropped.

## Critical

- **Definition**: Catastrophic production failures: genuine Linux kernel
  security vulnerabilities crossing a guaranteed trust boundary, silent or
  persistent corruption of user data or filesystems, deterministic or hot-path
  whole-system crashes/deadlocks under normal workloads, or userspace ABI/UAPI
  breakage.
- **Question to ask**: Does this cross a kernel threat-model security boundary
  with realistic exploitability, silently corrupt persistent data on normal
  paths, crash or hang normal production workloads, or break existing userspace
  ABI? If no, it is **not** `Critical`.
- **Examples**:
    - Memory corruption (use-after-free, out-of-bounds write, double free),
      privilege escalation, cross-user/cross-guest data disclosure, or
      system-wide panic/hang reachable by an unprivileged local user, remote
      network peer, or untrusted VM guest.
    - Silent filesystem, block-layer, or page-cache corruption destroying user
      data during normal I/O.
    - Kernel panic, oops, or whole-system deadlock on boot, hot paths, or
      standard syscalls/workloads without requiring privileged debug interfaces.
    - Breaking an established UAPI/ABI contract visible to existing userspace
      applications without deprecation.
- **Never `Critical`**:
    - Bugs requiring `root` or `CAP_SYS_ADMIN` (or similar initial-namespace
      capabilities), `debugfs`/staging/experimental/debug Kconfigs, crafted
      mounted filesystem images, or fake/modified hardware, unless they
      independently cause silent persistent data corruption during normal valid
      use.
    - Panics or UAFs gated behind both privileged interfaces (`debugfs`, admin
      sysfs/ioctl) and a narrow race window.
    - Any bug in `tools/testing/selftests/` (`kselftests`), `tools/`,
      `samples/`, `scripts/`, or `Documentation/`.

## High

- **Definition**: Serious, high-confidence kernel defects that crash, wedge,
  corrupt kernel state, leak resources continuously, or break primary
  functionality on realistic runtime or standard administrative paths.
- **Question to ask**: With high confidence and without contrived preconditions,
  will this crash or deadlock the kernel during normal runtime or standard
  administration, break a primary supported feature, or leak resources on a
  recurring path?
- **Examples**:
    - Kernel panic, oops, use-after-free, out-of-bounds access, or deadlock
      triggered reliably during normal runtime or routine privileged operations
      (device probe/unbind, hotplug, suspend/resume, network interface up/down,
      clean mount/umount on valid media, cgroup/namespace teardown).
    - Logic error that systematically breaks a driver, filesystem, or subsystem
      on a primary supported code path.
    - Unbounded or recurring resource leak (memory, pages, refcounts, file
      descriptors, locks) hit on normal recurring execution paths.
    - Significant performance or power regression on a hot path.
    - Confirmed lock-ordering inversion or sleep-in-atomic bug on a reachable
      runtime path.
    - Missing or clearly broken `Signed-off-by` trailer in the commit message
      (such as a missing DCO sign-off, missing or placeholder email address, bot
      identity, or handle/username instead of a real human name).
- **Never `High`**:
    - Speculative findings resting on unverified assumptions (cap at `Medium`).
    - Bugs requiring both privileged/debug access (`root`, `CAP_SYS_ADMIN`,
      `debugfs`) and a narrow uncontrolled race window or contrived sequence
      (use `Medium`).
    - One-shot or cold error-unwind leaks such as driver `probe()` allocation
      failures (use `Medium`).
    - Bugs in `kselftests`, `tools/`, `samples/`, `scripts/`, or
      `Documentation/`.

## Medium

- **Definition**: Real, contained defects with concrete failure modes under
  cold, error-unwind, or privileged/racy conditions, as well as test bugs that
  compromise test validity.
- **Examples**:
    - Memory, refcount, mapping, IRQ, or clock leaks on cold paths or error
      unwind branches (such as `probe()` failure or uncommon ioctl error
      returns).
    - Kernel oops, UAF, or deadlock that requires **both** administrative
      privilege (`root`, `CAP_SYS_ADMIN`, `debugfs`, crafted mounted image)
      **and** a narrow race window or contrived sequence (for example, racing
      `debugfs` reads against module unload or device reset).
    - Localized functional bugs on secondary paths, wrong errno values, partial
      state updates, or inaccurate statistics/accounting.
    - Non-fatal `WARN_ON` or lockdep splats that do not corrupt memory or wedge
      the system.
    - Small non-steerable uninitialized structure padding or stack bytes copied
      to unprivileged userspace (`copy_to_user` padding leak).
    - `kselftests`, `perf`, or in-tree userspace tool bugs that cause silent
      false passes or break building/running the suite on supported targets.
    - Meaningful mismatch between code behavior and commit message, or
      speculative kernel bugs whose consequence would otherwise be `High` or
      `Critical`.

## Low

- **Definition**: Minor non-production defects, improbable or debug-only edge
  cases, hardening weaknesses without an exploit path, and code/commit hygiene
  issues with negligible production impact.
- **Question to ask**: Is the impact confined to minor test/tool behavior,
  debug/developer-only configurations, unrealistic inputs, or code/commit
  hygiene? If yes, use `Low`.
- **Examples**:
    - Minor bugs in `tools/testing/selftests/` (`kselftests`), `samples/`,
      `scripts/`, or `tools/` (for example, missing `free()`/`close()` before
      test exit, unchecked allocation in a test helper, minor output typos, or
      edge-case CLI argument handling).
    - Minor issues reachable only under developer/debug Kconfigs (`LOCKDEP`,
      `KASAN`, `FAULT_INJECTION`, `CONFIG_STAGING`) or out-of-production lab
      conditions (unrealistic CPU/RAM/thread counts or silent-lab timing).
    - Hardening weaknesses with no demonstrable exploit path or kernel pointer
      leaks limited to privileged logs/interfaces.
    - Obscure build issues (standard builds are checked deterministically),
      negligible performance nitpicks, dead code, redundant checks, confusing
      naming, or comment/documentation typos.
    - Missing, malformed, or inaccurate `Fixes:` trailer in the commit message.
