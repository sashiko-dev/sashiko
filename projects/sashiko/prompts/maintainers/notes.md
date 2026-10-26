+++
title = "Maintainer Notes and Code Notes Architecture Policy"
paths = [
    "projects/linux/prompts/maintainers/",
    "projects/linux/prompts/notes/",
    "projects/sashiko/prompts/maintainers/",
    "projects/sashiko/prompts/notes/",
    "src/notes/",
    "src/workflows/code_notes.rs",
    "src/workflows/notes_agenda.rs",
    "src/workflows/notes_corpus.rs",
]
symbols = [
    "MaintainerNote",
    "NotesProfile",
    "select_code_notes",
    "select_maintainer_notes",
]
+++

## Maintainer Notes (`projects/*/prompts/maintainers/*.md`)

- Every maintainer note file must begin with a valid `+++` TOML front matter
  block containing only `title`, `paths`, and `symbols` (`deny_unknown_fields`).
- **Scope discipline:** Only `general.md` may use `paths = ["*"]`. Every other
  maintainer note must scope `paths` to specific subsystem directory prefixes
  (ending with `/`), exact file paths, or narrow file globs, and must keep
  `symbols` to specific subsystem types or functions (never short or generic
  identifiers such as `"err"`, `"db"`, `"lock*"`, or `"get_*"` that match
  unrelated diffs).
- **Human policy only, no volatile code facts:** Maintainer notes record
  subjective maintainer preferences, architectural policies, coding conventions,
  and narrowly-scoped false-positive suppression rules. Flag any maintainer note
  addition that restates volatile codebase implementation facts (such as struct
  fields, helper call chains, or lock internals) that belong in generated code
  notes (`notes/<set>/units/*.md`).
- **Narrow suppression preconditions:** Any rule instructing reviewers not to
  report a pattern must state the exact preconditions under which the pattern is
  safe so real bugs are not blanket-suppressed.

## Code Notes (`projects/*/prompts/notes/` and `src/notes/`)

- Agenda modules (`notes/agenda/**/*.md`) must use valid `## ID: Title` topic
  headings and `- ID-N: question` bullets with globally unique uppercase IDs,
  and must remain version-agnostic (no hard-coded function names, file paths, or
  release versions).
- Generated unit notes (`notes/<set>/units/*.md`) and `manifest.json` are
  checked into git, whereas `.facts.json` audit sidecars and temporary build
  artifacts must never be committed.
- System prompts that inject `{{code_notes}}` must explicitly frame
  `<code_notes>` as background reference from a fixed snapshot that is
  subordinate to the repository source code at `Target Commit SHA` and
  `Baseline SHA`.
- Any content extracted from repository source or commit history into code notes
  must pass through `neutralize_directives` before being saved or rendered so
  literal `@include(...)`, `@includes`, `{{...}}`, or closing XML tags cannot
  escape the `<code_notes>` block.
