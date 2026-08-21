# Syscall Subsystem Guide

## ABI Compatibility and Extensibility

- **Syscall signatures are immutable**: Existing system call parameter
  lists cannot be modified or extended with additional arguments without
  breaking userspace ABI.
- **Extensible structs**: Modern system calls taking struct pointers must
  use `copy_struct_from_user()` along with a `size` argument to allow
  future expansion while verifying trailing bytes are zeroed.
- **New system calls**: When adding parameters to legacy fixed-argument
  syscalls, a new numbered system call variant (e.g., `openat2`, `clone3`)
  must be introduced.

## Syscall Parameter Trust Boundaries

Syscall parameters come from user-controlled registers or stack slots.
Parameters that are only meaningful when a specific flag is set may contain
arbitrary garbage when that flag is absent — userspace is not required to
zero-fill unused arguments.

When syscall arguments are copied into a kernel struct, each field
inherits the trust boundary of its source argument and remains garbage
outside the flag gate even though it looks initialized in C. When
refactoring moves a check across a flag gate, verify that every variable
the check uses is valid in the broader scope.

**REPORT as bugs**: Any validation, arithmetic, or comparison that uses
a flag-gated syscall parameter outside the scope of its flag gate.
