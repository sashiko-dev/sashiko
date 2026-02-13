# DAX Subsystem Details

## Subsystem-Specific Rules

### DAX Mapping Requirements
- CPU addressable persistent memory
- Page faults handled differently than page cache
- No struct page for some DAX mappings
- Cache flushing required for persistence

### Synchronization
- DAX entries in radix tree need locking
- Special handling for 2MB/1GB huge pages
- Coordination with filesystem for truncate/punch

### Persistence Guarantees
- CPU cache flushes for durability
- Memory barriers for ordering
- Power-fail atomicity considerations

## Quick Checks
- Proper flushing after CPU writes
- DAX entry locking for concurrent access
- Huge page splitting/collapsing handled correctly
- Filesystem metadata consistency with DAX operations
