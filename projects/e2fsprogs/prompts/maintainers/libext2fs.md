+++
title = "libext2fs Core Library"
paths = ["lib/ext2fs/"]
symbols = ["ext2fs_*", "ext2_filsys"]
+++
# libext2fs Core Library: maintainer notes

ABI stability, untrusted image handling, endianness, and error-handling policy
for `libext2fs`.

- Shared-library ABI compatibility is strict (`High` if broken): never change
  the signature, parameter types, return convention, or observable semantics of
  an existing exported `ext2fs_*` function, and never change the size or field
  offsets of public structs in `lib/ext2fs/ext2fs.h` without consuming explicit
  reserved padding fields. When new flags or parameters are needed, introduce a
  numbered successor (such as `ext2fs_foo2`) and keep the older symbol as a
  compatibility wrapper.
- New `errcode_t` definitions in `lib/ext2fs/ext2_err.et.in` must always be
  appended at the very end of the error table so existing numeric error codes do
  not shift (`High`).
- `libext2fs` routines must never write diagnostics to `stdout` or `stderr` via
  `printf` / `fprintf`; return an `errcode_t` to the caller instead (`Low` if
  diagnostic-only, higher if it corrupts protocol output).
- Treat all on-disk metadata read through `io_channel` as untrusted input:
  validate sizes, offsets, shift exponents, and loop bounds before allocation or
  pointer arithmetic, swap raw little-endian on-disk fields with
  `ext2fs_le*_to_cpu` / `ext2fs_cpu_to_le*` when not already swapped by a
  higher-level helper, and keep `metadata_csum` and `bigalloc` cluster-vs-block
  conversions consistent.
- Allocate heap memory with `ext2fs_get_mem` / `ext2fs_get_memzero` /
  `ext2fs_get_array` / `ext2fs_resize_mem` and release via
  `ext2fs_free_mem(&ptr)` through a single initialized `errout:` / `cleanup:`
  exit path.
