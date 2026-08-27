# Design: Daemon Prompt Directory Selection

## Goal

Allow automated daemon reviews to select a local prompt directory without
changing the existing Linux kernel default. This is the smallest plumbing
change needed before a dedicated Sashiko instance can review Sashiko pull
requests with project-specific guidance.

## Current Flow

Forge webhooks normalize a pull request into a repository URL and a
`base..head` range. `FetchAgent` fetches the commits into the configured Git
repository and emits patches. The reviewer applies those patches in a
worktree, then starts the `review` subprocess. The subprocess already accepts
`--prompts`, but the daemon does not pass it, so the subprocess installs and
uses the bundled kernel profile.

## Proposed Change

Add an optional `prompts_path` key to `[review]`. When a daemon review starts:

1. If `prompts_path` is absent, install and select the existing bundled kernel
   prompt directory.
2. If `prompts_path` is present, use that local directory exactly.
3. Validate that the selected path is a directory containing
   `review-core.md`.
4. Pass the resolved path to the existing review subprocess with `--prompts`.

An invalid explicit path returns an error. It never falls back to the kernel
profile.

## Compatibility

The new setting is optional and uses Serde's default for old configuration
files. The default profile is the same path currently selected by the review
binary. Review stages, tools, AI providers, output protocol, Git baselines,
worktrees, forge ingestion, NNTP ingestion, and webhook validation are not
changed.

Existing CLI review commands keep their current `--prompts` handling. This
change only connects daemon configuration to the already-supported review
subprocess argument.

## Validation

Deterministic tests will verify:

- configurations without `prompts_path` continue to parse;
- the absent setting resolves to the bundled kernel profile;
- an explicit local profile is preserved exactly;
- missing and malformed explicit profiles return errors;
- subprocess arguments contain the resolved `--prompts` value;
- a mock review subprocess receives the configured path without an AI call.

## Non-goals

This change does not add remote prompt downloads, configurable stages,
template variables, custom tools, arbitrary multi-repository management, or a
Sashiko-specific profile. Those can be reviewed as separate follow-up changes.

## Overlapping Work

PR #188 proposes a broader customization framework including remote prompt
sources, stage configuration, templates, and custom shell tools. This design
does not duplicate that framework. It adds only the local daemon-to-worker
prompt selection seam needed for incremental project support.

## Risks

The new validation treats `review-core.md` as the profile entry-point marker,
matching all currently bundled project profiles. Validation runs before the
review subprocess is spawned, so configuration errors fail the affected
review clearly without a panic or model request.
