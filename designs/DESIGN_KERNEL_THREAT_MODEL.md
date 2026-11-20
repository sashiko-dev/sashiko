# Design: Kernel Threat Model and Linux Severity Calibration

## 1. Problem

Linux v7.3 added `Documentation/process/threat-model.rst`, the kernel's
authoritative statement of what boundaries it guarantees (user-based
isolation and capability-based protection) and what classes of issues are
**not** kernel security vulnerabilities (`root` or `CAP_SYS_ADMIN` actions,
`debugfs`/staging/experimental/debug-only Kconfig options, mounting
corrupted or crafted filesystem images, physically modified or emulated
non-conforming hardware, physical access without configured IOMMU
defenses, lab-only probabilistic attacks, hardening weaknesses without an
exploit path, random padding or pointer leaks, and regressions mitigable
by permissions or resource limits).

Today Sashiko suffers from two coupled gaps when evaluating Linux kernel
bugs and patches:

1. **Paraphrased or missing threat-model definitions**:
   - `SeveritySession` (`src/workflows/linux_bug.rs`) and the patch-review
     verification stages (`src/workflows/verification_shards.rs` and
     `src/workflows/linux_patch_review.rs`) see only `kernel/severity.md`,
     where "Security vulnerability" and "untrusted, remote, or
     unprivileged input" are left undefined.
   - `SeveritySession` also receives neither `verification_reasoning`, nor
     prefetched source context, nor repository read tools, leaving it
     blind to caller capability checks (`capable(CAP_SYS_ADMIN)`),
     `debugfs`/sysfs permissions, or `Kconfig` gates unless normalization
     happened to mention them.
   - `CHECK_INSTRUCTION` (`src/workflows/related_preexisting.rs`)
     paraphrases kernel security policy and classifies "a malicious device
     or filesystem image" as `SecuritySensitive`, directly contradicting
     `Documentation/process/threat-model.rst`.
   - `STAGE_SECURITY_INSTRUCTION` (`src/workflows/linux_patch_review.rs`)
     uses a generic red-team prompt without naming the kernel's trust
     boundaries or requiring concerns to record required privileges.

2. **Systematic Linux severity miscalibration (`projects/linux/prompts/severity.md`)**:
   - `severity.md` instructs the model that speculative uncertainty is
     *"the only reason to lower a level. Reachability never does."*
     Combined with `Critical` listing generic memory corruption and
     userspace-triggered panics, any panic or UAF reachable only by `root`
     racing `debugfs` reads is labeled `Critical`.
   - `Medium` explicitly lists *"Issues in kselftests, perf and other
     userspace applications"*, while `Low` is restricted to naming and
     coding style, forcing trivial bugs in `tools/testing/selftests/` into
     `Medium`.
   - `High` lists generic logic errors, resource leaks, and locking
     violations without factoring in privilege requirements, narrow race
     windows, or cold one-shot error paths, diluting `High` with low-risk
     issues.

## 2. Goals

1. Load `Documentation/process/threat-model.rst` at run time directly from
   the mainline Linux git ref (`torvalds/linux` remote `master`, falling
   back to `origin/master`, `master`, `HEAD`), with graceful degradation
   when absent.
2. Recalibrate `projects/linux/prompts/severity.md` across consequence,
   threat-model actor/privilege boundaries, and trigger likelihood so that:
   - `Critical` is reserved for genuine catastrophic issues (true kernel
     security boundary breaches, silent/persistent data corruption,
     hot-path/normal-workload crashes or deadlocks, UAPI breakage);
   - `High` represents high-confidence kernel defects well worth
     immediate maintainer attention on realistic runtime or normal
     administrative paths;
   - `Medium` covers non-negligible contained kernel defects (cold/error
     leaks, crashes requiring *both* administrative privilege/`debugfs`
     *and* a subtle race or contrived sequence, selftest false-pass bugs,
     or speculative serious bugs);
   - `Low` covers minor `kselftests`/tools/scripts bugs, debug/staging
     edge cases, out-of-production theoretical issues, and style/hygiene
     items.
3. Supply the loaded kernel threat model (and ground-truth code/tool
   context in `SeveritySession`) to:
   - Standalone bug severity assessment (`src/workflows/linux_bug.rs`);
   - Patch review verification shards and post-verification
     (`src/workflows/verification_shards.rs`,
     `src/workflows/linux_patch_review.rs`);
   - Related pre-existing bug disclosure checks
     (`src/workflows/related_preexisting.rs`);
   - The `security` analysis stage (`src/workflows/linux_patch_review.rs`),
     strictly for identifying the actor and trust boundary without
     dismissing regular non-vulnerability bugs.

