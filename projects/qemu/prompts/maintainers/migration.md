+++
title = "Migration and vmstate Compatibility Policy"
paths = [
    "migration/",
]
symbols = [
    "VMStateDescription",
    "vmstate_*",
    "vmstate_register*",
    "register_savevm*",
    "post_load",
    "pre_save",
    "pre_load",
    "version_id",
    "minimum_version_id",
]
+++

# Migration & vmstate

Device state crosses machine boundaries through `VMStateDescription`. Migration
bugs are state-corruption bugs: the stream either has too few, too many, or
mismatched fields, and the guest wakes up in a state no hardware would produce.

## Versioning discipline

- Adding, removing, or reordering a migrated field is a change to the wire
  format. It MUST be covered by a `version_id` bump or a
  `VMSTATE_*_BUFFER`/optional-subsection path, and the bump must name which
  machine types are allowed to change format.
- `minimum_version_id` changes that drop support for an old stream, or new
  fields migrated unconditionally when the destination may be an old version,
  are compat breaks.

## Subsection guards

- New optional state belongs in a subsection with an explicit `.needed` guard;
  a guard that always returns true, or a field left in the top-level
  description so old machines must read fields they do not have, is a finding.
- `.vmsd` wiring: a new device that sets `dc->vmsd` must cover every
  guest-visible field the realize path initializes; state that is guest
  observable but unmigrated is a finding on the migration stage's own terms.

## Cross-checks

- Post-load fixups: a field loaded from the stream that reaches cached
  derived state (`*_e`, compute-now caches) must be recomputed in the
  post_load handler, not left at its default.
- Device state shared with an accelerator (KVM/ HVF registers) must round-trip
  through both the migration stream and the accelerator sync path.
