+++
title = "Locking"
paths = []
symbols = ["spin_lock*", "mutex_*", "rwsem*", "seqlock*", "seqcount*", "read_seqcount*", "write_seqcount*", "raw_write_seqcount*"]
+++
# Locking: maintainer notes

Review conventions and judgment calls for patches that add or change kernel locking.

- When a patch adds a second lock type or mode to a function that previously handled only one, rename generic names (e.g. `locked`, `flags`, `state`) to disambiguate (`mmap_locked` vs `vma_locked`) rather than adding a comment to work around a name that is now ambiguous; make sure comments about "the lock" still refer to one specific lock.
- Prefer completion variables (`wait_for_completion()`/`complete()`) over open-coded spinlock polling loops.
- Assume the patch author's memory barrier usage is correct unless it is clearly wrong (e.g. a missing paired barrier, or `smp_wmb()` where `smp_mb()` is needed); subtle barrier issues need deep architecture knowledge and should not be flagged speculatively.
- Do not dismiss a race because the window is small: if the ordering violation can happen at all, treat it as a bug that must be fixed.
