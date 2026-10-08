# GCC Review Prompts

First-party prompts for Sashiko's GCC (GNU Compiler Collection) patch review,
used by the `gcc` project profile (`--project gcc`). `build.rs` compiles this
directory into the prompt bundle under the `gcc/` namespace.

## Layout

| Path | Loaded by |
|---|---|
| `review-core.md` | Orientation: what GCC is and how to review compiler changes |
| `severity.md` | The verification stages |
| `false-positive-guide.md` | The verification stages |
| `inline-template.md` | The inline report stage |
| `technical-patterns.md` | The execution-flow stage |
| `subsystem/subsystem.md` | The index the pre-screen stage selects guides from |
| `subsystem/*.md` | Per-component compiler invariants, IR contracts, and bug patterns |
| `maintainers/*.md` | Maintainer notes: subjective preferences, policies, and conventions of GCC maintainers, selected per patch by their `paths` and `symbols` (see `src/notes/maintainer_notes.rs`) |
| `notes/` | Code notes: the profile (`notes/profile.toml`) and notes sets built from the source tree by `sashiko notes build` |
