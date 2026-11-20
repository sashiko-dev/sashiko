+++
title = "Support and Legacy Shared Libraries"
paths = ["lib/e2p/", "lib/et/", "lib/ss/", "lib/support/", "lib/blkid/", "lib/uuid/"]
symbols = ["e2p_*", "com_err*", "quota_*"]
+++
# Support and Legacy Shared Libraries: maintainer notes

ABI policy and internal-vs-exported boundaries for `libe2p`, `libcom_err`,
`libss`, `libblkid`, `libuuid`, and `libsupport`.

- `libe2p`, `libcom_err` (`lib/et/`), `libss` (`lib/ss/`), `libblkid`, and
  `libuuid` are installed shared libraries with external consumers: preserve
  exported function signatures, struct layouts, and symbol versions (`High` if
  broken), and never print errors directly to `stdout`/`stderr` inside
  `libe2p`, `libblkid`, or `libuuid`.
- `libsupport` (`lib/support/`) is an internal static convenience library used
  only by `e2fsprogs` binaries (quota helpers, profile parser, rbtree/dict
  containers, plausible-device checks); `printf`/`com_err` diagnostics are
  permitted in `libsupport`, and its internal APIs may evolve alongside all
  in-tree callers without shared-library ABI wrappers.
- When adding a new feature name to `lib/e2p/feature.c`, use `git_grep` to
  confirm that its bit constant is defined in `lib/ext2fs/ext2_fs.h`
  (`lib/ext2fs/kernel-jbd.h` for journal features), that a constant the patch
  newly defines does not reuse a bit already used or reserved in the same
  feature field, and that the table entry's class matches the constant's
  field: `E2P_FEATURE_COMPAT` for `*_FEATURE_COMPAT_*`, `E2P_FEATURE_INCOMPAT`
  for `*_FEATURE_INCOMPAT_*`, and `E2P_FEATURE_RO_INCOMPAT` for
  `*_FEATURE_RO_COMPAT_*` (`High` if mapped to the wrong feature field or bit).
