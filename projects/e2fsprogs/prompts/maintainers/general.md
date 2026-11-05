+++
title = "General"
paths = ["*"]
symbols = []
+++
# General: maintainer notes

Project-wide coding, portability, ABI, and patch submission conventions for
`e2fsprogs`.

- Every `.c` source file must include `"config.h"` as its very first header, and
  optional platform headers (`unistd.h`, `errno.h`, `getopt.h`, `sys/ioctl.h`,
  `sys/mount.h`) must remain guarded by `#ifdef HAVE_<HEADER>_H` so builds keep
  working across Linux (glibc, musl, Bionic), FreeBSD/NetBSD/OpenBSD, macOS,
  GNU/Hurd, and MinGW.
- `e2fsprogs` targets GNU99/C99 with strict `-Wall` sweeps: keep variable
  declarations at the start of blocks, never introduce variable-length arrays
  (VLAs), mark file-local functions and globals `static`, declare non-static
  functions in an included header, and mark intentionally unused parameters with
  `EXT2FS_ATTR((unused))` (especially across `#ifdef` feature combinations).
- Always use 64-bit block and offset types (`blk64_t`, `e2_blkcnt_t`,
  `ext2_off64_t`) and 64-bit `libext2fs` entrypoints in new or modified code
  rather than legacy 32-bit `blk_t` interfaces.
- Do not flag Linux-kernel K&R C formatting preferences (8-column tabs, opening
  brace placement, omitted braces on single-statement branches) unless the
  indentation or macro expansion creates an actual control-flow defect.
- Submitted patches require a `Signed-off-by: Real Name <email>` DCO trailer
  (report missing or clearly broken `Signed-off-by:` trailers as `High`), and
  bug-fix commits referencing an earlier commit should carry an accurate
  `Fixes:` trailer (report `Fixes:` and standalone commit-message hygiene issues
  no higher than `Low`).
