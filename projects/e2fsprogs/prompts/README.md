# e2fsprogs Review Prompts (`--project e2fsprogs`)

First-party prompts and code-notes configuration for reviewing `e2fsprogs`
patches (submitted to `linux-ext4@vger.kernel.org` or opened as pull requests
against `tytso/e2fsprogs`), bundled under the `e2fsprogs/` namespace by
`build.rs`.

## Layout

- `review-core.md` — Core `e2fsprogs` review methodology, untrusted on-disk
  image discipline, shared-library ABI awareness, and repository inspection
  rules.
- `severity.md` — 4-tier severity calibration (`Critical`, `High`, `Medium`,
  `Low`) for `e2fsprogs` utilities and shared libraries.
- `false-positive-guide.md` — Patterns that are intentional in `e2fsprogs` vs.
  real defects that must never be dismissed.
- `inline-template.md` — Plain-text LKML-style inline review format used for
  mailing-list replies and GitHub PR review comments.
- `maintainers/` — Human-authored maintainer policy and subsystem conventions
  selected dynamically by touched file paths and diff identifiers.
- `notes/profile.toml` & `notes/agenda/` — Ownership-unit mapping and
  version-agnostic research agenda for `sashiko --project e2fsprogs notes`.