Non-goals: altering GCC, QEMU, or Sashiko severity guides; fetching files
over the network during reviews; changing database severity enum values.

## 3. Runtime Threat-Model Loader (`src/threat_model.rs`)

Add an unconditional library module `src/threat_model.rs` (usable in both
daemon builds and `--no-default-features` local reviews):

```rust
pub const THREAT_MODEL_PATH: &str = "Documentation/process/threat-model.rst";
pub const MAX_THREAT_MODEL_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainlineRef {
    pub git_ref: String,
    pub commit: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelThreatModel {
    pub text: String,
    pub commit: String,
    pub git_ref: String,
}
```

- **Mainline ref resolution** (`resolve_linux_mainline_ref`): inspects
  `git remote -v` via `git_cmd::in_dir_async`, preferring `{remote}/master`
  for any remote URL containing `torvalds/linux`, then falling back to
  `origin/master`, `master`, and `HEAD`. Resolves the commit SHA via
  `git rev-parse --verify --quiet --end-of-options {ref}^{commit}`.
- **Blob loading** (`KernelThreatModel::load`): reads
  `{commit}:Documentation/process/threat-model.rst` via `git show` with
  output capped at `MAX_THREAT_MODEL_BYTES + 1`. Missing files (e.g. an
  older local clone without mainline refs), oversized blobs, empty blobs,
  or invalid UTF-8 return `None` without failing the review or bug run.
- **Tailored rendering helpers**:
  - `render_for_severity(Option<&Self>) -> String`: wraps the verbatim
    document in `<kernel_threat_model source="...">...</kernel_threat_model>`
    followed by rules explaining how threat-model boundaries govern
    "security vulnerability" and untrusted reachability while preserving
    consequence- and likelihood-based calibration for regular bugs.
  - `render_for_disclosure_check(Option<&Self>) -> String`: wraps the
    document for `related-preexisting-check-*` so public-disclosure
    safety is judged directly against the upstream threat model (plus
    virtualization guest-to-host isolation).
  - `render_for_security_stage(Option<&Self>) -> String`: wraps the
    document for the `security` stage with an explicit directive never to
    dismiss a real bug merely because `threat-model.rst` classifies it as
    a regular bug rather than a security vulnerability.

## 4. Linux Severity Calibration (`projects/linux/prompts/severity.md`)

`projects/linux/prompts/severity.md` is rewritten around three explicit
calibration dimensions that must be stated at the start of
`severity_explanation`:

1. **Consequence and blast radius**: what actually breaks when triggered
   (silent persistent data corruption, arbitrary kernel memory corruption,
   whole-system panic/deadlock, broken subsystem feature, bounded cold
   leak, `WARN_ON_ONCE`, wrong errno/stat, or userspace test/tool issue).
2. **Actor, privilege, and threat-model boundary**: who can trigger the
   bug and whether it crosses a boundary guaranteed by the Linux kernel
   threat model (or guest-to-host/hypervisor isolation in virtualization
   code). Issues requiring `root` or elevated capabilities
   (`CAP_SYS_ADMIN`, `CAP_NET_ADMIN`, `CAP_SYS_RAWIO`, `CAP_SYS_MODULE`),
   `debugfs`/staging/experimental/debug Kconfigs, administrator-mounted
   crafted filesystem images, or physically modified/emulated hardware are
   **not** security vulnerabilities and never receive security-based
   escalation to `Critical`.
3. **Likelihood, trigger preconditions, and confidence**:
   - In patch review, never *drop* a bug or invent unproven privilege
     guards when callers are outside the diff.
   - However, whenever concrete preconditions *are* established in code
     (administrative capability checks, `debugfs`, driver `probe` failure
     unwind, `CONFIG_STAGING`, or a narrow multi-CPU race window without
     attacker timing control), factor those preconditions directly into
     the severity level.
   - Speculative findings resting on an unproven assumption remain capped
     at `Medium` (or `Low` when the consequence itself is minor).

Tier summary:

