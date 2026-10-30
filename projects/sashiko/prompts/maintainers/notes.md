+++
title = "Maintainer Notes and Code Notes Architecture Policy"
paths = [
    "projects/gcc/prompts/maintainers/",
    "projects/linux/prompts/maintainers/",
    "projects/linux/prompts/notes/",
    "projects/qemu/prompts/maintainers/",
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
- **Consistent severity terms:** When a maintainer note sets, raises, or caps
  the severity of an issue, it must use exactly the four levels Sashiko
  reports: Low, Medium, High, and Critical, as defined in the project's
  `severity.md`. Flag any other priority vocabulary used to rank an issue,
  such as "nit", "minor", "major", "blocker", "serious", "urgent", "P0"-"P3",
  or "must fix", and ask for the matching level instead (for example "report
  it as Low" rather than "it is a nit"). The reviewing model has to map every
  finding onto one of the four levels, and other terms leave that mapping to
  guesswork. Rules that only decide whether a pattern is reported at all
  ("do not report", "report it as a bug") need no severity level.
- **Every instruction must be doable by the review:** A Sashiko review sees
  only the patch and its commit message, any series or prior-version context
  included in the prompt, the prompts and notes loaded into it, and the
  reviewed git repository through read-only tools (`git_read_files`,
  `git_grep`, `git_find_files`, `git_ls`, `git_diff`, `git_show`,
  `git_log`, `git_blame`). It cannot access the internet, mailing list
  archives, bug trackers, datasheets, specifications, or any tree other than
  the reviewed one, and it cannot compile code, run tests, linters,
  checkpatch, scripts, or shell commands, or observe how a patch was sent.
  Flag any note bullet that asks the reviewer to do one of these things (for
  example "run checkpatch.pl", "check that it builds with W=1", "look up the
  earlier discussion on lore", or "verify the register layout against the
  datasheet"). Ask for a rewrite that names evidence the review can read
  instead, such as a file or document in the tree, a pattern to search for
  with `git_grep`, or history to inspect with `git_log`, or that drops the
  bullet when no such evidence exists.
- **No conflicts or duplicates:** Use `git_read_files` and `git_grep` to
  compare a new or changed bullet with the project's review prompts
  (`severity.md`, the review procedures) and with the other maintainer notes
  whose `paths` or `symbols` overlap with it. Flag a bullet that contradicts
  another note or a global rule without saying which one wins, and a bullet
  that only repeats a rule already stated elsewhere. Subsystem-local
  overrides are allowed and expected: a note may report a pattern at a higher
  or lower severity than `severity.md` would, or relax a global rule, within
  its own subsystem. Such an override is not a conflict as long as it is
  scoped to the note's subsystem and states the pattern and level it applies
  to.

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
