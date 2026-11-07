# Zephyr Review Prompts

First-party review prompts for reviewing Zephyr RTOS commits, used by the
`zephyr` project profile (`--project zephyr`). The review runs locally against
a Zephyr checkout; there is no hosted Zephyr instance.

## Layout (three layers)

| Path | Loaded by |
|---|---|
| `review-core.md` | Orientation: what Zephyr is and how to review it (system prompt, every stage) |
| `false-positive-guide.md`, `severity.md` | The verification and post-verification stages |
| `report-template.md` | The report stage |
| `maintainers/*.md` | Maintainer notes: human-curated policy, scoped by `+++` front matter (`paths`, `symbols`) and selected deterministically from the patch diff |
| `notes/` | Generated code notes (source-verified codebase facts); not yet generated for Zephyr |

Maintainer notes are layer 2 of the three-layer prompt architecture: they
state what maintainers require, forbid, or deliberately suppress, never
volatile implementation facts, which belong in generated code notes. Notes
outside `general.md` load through `paths` directory prefixes or specific
`symbols`. Some prefixes are wide on purpose: `execution-context` covers
`arch/`, `drivers/`, `kernel/`, and `soc/`, since its rules apply to any code
there.

## Sources

Every section names the Zephyr document it comes from, so a reviewer can check
it. Paths are relative to the root of the Zephyr tree. When Zephyr's documentation
changes, update the rule to match: the tree under review is always authoritative
over these notes.

## Editing

Prompt content is embedded in the binary at build time and installed to
`$XDG_DATA_HOME/sashiko/prompts/<revision>/zephyr/`. After editing a file here,
rebuild and run the new binary. If using the installed `sashiko` command, repeat
the installation command in [the setup guide](../../../docs/zephyr-review.md#setup).
The new binary installs its updated bundle into a directory named by the new
content fingerprint; an existing binary continues to use its embedded version.

For prompt iteration without rebuilding, pass `--prompts` with the absolute path
to this directory when running a review from the Zephyr checkout.

## Writing a new maintainer note

Add the file under `maintainers/` with `+++` TOML front matter (`title`, plus
`paths` and/or `symbols` triggers), one rule per bullet. A rule belongs here if
it is verifiably true of the Zephyr tree, names real symbols or files, cites
where it comes from, and helps decide whether a specific change is wrong.
Generic C style advice does not belong here, and neither does anything
`scripts/ci/check_compliance.py` already enforces.
