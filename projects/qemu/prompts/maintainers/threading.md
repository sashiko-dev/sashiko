+++
title = "Threading, BQL, and iothread Discipline"
paths = [
    "util/qemu-thread*",
    "util/aio*",
    "util/async.c",
]
symbols = [
    "bql_lock",
    "bql_unlock",
    "bql_block*",
    "AioContext",
    "aio_context_acquire",
    "aio_context_release",
    "aio_bh*",
    "qemu_bh_*",
    "qemu_mutex_*",
    "smp_mb",
    "qatomic_*",
]
+++

# Threading & BQL

QEMU device code runs concurrently on vCPU threads, the main loop, and
iothreads. Concurrency findings must name the concrete interleaving.

## BQL discipline

- Most legacy device code expects the Big QEMU Lock. State reachable from a
  BQL-protected handler must not be mutated from a context that runs without
  the BQL (main loop bottom halves, iothread handlers, RCU callbacks), or must
  be protected by its own documented lock.
- Handlers or bottom halves explicitly designed to run outside the BQL must not
  touch BQL-protected state without taking the lock.

## Lock ordering

- Acquiring the BQL while holding a per-device or per-AioContext lock, while
  another path takes them in the opposite order, is a deadlock. Name both
  paths.
- Never block the main loop thread on I/O without dropping the BQL where the
  protocol requires it.

## Cross-thread state

- A field mutated from more than one thread is either atomic or lock-protected;
  "it is only a flag" is not protection. Check publish/consume ordering: the
  reader must not observe a half-initialized object because the pointer
  publish was reordered before the initializer stores.
- `smp_mb`/atomics pairing: a relaxed store published against must be read with
  matching acquire semantics on the consuming side.

## Race windows

- Hotplug against in-flight guest DMA; unrealize against a pending timer; two
  vCPUs writing the same register with read-modify-write on shared state.
- The finding must name the two racing code paths; an unnamed "this could race
  with something" is not a finding.
