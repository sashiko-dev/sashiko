# Btrfs Subsystem Details

## Extent Map Fields

Using the wrong `extent_map` field for sizing or I/O causes silent data
corruption, over-reads, or oversized allocations. These bugs hide in testing
because for simple uncompressed, non-reflinked extents, `len`, `ram_bytes`,
and `disk_num_bytes` are all equal. The fields only diverge with compression
or partial references (reflinks, bookend extents).

The `extent_map` struct (`fs/btrfs/extent_map.h`) maps file offsets to on-disk
locations. Fields correspond to the on-disk `btrfs_file_extent_item`:

- `start`: file offset (matches the key offset of `BTRFS_EXTENT_DATA_KEY`)
- `len`: number of file bytes this extent map covers (`num_bytes` on disk; for inline extents always sector size)
- `disk_bytenr`: physical byte address on disk (or sentinel `EXTENT_MAP_HOLE` / `EXTENT_MAP_INLINE`)
- `disk_num_bytes`: full size of the on-disk allocation (compressed size when compression is used)
- `offset`: offset within the decompressed extent where this file range starts (nonzero for reflinked/cloned partial references)
- `ram_bytes`: decompressed size of the entire on-disk extent (equals `disk_num_bytes` for uncompressed extents)

### Uncompressed vs Compressed Layout

```
Uncompressed:
  On-disk:    [disk_bytenr .................. disk_bytenr + disk_num_bytes]
                           |<-- offset -->|<------- len ------->|
  File:                                   [start ... start + len]
  ram_bytes == disk_num_bytes

Compressed:
  On-disk:    [disk_bytenr ... disk_bytenr + disk_num_bytes]  (smaller)
  Decompressed: [0 ................................ ram_bytes]
                    |<-- offset -->|<------- len ------->|
  File:                            [start ... start + len]
  ram_bytes > disk_num_bytes
```

### Computed Helpers

The old `block_start`, `block_len`, `orig_block_len`, and `orig_start` struct
fields have been removed. The replacement helpers:

- `btrfs_extent_map_block_start(em)` (in `extent_map.h`, public): for uncompressed returns `disk_bytenr + offset`, for compressed returns `disk_bytenr`
- `extent_map_block_len(em)` (static in `extent_map.c`, file-private): for uncompressed returns `len`, for compressed returns `disk_num_bytes`

External callers that need the block length must compute it directly: use
`disk_num_bytes` for compressed extents (`btrfs_extent_map_is_compressed(em)`)
or `len` for uncompressed extents.

### Invariants (from `validate_extent_map()` in `extent_map.c`)

For real data extents (`disk_bytenr < EXTENT_MAP_LAST_BYTE`):
- `disk_num_bytes != 0`
- `offset + len <= ram_bytes`
- Uncompressed: `offset + len <= disk_num_bytes`
- Uncompressed: `ram_bytes == disk_num_bytes`

For holes/inline (`disk_bytenr >= EXTENT_MAP_LAST_BYTE`):
- `offset == 0`

### Field Confusion Patterns

The common confusions between fields, with the damage each causes:

- `len` vs `ram_bytes`: `ram_bytes` is the full decompressed extent size,
  which may be much larger than `len` (the file range). Using `ram_bytes`
  where `len` is intended causes over-reads or oversized allocations.
- `len` vs `disk_num_bytes`: for compressed extents, `disk_num_bytes` is
  smaller than `len`. Using `len` for on-disk I/O sizing reads past the
  extent. Using `disk_num_bytes` for file-level sizing truncates data.
- `disk_bytenr` vs `btrfs_extent_map_block_start()`: raw `disk_bytenr`
  is the start of the full on-disk extent. For uncompressed partial
  references, the actual data starts at `disk_bytenr + offset`.

| Intent | Correct Field | Common Mistake |
|--------|--------------|----------------|
| File range covered by this extent | `len` | `ram_bytes` |
| On-disk bytes to read/write | `disk_num_bytes` | `len` |
| Decompressed extent size | `ram_bytes` | `disk_num_bytes` |
| Physical disk location for I/O | `btrfs_extent_map_block_start()` | raw `disk_bytenr` |

## Zoned Storage Active vs Open Zone Limits

Conflating `bdev_max_active_zones()` with `bdev_max_open_zones()` causes mount
failures on existing valid filesystems. When a synthesized zone limit (derived
from combining both values) is used for mount-time validation without an escape
hatch, filesystems created before the limit was introduced can fail to mount
with `-EIO`.

Active zones and open zones are semantically different in the ZNS/zoned block
device model:

- **Active zones** (`bdev_max_active_zones()`): zones that are implicitly open,
  explicitly open, or closed -- zones consuming an active resource on the device
- **Open zones** (`bdev_max_open_zones()`): zones currently open for write
  operations (a subset of active zones)

A device may report no active zone limit (`bdev_max_active_zones() == 0`) but
still have an open zone limit. Open zones is a more restrictive subset
constraint, not a valid proxy for active zone limits.

In `btrfs_get_dev_zone_info()` (`fs/btrfs/zoned.c`), the active zone limit is
synthesized using `min_not_zero()` on both device limits. When the resulting
count is exceeded at mount time, the code must check whether the device actually
reports a hard active zone limit before failing:

```c
// WRONG: Fails mount on existing valid filesystems
max_active_zones = min_not_zero(bdev_max_active_zones(bdev),
                                bdev_max_open_zones(bdev));
if (nactive > max_active_zones)
    return -EIO;

// CORRECT: Escape hatch when active limit was synthesized
if (nactive > max_active_zones) {
    if (bdev_max_active_zones(bdev) == 0) {
        max_active_zones = 0;  // Clear synthesized limit
        goto validate;         // Allow mount to proceed
    }
    return -EIO;  // Only fail for real device limits
}
```

Any code that synthesizes zone limits from multiple sources must provide an
escape path when the underlying device reports no hard limit for the stricter
constraint.
