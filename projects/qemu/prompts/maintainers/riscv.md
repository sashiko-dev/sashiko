+++
title = "RISC-V Upstreaming Requirements"
paths = [
    "target/riscv/",
    "hw/riscv/",
]
symbols = [
    "riscv_cpu_*",
    "riscv_extension*",
    "csr_ops",
]
+++

# RISC-V Subsystem

Source: the QEMU wiki page `Documentation/Platforms/RISCV`. These are
QEMU RISC-V's published upstreaming requirements — they are project rules,
and a patch that violates one is a finding on its own terms. Severity comes
from `severity.md`; this note only names `High` explicitly where the rule
itself says so.

## Foundation Extensions (frozen or ratified)

- Only extensions frozen or ratified by the RISC-V Foundation may be
  upstreamed. Support for a ratified extension must not be silently removed;
  flag removal or disabling of ratified extension support as High severity.
- Extension support must show a clear user: links to patches submitted to
  upstream compilers, toolchains or kernels (for example posted GCC patches).
  A series adding an extension without evidence the feature is tested and
  will be used is a finding.
- All new extensions MUST include unit tests covering the new instructions
  and/or CSR accesses, most likely by adding test cases to `tests/tcg/`.
  A new extension series without `tests/tcg/` coverage is a finding.

## Draft (non-ratified) Foundation Extensions

- An official RISC-V Foundation extension in a draft state may only be
  supported *experimentally*: it must be disabled by default and marked
  with an `x-` prefix in the user-facing CPU/board property. A draft
  extension exposed by default presents an unstable interface as stable;
  flag it as High severity.
- The draft must be hosted by RVI, and the code must be developed against
  the latest tag. The series must cite, in the commit message or `docs/`,
  the RVI-hosted specification and the exact tag the code was developed
  against; a series that does not name both is a finding. Whether the cited
  tag is still the latest cannot be verified from the repository — only flag
  staleness when the commit itself shows it.
- Draft extensions must also include unit tests and a written justification
  for why QEMU should support the extension.

## Custom / Vendor Extensions

- Vendor extensions must be disabled by default (they may be enabled for
  specific vendor CPUs and boards) and must follow the RISC-V toolchain
  convention prefixes and names. A vendor extension enabled by default
  exposes an unstable vendor interface to users; flag it as High severity.
- A publicly available specification must exist, cited in the commit message
  or `docs/`; a vendor-extension series that cites no specification is a
  finding.
- A vendor extension must not interfere with other extensions and must not
  be obtrusive to the core RISC-V target code; flag encroachment into
  shared core code as a finding.

## RISC-V Machines / Boards

- RISC-V machines are user facing and follow QEMU's deprecation policy.
  Backwards-incompatible changes to a board (especially the `virt` board,
  which is fixed to allow developers to target a stable machine) break the
  machine contract guests and tooling rely on; flag them as High severity.
  Soft-core boards (like OpenTitan) are allowed more flux.
- Adding new features to boards is acceptable, but the justification for
  the feature must be present. Adding docs doesn't need a justification.
- Boards based on physical hardware should model the hardware as closely as
  possible, and a datasheet or equivalent technical reference must be cited
  in the commit message or `docs/`.
- All new boards MUST include a boot test that boots a guest (Linux if the
  board supports it); a new board without a boot test is a finding.
