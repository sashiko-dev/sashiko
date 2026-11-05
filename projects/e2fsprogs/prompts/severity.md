# Severity Levels

Calibrate every validated `e2fsprogs` finding to one of four severity levels
(`Critical`, `High`, `Medium`, `Low`) based on its impact on user filesystems,
shared-library consumers, and host memory safety.

> Threat-model note: `e2fsprogs` utilities and `libext2fs` routinely parse
> damaged or untrusted filesystem images (for example in `e2fsck`, `fuse2fs` /
> `fuse4fs`, container/VM image tooling, and fuzzers). Memory corruption or
> out-of-bounds writes triggered by a crafted image are `Critical`, whereas a
> clean crash, assertion failure, or bounded hang on a corrupted image without
> memory corruption or data loss is `Medium` (or `High` when it crosses an
> active `fuse2fs`/`fuse4fs` mount privilege boundary).

## Critical

Bugs that cause **silent or unrecoverable on-disk filesystem corruption on valid
filesystems**, **destructive `e2fsck`/`resize2fs`/`tune2fs` mis-repair that
loses user data**, or **memory corruption / arbitrary code execution**:
- **On-Disk Metadata or Data Corruption**: Writing wrong block or cluster
  numbers, corrupting extent trees, directory blocks, xattr blocks, bitmaps,
  superblocks, group descriptors, `orphan_file`, or JBD2 journals, or leaving
  broken `metadata_csum` checksums so Linux `ext4` refuses to mount or panics.
- **Destructive Automatic Repair**: `e2fsck` (especially `PR_PREEN_OK` / `-p`)
  or `resize2fs` truncating, unlinking, or overwriting valid user files or
  allocated blocks due to faulty validation or block accounting.
- **Memory Corruption / Out-of-Bounds Read-Write**: Heap/stack buffer overflow,
  use-after-free, double-free, or wild pointer dereference reachable from CLI
  inputs or a crafted filesystem image (for example trusting unvalidated on-disk
  `rec_len`, `eh_entries`, `e_value_offs`, `e_value_size`, or `s_desc_size`).

## High

Bugs that **break shared-library ABI compatibility**, **violate ext4 kernel
interoperability**, **truncate 64-bit block numbers**, **break big-endian
byte-swapping**, **crash on valid filesystems**, or **violate `fuse2fs` /
`fuse4fs` security boundaries**:
- **Shared-Library ABI / API Breakage**: Changing the signature, return
  convention, or public struct layout of exported interfaces in `libext2fs`,
  `libe2p`, `libcom_err`, or `libss`, or inserting error codes in the middle of
  `ext2_err.et.in` instead of appending at the end.
- **64-Bit Truncation & `bigalloc` Unit Bugs**: Truncating `blk64_t`,
  `e2_blkcnt_t`, or `ext2_off64_t` values to 32 bits (breaking >16 TiB
  filesystems), or mixing block counts and cluster counts (`EXT2FS_B2C` /
  `EXT2FS_C2B`) on `bigalloc` filesystems.
- **Endianness Bugs (`EXT2FS_BE`)**: Missing or double byte-swapping
  (`ext2fs_le*_to_cpu` / `ext2fs_cpu_to_le*`) on raw on-disk fields.
- **Lost Dirty Tracking / Undo / Mounted Safety**: Modifying superblocks, group
  descriptors, or bitmaps without marking them dirty before flush/close,
  bypassing MMP (`EXT4_FEATURE_INCOMPAT_MMP`) or mounted-filesystem checks on
  destructive operations, or corrupting `e2undo` rollback state.
- **Crash or Failure on Valid Filesystems & `fuse2fs`/`fuse4fs` Security**:
  Null dereference, assertion failure, uninitialized variable read, or logic
  error triggered during normal operation on a valid filesystem, data races in
  multi-threaded `fuse2fs`/`fuse4fs`, or permission/uid/gid/suid bypasses in
  FUSE mounts.
- **Missing or Broken `Signed-off-by` Trailer**: Missing `Signed-off-by:` DCO
  sign-off, missing or placeholder email address, bot identity, or
  handle/username instead of a real human name.

## Medium

Bugs that **crash or hang on corrupted/fuzzed filesystem images without memory
corruption**, **leak memory or handles**, **mishandle I/O errors**, **unsort
`e2fsck` problem tables**, or **break cross-platform builds**:
- **Crash / DoS on Corrupted Images**: Division by zero (for example unvalidated
  `s_blocks_per_group == 0` or `s_inodes_per_group == 0`), shift undefined
  behavior on corrupt `s_log_block_size` / `s_log_cluster_size`, null
  dereference, or infinite loop on cyclic extent/HTree/xattr/journal structures
  when inspecting a damaged image.
- **Resource & Memory Leaks**: Leaking `ext2fs_get_mem` allocations,
  `ext2_inode_scan`, `ext2_extent_handle_t`, `io_channel`, or file descriptors
  on error or early-return paths.
- **Ignored `errcode_t` / Partial I/O**: Dropping `io_channel` or `libext2fs`
  error returns so an operation reports exit code `0` despite failed writes.
- **Build, Test-Harness, or Portability Breakage**: Misordered `problem_table`
  entries in `e2fsck/problem.c` (breaking `tst_problem`), missing
  `#include "config.h"` first in `.c` files, VLAs, `#ifdef` unused-variable
  warnings under `-Wall -Werror`, non-portable headers breaking BSD/macOS/MinGW
  builds, or `exit` instead of `return` in sourced `tests/*/script` files.

## Low

Isolated defects with no data loss, memory safety impact, ABI break, crash, or
build failure:
- Wrong `@`-expansion argument populated in `e2fsck` `problem_context` when only
  the diagnostic message text (not the repair decision) is affected.
- Calling `printf`/`fprintf(stderr, ...)` directly inside `libext2fs`, `libe2p`,
  `libcom_err`, or `libss` library routines instead of returning `errcode_t`.
- Concrete `Fixes:` trailer SHA/subject mismatches or standalone commit-message
  hygiene issues on submitted commits.
