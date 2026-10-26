+++
title = "C Error Propagation and Cleanup Discipline"
paths = []
symbols = [
    "errp",
    "local_err",
    "error_setg",
    "error_setg_errno",
    "error_propagate",
    "error_propagate_prepend",
    "error_prepend",
    "error_abort",
    "error_fatal",
]
+++

# Error Propagation (C)

QEMU's C error discipline: functions report failure through return values and
`Error **errp`, and every failure path must leave clean state.

## Return value discipline

- Every non-void call whose failure is possible must have its return value
  checked on every path, or a documented reason in the commit for ignoring it.
- `goto fail` cleanup labels: verify each cleanup label actually releases
  everything acquired before it — allocations, references, memory regions,
  fds — in the reverse order of acquisition, and that a newly added resource
  in the function was added to the existing cleanup ladder.

## `Error **errp` discipline

- A function that can fail takes `Error **errp`, sets it via `error_setg`
  (family) before returning failure, and callers pass either a local `Error
  *local_err` they propagate with `error_propagate`, or `&error_abort` only
  when the failure is genuinely unrecoverable — a guest-reachable failure is
  never unrecoverable.
- Returning failure without setting `*errp` turns the caller's error message
  into `NULL`; flag it.
- Setting `*errp` and then returning success (or returning failure and then
  continuing to use partially initialized state) is a bug.

## Half-applied mutations

- On failure, the device/global state must be exactly what it was before the
  call. Flag sequences where step 1 mutated visible state, step 2 failed, and
  the error path does not roll step 1 back.
