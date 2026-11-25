# LLVM Review Prompts

First-party prompts for Sashiko's LLVM patch review, used by the `llvm`
project profile (`--project llvm`). `build.rs` compiles this directory into
the prompt bundle under the `llvm/` namespace.

## Layout

| Path | Loaded by |
|---|---|
| `review-core.md` | Orientation: what LLVM is, which defect classes matter, and how to review compiler changes |
| `severity.md` | The verification stages |
| `false-positive-guide.md` | The verification stages |
| `inline-template.md` | The inline report stage |
| `maintainers/*.md` | Maintainer notes: review preferences and conventions of LLVM/clang maintainers, selected per patch by their `paths` and `symbols` (see `src/notes/maintainer_notes.rs`) |

These prompts carry review procedures and judgment, not volatile codebase
facts: what the code does and must preserve comes from the tree the patch is
applied to. Generated code notes (`notes/`) are not produced for LLVM yet.
