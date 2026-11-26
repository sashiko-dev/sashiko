+++
title = "GNU linker (ld)"
paths = ["ld/"]
symbols = []
+++
# ld: maintainer notes

- Linker script evaluation, section layout and garbage collection are order-sensitive: a change must be checked against the script language and the target's default linker script.
- Relocation overflow, relax and stub insertion must be verified for every affected target; a fix in the generic path that ignores a target hook is incomplete.
- Output must be deterministic and must not depend on symbol-table hash order.
- A new option needs documentation and a ld/testsuite test that checks the resulting sections and symbols.
- Never paper over a script error by ignoring the failed assertion; the linker must report the problem.
