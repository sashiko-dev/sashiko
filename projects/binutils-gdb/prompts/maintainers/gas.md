+++
title = "GNU assembler (gas)"
paths = ["gas/"]
symbols = []
+++
# gas: maintainer notes

- Target and generic paths must agree: a change to a target's operand parsing or encoding usually needs the matching generic relocation or fixup update.
- Encoding bugs are silent wrong output: check the exact instruction bytes against the ISA, not just the mnemonic.
- Relaxation and branch sizing must not depend on iteration order or on the number of passes; a change there affects linker relaxation too.
- Error recovery after a bad operand must produce a diagnostic and leave the assembler in a defined state.
- A new instruction or pseudo-op needs a gas/testsuite test with the exact expected bytes.
