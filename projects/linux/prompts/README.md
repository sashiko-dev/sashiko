# Linux Review Prompts

First-party prompts for Sashiko's Linux kernel patch review, used by the
`linux` project profile. `build.rs` compiles this directory into the
prompt bundle under the `kernel/` namespace.

## Layout

| Path | Loaded by |
|---|---|
| `severity.md` | The verification stages |
| `false-positive-guide.md` | The verification stages |
| `inline-template.md` | The inline report stage |
| `callstack.md` | The execution-flow stage |
| `locking.md` | The locking stage |
| `pointer-guards.md` | Named by `callstack.md` and `false-positive-guide.md` for `read_prompt` |
| `maintainers/*.md` | Maintainer notes: the subjective preferences, policies and conventions of a subsystem's maintainers, selected per patch by their `paths` and `symbols` (see `src/notes/maintainer_notes.rs`) |
| `notes/` | Code notes: the profile, the agenda, and notes sets built from the code by `sashiko notes build` |

These prompts carry review procedures and judgment, not knowledge about the
code: what the code does and must preserve comes from the code notes, which
are generated from the source and verified against it.

## Origin

`severity.md` is Sashiko's own. `false-positive-guide.md`,
`inline-template.md`, `callstack.md`, `locking.md` and `pointer-guards.md`
descend from Chris Mason's
[review-prompts](https://github.com/masoncl/review-prompts) (commit
d048f8742f6740c38833adf02e87901412d771ca) with Sashiko's changes on top;
`LICENSE.review-prompts` is its license.
