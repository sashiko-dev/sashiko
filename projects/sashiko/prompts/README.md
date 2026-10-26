# Sashiko Review Prompts

First-party review prompts for Sashiko reviewing its own changes, used by the
`sashiko` project profile (`--project sashiko`). `build.rs` compiles this
directory into the prompt bundle under the `sashiko/` namespace.

## Layout

| Path | Loaded by |
|---|---|
| `review-core.md` | Every stage's system prompt: orientation and review priorities |
| `false-positive-guide.md`, `severity.md` | The verification and post-verification stages |
| `github-summary-template.md` | The inline report stage |
| `prompt-injection.md` | The security stage |
| `maintainers/*.md` | Maintainer notes: subjective preferences, policies, and conventions of Sashiko's maintainers, selected deterministically per patch by their `paths` and `symbols` front matter (see `src/notes/maintainer_notes.rs`) |
| `notes/` | Code notes: the `component_map` profile, domain agenda, and source-verified notes sets built from the codebase by `sashiko --project sashiko notes build` |

These prompts separate review procedures and maintainer judgment from volatile
codebase facts: what the code does and must preserve comes from the generated
code notes (`notes/`), which are extracted from the source and verified against
it, while human policies and conventions live in `maintainers/*.md`.

## Editing

Prompt content is embedded in the binary at build time and installed to
`$XDG_DATA_HOME/sashiko/prompts/<revision>/sashiko/`. The revision includes a
fingerprint of this tree, so editing a file here produces a new revision
directory and the change takes effect on the next run. No manual reinstall is
needed.
