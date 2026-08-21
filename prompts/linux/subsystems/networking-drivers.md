# Networking Drivers: Stats, and Synchronization

## Per-Device Statistics with u64_stats_sync

Driver pre-NAPI/per-queue/per-CPU statistics that count packets and bytes
use 64-bit counters that can tear on 32-bit architectures (a reader observes
the low half of one update combined with the high half of the next).
The `u64_stats_sync` helpers in `include/linux/u64_stats_sync.h` close that
race using a seqcount on 32-bit and compile to no-ops on 64-bit.

A counter is declared as `u64_stats_t` (paired with one `struct
u64_stats_sync` per per-CPU container, initialized via `u64_stats_init()`).
Writers wrap updates in `u64_stats_update_begin()` /
`u64_stats_update_end()`; readers retry around `u64_stats_fetch_begin()` /
`u64_stats_fetch_retry()`. Using `u64_stats_t` with typed accessors
(`u64_stats_add`, `u64_stats_read`, etc.) ensures compiler enforcement.

```c
/* writer (per-CPU, BH-disabled or otherwise serialized) */
u64_stats_update_begin(&stats->syncp);
u64_stats_add(&stats->bytes, len);
u64_stats_inc(&stats->packets);
u64_stats_update_end(&stats->syncp);

/* reader (e.g., ndo_get_stats64) */
do {
    start = u64_stats_fetch_begin(&stats->syncp);
    bytes = u64_stats_read(&stats->bytes);
    pkts  = u64_stats_read(&stats->packets);
} while (u64_stats_fetch_retry(&stats->syncp, start));
```

Constraints that are easy to get wrong:

- **Writers must be mutually exclusive** for a given `syncp`. Per-CPU
  or per-NAPI stats satisfy this because each CPU writes its own copy
  from a non-preemptible context (NAPI, softirq).
- **Writers must run with preemption disabled.** Per-CPU + BH-disabled (or
  hardirq) context already provides this. If a writer can be preempted by a
  reader, the reader spins forever on 32-bit.
- **Use the `_irqsave` variant when an IRQ handler may also write the same
  `syncp`, or read it.** `u64_stats_update_begin_irqsave(&stats->syncp, flags)` /
  `u64_stats_update_end_irqrestore(&stats->syncp, flags)` are no-ops with respect
  to interrupts on 64-bit but disable interrupts on 32-bit.
- Readers may sleep or be preempted; they perform pure reads.
- Reader retry loops must be idempotent on retry, common mistake is to
  use `+=` e.g. `retval->rx_bytes += u64_stats_read(&stats->bytes)`
  which will add the byte counter multiple times in case on retry.

## Ethtool Driver Statistics vs Standard Stats

Adding statistics to `ethtool -S` that duplicate counters for which a
standard kernel uAPI already exists creates confusion, leads to huge
ethtool -S lists, and adds maintenance burden.

- **Stats that have a standard uAPI must not be duplicated in `ethtool -S`.**
  The `ethtool -S` interface (`get_ethtool_stats()` / `get_sset_count()` /
  `get_strings()`) is for driver-private statistics only — counters that are
  specific to the hardware or driver and have no standard representation.
- Standard uAPIs exist for common SW-maintained and standards-defined HW
  counters. Categories with standard interfaces include:
  - Network device stats (`struct rtnl_link_stats64` via `ip -s link show`)
  - Per-queue statistics (via netlink, struct netdev_queue_stats_rx,
    struct netdev_queue_stats_tx)
  - Page pool statistics (via netlink, accessible through `ynl` tooling)
  - Ethtool statistics (for which there is a dedicated callback in
    `struct ethtool_ops`)
  - Other counters exposed through standardized netlink attributes
- When a driver wants to expose a statistic that fits an existing standard
  category, it should implement the appropriate standard interface
  rather than adding a private ethtool string.
- `Documentation/networking/statistics.rst` documents the statistics
  hierarchy and which interfaces to use.

**REPORT as bugs**: Driver patches that add strings to `ethtool -S`
for counters that have a standard uAPI. You must find at least one statistic
being added to the driver for which standard interface exists before reporting.
Pre-existing `ethtool -S` stats that predate the standard uAPI are not bugs
in new patches (migrating them is a separate cleanup).

## Ad-hoc Synchronization with Flags and Atomics

Driver code that uses atomic variables, bit flags, or boolean fields as
substitutes for real locks or RCU almost always contains races. These
homebrew schemes provide no actual synchronization guarantees and are
invisible to lockdep, so the bugs they introduce go undetected by
standard kernel debugging tools.

Common broken patterns:

- **Atomic/flag as gate guard**: reading an atomic or flag to decide whether
  to proceed, then operating on shared data without holding a lock. The
  flag's value can change immediately after the read, so the "protection"
  is illusory.
  ```c
  // WRONG: intr_sem can change right after the read
  if (atomic_read(&priv->intr_sem) != 0)
      return;
  // ... operates on shared state with no actual lock held
  ```

- **Bit flags as reader/writer protocol**: using `set_bit()` /
  `test_bit()` / `clear_bit()` to coordinate access between readers and
  a teardown path. Multiple concurrent readers can enter, one clears the
  bit while another is still mid-operation, and the teardown path frees
  memory that the remaining reader is still accessing.
  ```c
  // WRONG: concurrent readers race on the bit
  set_bit(STATE_READ_STATS, &priv->state);
  if (!test_bit(STATE_OPEN, &priv->state)) {
      clear_bit(STATE_READ_STATS, &priv->state);
      return;
  }
  // ... reads from shared data that close path may free
  clear_bit(STATE_READ_STATS, &priv->state);
  ```

- **Retry/poll loops on flags**: spinning on a flag waiting for another
  context to clear it, reimplementing a spinlock without the fairness,
  deadlock detection, or memory ordering guarantees.

- **Trylock loops to avoid deadlock**: using `mutex_trylock()` or
  `spin_trylock()` in a loop or repeated invocation to avoid a lock
  ordering issue is a sign that the locking design is wrong. Trylock is
  only acceptable in narrow cases — for example, a work item that calls
  `mutex_trylock()` and on failure reschedules itself (via
  `schedule_work()` / `schedule_delayed_work()`) so the work runs again
  later. Open-coded retry loops around trylock, or trylock with fallback
  to "skip the work entirely", are almost always bugs.

The correct alternatives depend on the access pattern:

- Reader-heavy paths (e.g., `ndo_get_stats64`): use RCU
- Mutual exclusion with sleep: use a `mutex`
- Mutual exclusion in atomic context: use a `spinlock_t`
- Preventing concurrent execution of a timer or work: use
  `del_timer_sync()` / `cancel_work_sync()`

**REPORT as bugs**: any pattern where a flag, atomic variable, or bit
operation appears to guard a section of code rather than express state —
i.e., where the flag is set on entry and cleared on exit of a code region
to prevent concurrent access, instead of using a proper lock or RCU.

## Quick Checks

- **u64_stats_sync writers**: must be mutually exclusive per `syncp` and run with preemption disabled; use the `_irqsave` variant when IRQ context also touches the same `syncp`
- **Ethtool -S stat duplication**: check whether any new `ethtool -S` counters cover values for which a standard uAPI exists (rtnl_link_stats64, page pool stats, per-queue stats via netlink), regardless of whether the driver currently uses that standard interface
- **Flags used as locks**: flag/atomic/bit set-on-entry clear-on-exit patterns that guard code sections are ad-hoc locks; use real locks or RCU instead
