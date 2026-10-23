# C Language and Compiler Semantics

Invariants governing C language semantics across 32-bit and 64-bit architectures, including integer promotions, shift and division rules, pointer arithmetic scaling, memory sizing, function pointer type safety, and string formatting.

Derived by `sashiko notes agenda` from 99995 fixes reachable from 3a8660878839faadb4f1a6dd72c3179c1df56787 (81580 code bugs, 104 themes).

## INT: Integer Widths, Promotions, Shifts, and Arithmetic

- INT-1: Where must narrower or signed operands be widened to 64-bit unsigned types before shifting, multiplying, or adding to prevent overflow, sign extension, or 32-bit truncation?
- INT-2: What bounds guarantee that shift exponents are non-negative and strictly less than the operand's bit width, and that divisors are non-zero before division or modulo operations?
- INT-3: Where are explicit 64-bit division helpers required to support 32-bit targets, and where must division use ceiling rounding instead of truncation to avoid losing resources?
- INT-4: How are wrapping counters, timestamps, and sequence numbers compared using modular signed-difference arithmetic rather than direct relational operators?

## MEMC: Memory Sizing, Pointer Scaling, Types, and Formatting

- MEMC-1: How does allocation and copy math distinguish element counts from byte sizes and pointer sizes from pointee sizes, especially across flexible arrays and decayed array parameters?
- MEMC-2: Where pointer arithmetic is performed, is the pointer typed (scaling by element stride) or byte-addressed, and where are overlapping memory moves or post-reallocation pointer refreshes required?
- MEMC-3: Do all direct and indirect function calls and callbacks match exact parameter types, qualifiers, and return types without incompatible function pointer casts that violate Control Flow Integrity?
- MEMC-4: How are base-structure downcasts verified against runtime container types across all callers, and how are stack variable addresses prevented from escaping their lexical scope?
- MEMC-5: Do format specifiers match exact argument widths and signedness, and how are string buffers sized and bounded (including NUL terminators and precision specifiers for non-NUL inputs)?
- MEMC-6: How do bitmap operations respect architecture-sized word arrays and bit lengths without casting narrower integer pointers or reading past word boundaries?
