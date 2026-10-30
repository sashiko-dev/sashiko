+++
title = "QEMU Repository-Wide Maintainer Conventions"
paths = [
    "*",
]
+++

# QEMU Repository-Wide Conventions

Cross-cutting rules that apply to every QEMU patch, following QEMU's
contributing process (docs/devel/submitting-a-patch.rst).

## Commit format and provenance

- Every commit carries a `Signed-off-by:` trailer with the author's real name
  and email (QEMU's Developer Certificate of Origin); flag a missing one.
- Subject lines follow the "area: short summary" convention (for example
  `ui/cocoa:`, `hw/net/virtio-net:`, `target/arm:`) in imperative mood;
  flag subjects that merely restate the diff.
- A non-trivial commit body explains WHAT changed and WHY, not just HOW.
  Do not demand expanded rationale for trivial or self-explanatory commits.

## Style and tooling

- C style follows QEMU's CODING_STYLE (4-space indent, no tabs except where
  the file already uses them, no C99 declarations after statements when the
  surrounding style forbids them); `scripts/checkpatch.pl` conformance issues
  are at most Low severity.
- Do not report formatting issues the project's own tooling would not flag.

## Tests and documentation

- New guest-visible behavior is expected to come with test coverage
  (`tests/qtest/`, `tests/tcg/`, `tests/qapi-schema/`, or `tests/avocado/`)
  where QEMU conventionally has it; flag missing coverage only when a peer
  feature in the same area ships tests.
- User-facing changes (new device/machine properties, CLI options, QMP
  commands, deprecations) must update `docs/` and, where applicable,
  `qemu-options.hx` and the deprecation/removed-features documentation.

## Deprecation policy

- Removing or changing user-visible behavior (machine types, CLI options,
  QMP commands, device properties) goes through docs/about/deprecated.rst
  first; flag silent behavior removal without a deprecation window.
