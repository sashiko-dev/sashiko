+++
title = "simulators (sim)"
paths = ["sim/"]
symbols = []
+++
# sim: maintainer notes

- Simulator instruction semantics must match the ISA, including overflow, NaN and endianness behavior; a wrong flag is a silent wrong result.
- The simulator's ABI and memory map are shared with GDB's target description; check both sides when registers or memory layout change.
- A new instruction or syscall needs a sim/testsuite test with the expected state after execution.
- Do not weaken accuracy checks to make a test pass; report the mismatch.
