# Reviewing Zephyr

Sashiko can review commits in a local checkout of
[Zephyr RTOS](https://github.com/zephyrproject-rtos/zephyr). Support is
experimental and covers local review only: there is no hosted Zephyr instance, and
Sashiko does not ingest Zephyr pull requests.

> [!IMPORTANT]
> Review sends the commit and the surrounding source to your LLM provider. In
> testing, one commit took 0.7 to 1.9 million input tokens with Sonnet and about 4
> million with Opus, even for a small change, and a few Opus reviews used up a
> subscription's five-hour limit. Start with a small commit to see what a review
> costs on your plan.

## Setup

Install Sashiko from a checkout. Local review needs none of the daemon features, and
the `cache` feature is what makes `--resume` work:

```bash
cargo install --path . --locked --no-default-features --features cache --bin sashiko
```

Get the Zephyr source, if you do not have it. The commits you review must be in
this repository's history:

```bash
git clone https://github.com/zephyrproject-rtos/zephyr.git
```

Configure a provider. [`docs/examples/Settings.zephyr.toml`](examples/Settings.zephyr.toml)
uses the Claude Code CLI with your subscription, which is what this support was
tested with; see [LLM Provider Configuration](llm-providers.md) for the others.
Copy it to a separate file and pass it with `--settings`. The command below
leaves an existing Zephyr settings file unchanged:

```bash
mkdir -p ~/.config
cp -n docs/examples/Settings.zephyr.toml ~/.config/sashiko-zephyr.toml
```

A review has to read the source. Codex CLI 0.165.0 may use its native shell,
which runs in a read-only bwrap sandbox that needs unprivileged user namespaces.
Where the host restricts them, as `kernel.apparmor_restrict_unprivileged_userns=1`
does, those shell commands fail. If a report says source access failed, do not
trust it.

Setting `kind = "zephyr"` under `[project]` makes `--project zephyr` the default for
that file, and it makes a run with a different `--project` fail. Without it, pass
`--project zephyr` each time, or set `SASHIKO_PROJECT=zephyr`.

## Reviewing commits

Run the review from inside the Zephyr checkout:

```bash
# Review the latest commit
sashiko review --project zephyr --settings ~/.config/sashiko-zephyr.toml

# Review one commit
sashiko review --project zephyr --settings ~/.config/sashiko-zephyr.toml 0c4382ae5e7d

# Review a range; each commit is checked separately
sashiko review --project zephyr --settings ~/.config/sashiko-zephyr.toml HEAD~3..HEAD

# Also report bugs that existed before the change
sashiko review --project zephyr --settings ~/.config/sashiko-zephyr.toml --report-preexisting HEAD

# Continue a review cut short by a quota stop, a timeout or Ctrl-C
sashiko review --project zephyr --settings ~/.config/sashiko-zephyr.toml HEAD~3..HEAD --resume
```

Review reads commits from git. It does not build Zephyr, does not apply patches to
your working tree, and does not change it. Lower `review.concurrency` if you hit
rate limits.

## Reading the results

Progress goes to stderr and the report to stdout. For structured findings, use
`--format json`. A human-readable report may include a bullet such as:

```text
- [HIGH] In drivers/sensor/foo/foo.c (foo_irq_handler), k_mutex_lock() is
  called from the ISR. kernel/mutex.c only asserts against this, so builds
  without CONFIG_ASSERT corrupt the mutex wait queue. Offload to a work item.
```

`Critical` is reserved for defects an external peer or a user-mode thread can reach,
and for defects that can brick a device.
`High` is a crash, deadlock, or data loss on a realistic path. Findings marked
`(pre-existing)` were not introduced by the change.

The command exits with 1 if a new `High` or `Critical` finding is reported.
Pre-existing findings do not change the exit status. Exit 3 means the review did
not complete, for example at a usage limit.

Treat a report as a list of candidates and check each one against the code. The
review cannot see datasheets or modules outside the tree, and it does not run CI, so
`scripts/ci/check_compliance.py` and Twister are still needed.

## What the review covers

Three stages always run: `goal`, `implementation`, and `execution-flow`. A planning
stage adds these when the patch calls for them:

| Stage | Looks for |
|---|---|
| `resources` | Leaks, object lifetime against pending work and timers, power management balance, DMA buffer ownership |
| `concurrency` | Blocking in the wrong execution context, locks, SMP, workqueues, unbounded work in ISRs |
| `configuration` | Kconfig, Devicetree, and CMake logic that disagrees with the code it gates |
| `security` | Untrusted input from peers, system call verification, flash and update paths |

Findings are checked against the policy in
[`projects/zephyr/prompts/`](../projects/zephyr/prompts/README.md). Each maintainer
note there names the Zephyr document it comes from, and is loaded only for patches
it applies to.

Not covered yet: generated code notes, a benchmark suite, per-subsystem notes for
networking and Bluetooth, and modules outside the Zephyr tree.
