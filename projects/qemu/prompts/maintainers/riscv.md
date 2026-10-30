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
and a patch that violates one is a finding on its own terms.

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

- An official RISC-V Foundation extension in a reasonable draft state (not
  too many changes still expected) may only be supported *experimentally*:
  it must be disabled by default and marked with an `x-` prefix in the
  user-facing CPU/board property.
- The draft must be hosted by RVI, and the code must be developed against
  the latest tag. The series must point to the exact spec used for the
  extension; a series that does not cite its exact spec is a finding.
- Draft extensions must also include unit tests and a written justification
  for why QEMU should support the extension.
- QEMU supports only the *latest* version of patches submitted for a draft
  extension, and a draft extension can be removed at any time without
  following QEMU's deprecation policy.

## Custom / Vendor Extensions

- Vendor extensions must be disabled by default (they may be enabled for
  specific vendor CPUs and boards) and must follow the RISC-V toolchain
  convention prefixes and names.
- Vendor extensions must be maintained and tested by the vendor; upstream
  takes effort not to break them, but fixes are the vendor's job.
- Patches adding open-source-toolchain support for a vendor extension are
  unlikely to be accepted without a publicly available specification —
  flag undocumented vendor-extension series.
- A vendor extension must not interfere with other extensions and must not
  be obtrusive to the core RISC-V target code; flag encroachment into
  shared core code as a finding.

## RISC-V Machines / Boards

- RISC-V machines are user facing and follow QEMU's deprecation policy.
  Backwards-incompatible changes to a board (especially the `virt` board,
  which is fixed to allow developers to target a stable machine) are
  findings; soft-core boards (like OpenTitan) are allowed more flux.
- Adding new features to boards is acceptable, but the justification for
  the feature must be present.
- Boards based on physical hardware should model the hardware as closely as
  possible, and datasheet or equivalent technical information must be
  provided.
- Machines that are not maintained or used must be deprecated; submitters
  of a machine are expected to help maintain it after acceptance.
- All new boards MUST include a boot test that boots a guest (Linux if the
  board supports it); a new board without a boot test is a finding.
