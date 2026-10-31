# Design: Zephyr Review

## 1. Overview

Add Zephyr RTOS as a Sashiko project, so that a contributor can run
`sashiko review --project zephyr <commit-or-range>` in a Zephyr checkout. This is
initial support for local review. It follows the per-project structure in
`DESIGN_MULTI_PROJECT_REVIEW.md`.

### 1.1 Goals

1. `ProjectId::Zephyr`, selected like any other project.
2. A Zephyr review workflow with stages aimed at RTOS defects.
3. A `projects/zephyr/prompts/` tree in which each rule cites the Zephyr document
   or source file it comes from.

### 1.2 Non-goals

- A hosted instance. Zephyr uses GitHub pull requests; ingesting them is daemon
  work. `projects/zephyr/mailing_lists.toml` exists because every project has one,
  and it tracks nothing.
- Daemon review. Baseline resolution knows Zephyr's remote and its `main` branch,
  because the code paths are shared, but no Zephyr daemon has been run.
  The reviewer passes the configured project to baseline resolution, so a fork
  with a different owner or repository name still uses `main` and skips Linux
  baseline candidates. Upstream bug checks also select remotes and branches for
  the configured project rather than accepting another project's reference tree.
- A benchmark suite and generated code notes. See section 5.

## 2. Workflow

`src/workflows/zephyr_patch_review.rs` is a sibling of `qemu_patch_review.rs`: same
graph, same state type, same consolidation pipeline. Forking matches the decision
in `DESIGN_MULTI_PROJECT_REVIEW.md`. The concern validators and the plain-text
report helpers have nothing QEMU-specific in them, so the Zephyr workflow imports
them rather than adding a fourth copy.

### 2.1 Stages

| Stage | Optional | Scope |
|---|---|---|
| `goal` | no | Intent, fit with Zephyr's patterns, API lifecycle, commit message |
| `implementation` | no | Does the code do what the message claims; touch points of new options, properties, and API callbacks |
| `execution-flow` | no | Control flow, arithmetic, return values, conditions guarded only by `__ASSERT()` |
| `resources` | yes | Memory, pools, object lifetime against pending work and timers, device lifecycle, PM balance, DMA ownership |
| `concurrency` | yes | Execution context, blocking in the wrong context, locks, SMP, workqueues, timing |
| `configuration` | yes | Kconfig, Devicetree, and CMake agreeing with the code they gate |
| `security` | yes | External input, system call verification, flash and update paths |

`concurrency` replaces QEMU's `locking` because the first question in Zephyr is
which context the code runs in. `configuration` has no QEMU counterpart: one source
file builds under many configurations, and a Kconfig or Devicetree error can build
cleanly and still be wrong. It asks for series context because a later commit in a
series often adds the matching binding or option.

### 2.2 Differences from the QEMU prompts

The consolidation stages tell the model to discard suspected build and device-tree
noise. For Zephyr, Devicetree is a review subject, so that wording is gone. Only
what `scripts/ci/check_compliance.py` or a Twister build reports is discarded, and
post-verification states that a Kconfig or Devicetree logic error is not noise.

## 3. Prompts

`maintainers/` holds one note per topic, selected from the diff:

| Note | Loaded by |
|---|---|
| `general` | every patch |
| `execution-context` | `arch/`, `drivers/`, `kernel/`, `soc/`, or interrupt, timer, and callback symbols |
| `synchronization` | `kernel/`, or lock, workqueue, and thread symbols |
| `device-drivers` | `drivers/`, `include/zephyr/drivers/`, or `DEVICE_*` symbols |
| `devicetree` | `dts/`, `boards/`, `.dts`, `.dtsi`, and overlay files, or `DT_*` / `DEVICE_DT_GET*` symbols |
| `kconfig` | Kconfig, `.conf`, and `_defconfig` files |
| `userspace` | `kernel/userspace/`, or system call symbols |
| `dma-and-cache` | `drivers/dma/`, `drivers/cache/`, or cache and DMA symbols |
| `power-management` | `subsys/pm/`, or `pm_device_*` symbols |
| `api-lifecycle` | `include/zephyr/` |
| `tests-and-samples` | `tests/`, `samples/` |

`kconfig` has no symbol triggers: `CONFIG_*` and `IS_ENABLED` appear in nearly every
C change, and the `configuration` stage already covers code that branches on them.

Optional stages are optional during planning. Every selected analysis stage and
post-verification batch must finish; a stage error fails the review rather than
allowing partial output to be reported as clean.

Each section names its source in the Zephyr tree. A rule that could not be traced
to the tree was left out, which is also why nothing is said about `z_` prefixes:
the documentation does not define them as private.

## 4. Touch points

Adding the variant forces an arm in every `match` on `ProjectId`, including the bug
pipeline in `linux_bug.rs`, which local review with `--report-preexisting` does not
reach but which must still compile. Those arms carry Zephyr wording rather than a
shared fallback, so a future daemon does not inherit QEMU's vocabulary.

`db.rs` keeps audit tools in SQL `CASE` expressions, which the compiler does not
check. Each QEMU clause has a Zephyr sibling, and a test covers them.

## 5. Open questions

- **Benchmark.** `docs/benchmarking.md` describes suites built from known bugs.
  QEMU support landed without one. Zephyr commits carrying a `Fixes:` trailer that
  names the introducing commit would make a good source for a suite.
- **Code notes.** Not generated yet. `west.yml` modules live outside the tree, which
  the notes profile would need to handle.
