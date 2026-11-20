+++
title = "User Mode and System Calls"
paths = [
    "kernel/userspace/",
    "include/zephyr/internal/syscall_handler.h",
]
symbols = [
    "K_OOPS",
    "K_SYSCALL_*",
    "__syscall",
    "k_object_*",
    "k_usermode_*",
    "z_impl_*",
    "z_vrfy_*",
]
+++

# User Mode and System Calls

Applies when `CONFIG_USERSPACE` is enabled. This is Zephyr's privilege boundary:
everything a user-mode thread passes in is untrusted, and a missing check here is
a security defect, not a robustness issue.

## Verification functions

Source: doc/kernel/usermode/syscalls.rst.

- Each `z_impl_` function has a `z_vrfy_` verifier that runs before it. Compare
  the two: every user pointer, buffer size, and kernel object the implementation
  uses must be validated by the verifier.
- General parameter checks belong in the implementation, usually through
  `CHECKIF()`, and return an error.
  Do not ask for them in the verifier. Only checks enforced solely for user mode
  go there, and those return a value where possible.
- Kernel object pointers are checked with `K_SYSCALL_OBJ()`, or its `_INIT` and
  `_NEVER_INIT` variants for init functions.
- Buffers are checked with `K_SYSCALL_MEMORY_READ()` or `K_SYSCALL_MEMORY_WRITE()`
  according to what the implementation does. Arrays use
  `K_SYSCALL_MEMORY_ARRAY_READ()` or `_WRITE()`, which check for integer overflow
  in the size. A read check on a buffer that is written to is a defect.
- Driver system calls check the device and the operation with
  `K_SYSCALL_DRIVER_OP()` or `K_SYSCALL_SPECIFIC_DRIVER()`.
- Failed memory and kernel object checks use `K_OOPS()` to kill the calling
  thread. A failed heap allocation returns `-ENOMEM` instead. Verifiers must not
  depend on `__ASSERT()`.
- A kernel-mode callback must never be registered from user mode. An API that only
  installs a callback must not be a system call, and a verifier taking an optional
  callback pointer must require NULL and `K_OOPS()` otherwise.

## TOCTOU

Source: doc/kernel/usermode/syscalls.rst (Verification Memory Access Policies).

- A user thread can change memory passed by reference at any time. If the kernel
  makes a decision based on that memory, the verifier must copy it with
  `k_usermode_from_copy()` and check the copy, then write results back with
  `k_usermode_to_copy()`. Reading a structure from user memory twice is a defect.
- Copy strings with `k_usermode_string_copy()` or
  `k_usermode_string_alloc_copy()`, since the length is not known in advance.
- The exception in that document covers large data buffers that are only written
  to, or whose contents are never used for validation or control flow. Do not ask
  for copies of those. Access to the buffer must still be validated, and it may
  only be used synchronously: saving its address for use after the call, such as
  from an interrupt, is a defect.

## Declaring system calls

Source: doc/kernel/usermode/syscalls.rst; doc/kernel/usermode/kernelobjects.rst.

- `__syscall` must be the first token of the prototype. A header declaring system
  calls includes its generated header. Array arguments are passed as pointers.
- Kernel objects live in memory user threads cannot access, and access requires
  permission. Flag a kernel object placed in a user-accessible memory partition.
