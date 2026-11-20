# QEMU Review Prompts

First-party review prompts for reviewing QEMU patches, used by the `qemu`
project profile (`--project qemu`).

These are distinct from `third_party/prompts/`, which is a vendored upstream
tree covering the Linux kernel, systemd and iproute. Both are compiled into the
prompt bundle by `build.rs` and extracted side by side, so a directory name must
not appear in both; the build fails if one does.

## Layout (three layers)

| Path | Loaded by |
|---|---|
| `review-core.md` | Orientation: what QEMU is and how to review it (system prompt, every stage) |
| `false-positive-guide.md`, `severity.md` | The verification and post-verification stages |
| `report-template.md` | The report stage |
| `maintainers/*.md` | Maintainer notes: human-curated policy, scoped by `+++` front matter (`paths`, `symbols`) and selected deterministically from the patch diff |
| `notes/` | Generated code notes (source-verified codebase facts); not yet generated for QEMU |

Maintainer notes are layer 2 of the three-layer prompt architecture: they
state what maintainers require, forbid, or deliberately suppress — never
volatile implementation facts, which belong in generated code notes. Every
note outside `general.md` must use narrow `paths` directory prefixes or
specific (non-generic) `symbols`, so unrelated patches do not load it.

## Editing

Prompt content is embedded in the binary at build time and installed to
`$XDG_DATA_HOME/sashiko/prompts/<revision>/qemu/`. The revision includes a
fingerprint of this tree, so editing a file here produces a new revision
directory and the change takes effect on the next run. No manual reinstall is
needed.

## Writing a new maintainer note

Add the file under `maintainers/` with `+++` TOML front matter (`title`,
plus `paths` and/or `symbols` triggers). The bar for content is in
`review-core.md`: everything stated must be verifiably true of the QEMU tree
under review, anchored to real symbol names, and useful for deciding whether
a specific change is wrong. Generic C style advice does not belong here.
