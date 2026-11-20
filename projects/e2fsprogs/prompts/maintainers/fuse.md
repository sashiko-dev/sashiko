+++
title = "FUSE Filesystem Driver (fuse2fs / fuse4fs)"
paths = ["fuse4fs/", "fuse2fs/", "misc/fuse2fs.c"]
symbols = ["fuse4fs_*", "fuse2fs_*"]
+++
# FUSE Filesystem Driver: maintainer notes

Concurrency, privilege boundary, and error-translation policy for `fuse2fs` and
`fuse4fs`.

- Unlike single-threaded CLI runs of `mke2fs` or `dumpe2fs`, `fuse2fs`/`fuse4fs`
  services concurrent FUSE requests across worker threads while `ext2_filsys` is
  not internally thread-safe: every access to `ext2_filsys`, cached bitmaps,
  icount/inode caches, or JBD2 state must hold the driver's filesystem lock
  across the entire check-and-update sequence, and every error exit must release
  that lock (`High`).
- `fuse2fs`/`fuse4fs` is frequently used by unprivileged users to mount
  untrusted filesystem images (for example in container/OS build pipelines):
  audit permission checks, uid/gid translation, setuid/setgid bit clearing on
  write/chown/truncate, symlink/xattr bounds, and read-only mount enforcement at
  `High` or `Critical` severity depending on impact.
- FUSE callbacks must translate `errcode_t` / `EXT2_ET_*` failures into negative
  POSIX `-errno` values without leaking positive `errcode_t` numbers directly to
  the kernel FUSE layer (`Medium` or `High`).
