# e2fsprogs False Positive Prevention Guide

Before validating a candidate finding, verify these `e2fsprogs`-specific API
contracts and conventions so idiomatic code is not flagged as a bug while real
defects are never rationalized away.

## 1. Patterns That Are NOT Bugs in `e2fsprogs`

1. **`ext2fs_free_mem(&ptr)` Null Safety and Pointer Zeroing**:
   - `ext2fs_free_mem(void *ptr)` takes the **address** of the pointer variable
     (`&buf`), frees `*buf` if non-null, and sets `*buf = NULL`.
   - Calling `ext2fs_free_mem(&buf)` when `buf == NULL` on an `errout:` path
     (provided `buf` was initialized to `NULL` at declaration!) is completely
     safe and idiomatic. Do NOT report a null-free bug on initialized `NULL`
     pointers passed to `ext2fs_free_mem(&buf)`, and do NOT claim `buf` is left
     dangling after `ext2fs_free_mem(&buf)`.
   - Conversely, passing `buf` instead of `&buf` to `ext2fs_free_mem`, or
     jumping to `errout:` *before* `buf = NULL` runs, IS a real bug (`Critical`
     wild free).
2. **`ext2fs_get_mem` / `ext2fs_get_memzero` / `ext2fs_get_array` Pointer-Address Convention**:
   - `ext2fs_get_mem(size, &buf)` expects `&buf` (a `void *` pointing to the
     caller's pointer variable), NOT `buf`. Do not misread `&buf` as an
     incorrect level of indirection.
3. **Legacy 32-Bit `libext2fs` Wrapper Functions Kept for Shared-Library ABI**:
   - `libext2fs` intentionally retains exported 32-bit functions (such as
     `ext2fs_bmap`, `ext2fs_block_iterate2`, `ext2fs_new_block`,
     `ext2fs_test_block_bitmap`) as compatibility wrappers around 64-bit `*2` /
     `*3` implementations so existing dynamically linked callers continue to
     link and run. Do NOT recommend deleting or changing the signatures of
     existing exported 32-bit functions.
   - However, **new internal callers** in `e2fsck`, `mke2fs`, `resize2fs`,
     `tune2fs`, `debugfs`, `fuse2fs`/`fuse4fs`, or new 64-bit `libext2fs` paths
     calling a 32-bit `blk_t` API instead of the `blk64_t` API IS a real `High`
     bug on >16 TiB filesystems.
4. **Byte-Swapping Already Performed by `libext2fs` High-Level Accessors**:
   - When `ext2fs_open` reads the superblock (`fs->super`) and group descriptors,
     or when callers use `ext2fs_read_inode` / `ext2fs_write_inode` /
     `ext2fs_read_ext_block` / `ext2fs_get_Next_extent`, `libext2fs` already
     swaps standard fields to host byte order on big-endian machines
     (`EXT2FS_BE`). Verify in the callee (`git_read_files` / `git_grep` for
     `ext2fs_swap_*`) whether a structure is already in host byte order before
     claiming a missing `ext2fs_le*_to_cpu` conversion—adding a second swap on
     an already-swapped field breaks big-endian hosts!
5. **`e2fsck` Bitmask Exit Codes and `longjmp` Abort Paths**:
   - `e2fsck` exit codes are bitwise OR combinations of `FSCK_NONDESTRUCT` (1),
     `FSCK_REBOOT` (2), `FSCK_UNCORRECTED` (4), `FSCK_ERROR` (8), `FSCK_USAGE`
     (16), and `FSCK_CANCELED` (32), NOT `0`/`1` booleans.
   - `fatal_error(ctx, ...)` and `ctx->abort_context` (`longjmp`) terminate
     `e2fsck` cleanly; do not report missing post-`fatal_error` cleanup in
     `e2fsck` CLI execution unless a partially written on-disk structure was
     flushed in an inconsistent state beforehand.
6. **Intentional Corruption in `tests/*` Fixtures**:
   - Regression tests under `tests/` deliberately construct invalid superblocks,
     overlapping extents, bad checksums, and orphan loops via `debugfs` to test
     `e2fsck` and `libext2fs`. Never flag intentional filesystem corruption in
     test inputs as a bug.

## 2. Invalid Dismissals (Real Bugs You Must NOT Dismiss)

1. **Never Assume On-Disk Fields Are Well-Formed**:
   - Never dismiss a missing bounds check, overflow check, shift-width check,
     or division-by-zero guard on an on-disk field by assuming "mke2fs only
     writes valid values" or "the superblock was checked earlier" unless you
     cite the exact dominating check in `ext2fs_open2` or the caller that
     rejects the invalid value on EVERY path reaching the code (including
     `EXT2_FLAG_SUPER_ONLY`, `EXT2_FLAG_IGNORE_CSUM_ERRORS`, `e2fsck`, and
     `debugfs` catastrophic mode!).
2. **Never Dismiss Missing `metadata_csum` Updates or Dirty Flags**:
   - If code mutates an inode, extent block, directory block, xattr block,
     bitmap, group descriptor, or superblock and writes it out (or relies on
     `ext2fs_close` / `ext2fs_flush`) without updating the corresponding
     checksum helper or dirty flag (`ext2fs_mark_super_dirty`,
     `ext2fs_mark_bb_dirty`, `ext2fs_mark_ib_dirty`), the change is either
     silently lost or corrupts `metadata_csum` filesystems.
3. **Never Dismiss `bigalloc` Cluster vs. Block Confusion**:
   - On `EXT4_FEATURE_RO_COMPAT_BIGALLOC` filesystems, block bitmaps and
     `s_free_blocks_count` (cluster-level in some internal bitmaps vs. block
     units in superblock/inode `i_blocks`) have strict unit contracts. Check
     `EXT2FS_B2C` / `EXT2FS_C2B` usage whenever block allocation, freeing, or
     bitmap iteration is modified.
4. **Never Dismiss Symmetric Tool Omissions or Shared-Library ABI Drift**:
   - When adding or modifying a filesystem feature flag, superblock field, or
     mount/tune option, check whether `lib/e2p/`, `mke2fs`, `tune2fs`,
     `dumpe2fs`, `debugfs`, `e2fsck`, and `resize2fs` require matching updates
     to avoid inconsistent behavior across tools.
   - Never dismiss appending a new error code anywhere except the very end of
     `lib/ext2fs/ext2_err.et.in`, or altering a public struct size in
     `lib/ext2fs/ext2fs.h`.
