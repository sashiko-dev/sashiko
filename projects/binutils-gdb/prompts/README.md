# binutils-gdb Review Prompts

First-party prompts for Sashiko's review of the binutils-gdb repository
(binutils and GDB), used by the `binutils-gdb` project profile
(`--project binutils-gdb`). `build.rs` compiles this directory into the
prompt bundle under the `binutils-gdb/` namespace.

## Layout

| Path | Loaded by |
|---|---|
| `review-core.md` | Orientation: what the GNU toolchain is, which defect classes matter |
| `severity.md` | The verification stages |
| `false-positive-guide.md` | The verification stages |
| `inline-template.md` | The report stage (plain-text sourceware review) |
| `maintainers/*.md` | Maintainer notes, selected per patch by their `paths` and `symbols` (see `src/notes/maintainer_notes.rs`) |

binutils and GDB share this repository and the `bfd/`, `opcodes/`,
`libiberty/`, `include/` and `readline/` directories, so they share one
profile. Subsystem differences live in the path-filtered maintainer notes and
in the optional object-formats and debugger-semantics analysis stages.
Generated code notes (`notes/`) are not produced yet.