| Tier | Meaning | Examples | Explicit Exclusions |
|---|---|---|---|
| `Critical` | Catastrophic production impact or true kernel security vulnerability | Unprivileged/remote/guest security boundary breach (UAF, OOB write, LPE, cross-user data leak, unprivileged panic); silent/persistent filesystem or block corruption; normal-workload hot-path panic/deadlock; UAPI breakage | Root/`CAP_SYS_ADMIN`-only paths; `debugfs`/staging/debug Kconfigs; crafted mounted FS images (unless corrupting valid FS); narrow uncontrolled races on privileged paths; all `kselftests`/userspace tools |
| `High` | High-confidence serious kernel bug worth immediate maintainer action | Crash/UAF/OOB/deadlock on realistic runtime or normal admin operations (device unbind, hotplug, network config, clean mount/umount); broken primary driver/subsystem functionality; recurring leak on normal paths; major hot-path regression; reachable lock-order inversion or sleep-in-atomic | Speculative findings; root/`debugfs` + narrow race combinations; one-shot `probe`/init error-unwind leaks; `kselftests`/docs/scripts |
| `Medium` | Contained real defect with concrete impact under specific/uncommon conditions | Cold/one-shot error-path resource leaks; oops/UAF/deadlock requiring *both* `root`/`CAP_SYS_ADMIN`/`debugfs` *and* a narrow race or contrived sequence; wrong errno/stats or secondary-path logic bug; non-fatal `WARN_ON`/lockdep splat; selftest bug causing false pass or broken suite run; speculative serious bug | Minor leaks/glitches in short-lived selftests or scripts; pure style/comments/`Fixes:` |
| `Low` | Minor, non-production, improbable, or hygiene issue | Minor bugs in `kselftests`, `samples/`, `scripts/`, `tools/` (missing `free`/`close` before exit, minor edge cases); minor debug/staging-only or out-of-production theoretical issues; small non-steerable padding/pointer leaks without boundary breach; style, comments, docs, `Fixes:` trailers | — |

## 5. Workflow Integration

1. **Bug pipeline (`src/workflows/linux_bug.rs`)**:
   - Align `get_master_sha` with `resolve_linus_sha` so `torvalds/linux`
     remotes are preferred consistently.
   - Load `KernelThreatModel` once per Linux bug worker run when a
     repository worktree is available.
   - Compute `effective_prefetched` before `SeveritySession` and pass
     `kernel_threat_model`, `verification_reasoning`,
     `prefetched_context`, `master_sha`, and `tools` into
     `SeveritySession` so it can verify caller privileges, `debugfs`
     exposure, and `Kconfig` guards before assigning `High` or `Critical`.
2. **Patch review state & verification (`src/worker/prompts.rs`, `src/workflows/linux_patch_review.rs`, `src/workflows/verification_shards.rs`)**:
   - Store `kernel_threat_model: Option<KernelThreatModel>` on
     `LinuxPatchReviewState` (populated only for `ProjectId::Linux`).
   - Inject `{{kernel_threat_model_section}}` right after
     `<severity_guidelines>` in `verification_shards.rs`,
     `verification_stage_with_effort`, and
     `post_verification_stage_with_effort`.
3. **Related pre-existing disclosure (`src/workflows/related_preexisting.rs`)**:
   - Inject `{{kernel_threat_model_section}}` into `disclosure_check_stage`.
   - Rewrite `CHECK_INSTRUCTION` question 1 to judge `SecuritySensitive`
     by `Documentation/process/threat-model.rst` (plus virtualization
     guest isolation), removing the blanket classification of crafted
     filesystem images and non-conforming devices as security-sensitive
     while preserving fail-closed `Unknown` -> `Drop` behavior.
4. **Security analysis stage (`src/workflows/linux_patch_review.rs`)**:
   - Append `{{kernel_threat_model_section}}` to the `security` stage user
     prompt and instruct the stage to record the concrete actor, required
     capabilities, and crossed trust boundary in concern reasoning while
     never dismissing genuine non-vulnerability bugs.

## 6. Implementation Plan

1. `designs`: add this design document.
2. `threat_model`: load and render `Documentation/process/threat-model.rst`
   from the mainline Linux ref with unit tests.
3. `prompts: linux`: recalibrate `projects/linux/prompts/severity.md` around
   consequence, threat-model boundaries, and trigger likelihood.
4. `linux_bug`: pass the kernel threat model, verified reasoning,
   prefetched code, and tools to `SeveritySession`.
5. `linux_patch_review`: wire `KernelThreatModel` into review state and
   verification/post-verification stages.
6. `related_preexisting`: align disclosure check with the kernel threat
   model.
7. `linux_patch_review`: supply the kernel threat model to the `security`
   stage.
