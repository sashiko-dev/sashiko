+++
title = "Syscalls"
paths = []
symbols = ["SYSCALL_DEFINE*", "copy_from_user", "copy_to_user", "get_user", "put_user"]
+++
# Syscalls: maintainer notes

These notes cover ABI policy for changes to system call definitions.

- Adding arguments to an existing syscall is not an ABI break as long as the existing arguments are left unchanged; do not flag such additions as ABI breakage.
