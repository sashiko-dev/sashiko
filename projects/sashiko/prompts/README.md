# Sashiko Review Prompts

First-party review prompts for Sashiko reviewing its own changes, used by the
`sashiko` project profile (`--project sashiko`).

These are distinct from `third_party/prompts/`, which is a vendored upstream
tree covering the Linux kernel, systemd and iproute. Both are compiled into the
prompt bundle by `build.rs` and extracted side by side, so a directory name must
not appear in both; the build fails if one does.

## Layout

| Path | Loaded by |
|---|---|
| `review-core.md` | Orientation: what Sashiko is and how to review it |
| `subsystem/subsystem.md` | The index the pre-screen stage selects guides from |
| `subsystem/*.md` | Per-component invariants, contracts and bug patterns |
| `patterns/*.md` | Cross-cutting concerns not owned by one component |
| `false-positive-guide.md`, `severity.md` | The verification stage |
| `github-summary-template.md` | The report stage |
| `prompt-injection.md` | The security stage, and anything touching untrusted input |

A guide is selected by name. The workflow looks a selected name up in both
`subsystem/` and `patterns/`, so names must be unique across the two
directories.

## Editing

Prompt content is embedded in the binary at build time and installed to
`$XDG_DATA_HOME/sashiko/prompts/<revision>/sashiko/`. The revision includes a
fingerprint of this tree, so editing a file here produces a new revision
directory and the change takes effect on the next run. No manual reinstall is
needed.

## Writing a new guide

Add the file, then add a row to `subsystem/subsystem.md` — an unlisted guide is
never selected. The bar for content is in `review-core.md`: everything stated
must be verifiably true of this codebase, anchored to real symbol names, and
useful for deciding whether a specific change is wrong. Style and lint advice
does not belong here; `make lint` covers it.
