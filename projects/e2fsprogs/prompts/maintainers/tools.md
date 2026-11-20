+++
title = "Filesystem Utilities (misc, resize2fs, debugfs, scrub)"
paths = ["misc/", "resize/", "debugfs/", "scrub/"]
symbols = ["resize2fs_*", "debugfs_*"]
+++
# Filesystem Utilities: maintainer notes

Cross-utility consistency, mounted-filesystem safety, and rollback policy for
`mke2fs`, `tune2fs`, `dumpe2fs`, `e2image`, `e2undo`, `resize2fs`, `debugfs`,
and `e2scrub`.

- Offline mutating operations in `mke2fs`, `tune2fs`, `resize2fs`, and `e2undo`
  must preserve mounted-filesystem (`ext2fs_check_if_mounted` /
  `ext2fs_check_mount_point`) and Multi-Mount Protection (`EXT4_FEATURE_INCOMPAT_MMP`)
  safeguards before writing to a block device (`High` if bypassed without an
  explicit force flag).
- When adding or changing support for an `ext4` feature flag, superblock field,
  or extended option, check horizontal consistency across `mke2fs`, `tune2fs`,
  `dumpe2fs`, `debugfs`, `resize2fs`, and `lib/e2p/` so utilities do not create
  or modify filesystems that sibling tools misinterpret.
- When `undo_io` (`e2undo` tdb logging) is supported by a utility, ensure new
  device-modifying paths route writes through the configured `io_channel` stack
  and flush/close the undo channel cleanly on both success and failure paths.
- Source files in `debugfs/` that are also compiled into `e2fsck` or `misc`
  utilities (such as journal dump/recovery helpers) must keep `#ifdef` guards
  and symbol dependencies compatible with every consumer binary (`Medium` if a
  shared file breaks linking in another directory).
