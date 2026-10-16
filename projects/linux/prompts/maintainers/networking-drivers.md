+++
title = "Networking Drivers"
paths = ["drivers/net/"]
symbols = ["ethtool_ops", "net_device_ops"]
+++
# Networking Drivers: maintainer notes

Netdev community policies for network driver patches, covering statistics uAPI choices and accepted synchronization constructs.

- Do not add counters to `ethtool -S` (get_ethtool_stats / get_sset_count / get_strings) when a standard uAPI already exists for them; duplicates cause confusion, bloat ethtool -S output and add maintenance burden.
- `ethtool -S` is reserved for driver-private statistics specific to the hardware or driver that have no standard representation.
- Statistics fitting an existing standard category must use the standard interface instead: rtnl_link_stats64 netdev stats, per-queue stats via netlink (netdev_queue_stats_rx/tx), page pool stats via netlink, dedicated ethtool_ops stats callbacks, or other standardized netlink attributes. Documentation/networking/statistics.rst describes which interface to use.
- Report a duplicated ethtool -S stat only after identifying at least one newly added counter that has a standard interface, regardless of whether the driver currently implements that interface.
- Pre-existing ethtool -S stats that predate the standard uAPI are not a finding in new patches; migrating them is a separate cleanup.
- The community rejects homebrew synchronization: flags, atomics or bit operations set on entry and cleared on exit to guard a code region should be replaced by a real lock (mutex, spinlock) or RCU.
- Trylock loops used to sidestep lock ordering are treated as a sign of a wrong locking design; the accepted exception is a work item that reschedules itself when mutex_trylock() fails.
