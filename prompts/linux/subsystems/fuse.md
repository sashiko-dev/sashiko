# FUSE Subsystem Details

## io-uring ring pointer access

`fch->ring` is published with `smp_store_release()` after the ring's fields
have been initialized. `smp_load_acquire()` is not necessary since readers
only dereference through the pointer, so READ_ONCE() is sufficient for any
lockless reads that can concurrently race with the publish.

**Not a bug**: a lockless reader of `fch->ring` using `READ_ONCE()` instead of
`smp_load_acquire()`.

## io-uring SQE data stability

fuse reads the submission queue entry through the ->uring_cmd handler.
`fuse_uring_cmd()` uses `io_uring_sqe128_cmd(cmd->sqe, ...)` and
`READ_ONCE(cmd->sqe->...)` to access the sqe fields. This is correct and how
uring_cmd handlers should be accessing the sqe fields.

**Not a bug**: reading `cmd->sqe` fields at issue time instead of caching the
sqe's fields at prep time. `uring_cmd` has no `->prep()` and does not need
one, since the SQE is stable at issue time (the first issue runs while the
ring slot is valid and io_uring copies the whole SQE before any async
retries).
