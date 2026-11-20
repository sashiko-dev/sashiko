# C Language and Compiler Semantics

Invariants governing C language semantics across 32-bit and 64-bit host
architectures and big-endian versus little-endian targets.

## INT: Integer Widths, Promotions, Shifts, and Arithmetic

- INT-1: Where must 32-bit or signed counts, block numbers, and byte offsets be widened to 64-bit unsigned types before shifting, multiplying, or adding to prevent overflow, sign extension, or 16 TiB truncation?
- INT-2: What bounds guarantee that shift exponents derived from persistent headers are non-negative and strictly less than the promoted operand width, and that divisors are non-zero before division or modulo?
- INT-3: Where must block/cluster/group calculations use overflow-safe ceiling division rather than `(a + b - 1) / b` or truncating division?

## MEMC: Memory Sizing, Endianness, Alignment, and Formatting

- MEMC-1: How does allocation and copy sizing distinguish element counts from byte sizes and guard `count * size` multiplications against integer wraparound?
- MEMC-2: Which persistent fields are stored in little-endian byte order on disk, which layer converts them to and from host byte order, and where would missing or duplicate byte-swapping corrupt big-endian hosts?
- MEMC-3: How are variable-length trailing records, extended headers, and unaligned fields accessed without reading past buffer ends or violating strict alignment?
- MEMC-4: Do printf/diagnostic format specifiers match exact argument widths across 32-bit and 64-bit platforms, and how are non-NUL-terminated on-disk strings bounded when copied or printed?
