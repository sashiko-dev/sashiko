+++
title = "DAMON"
paths = ["mm/damon/", "include/linux/damon.h"]
symbols = ["damon_*", "damos_*"]
+++
# DAMON: maintainer notes

Conventions and design assumptions for the Data Access Monitor (DAMON) and Data Access Monitoring-based Operation Schemes (DAMOS).

- DAMON access monitoring and DAMOS quota and auto-tuning loops are best-effort sampling mechanisms by design; do not flag minor transient statistics or quota-tuning imprecision, pointer-reuse sampling noise, or multi-week 32-bit duration wraparounds that cannot cause crashes, memory corruption, leaks, or runaway scheme actions.
