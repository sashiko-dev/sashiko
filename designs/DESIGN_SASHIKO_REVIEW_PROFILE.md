# Design: Sashiko Review Profile

## Goal

Add a bundled prompt profile for reviewing Sashiko's Rust code. The existing
review binary can select it directly, and daemon prompt-directory selection can
use the same profile when that separate plumbing is available. The profile
should improve review of Sashiko-specific failure modes without changing the
Linux kernel profile or introducing model-backed tests.

## Current Constraint

`PromptRegistry` loads optional subsystem and stage guidance from the selected
directory, but its shared identity and stage instructions still contain Linux
kernel defaults. `review-core.md` is currently a profile-validation marker; it
is not added to the worker's shared context.

Changing every stage is outside this follow-up and overlaps the broader stage
configuration proposed by PR #188. A Sashiko profile nevertheless needs one
piece of guidance that is loaded for every review so kernel-specific examples
are not mistaken for project requirements.

## Proposed Change

Teach `PromptRegistry::build_context()` to load an optional
`project-context.md` before conditional subsystem guidance.

- Profiles without this file produce the same shared context as before.
- The Sashiko profile uses it to identify the project, establish Rust and
  service-review priorities, and mark inapplicable kernel examples as such.
- Existing stage-specific filenames remain unchanged.
- No stage configuration, remote prompt loading, template substitution, or
  custom tools are introduced.

Add `third_party/prompts/sashiko/` with:

- `review-core.md` as the validated entry point and review protocol;
- `project-context.md` as always-loaded Sashiko guidance;
- focused guidance for async execution, Git/worktree safety, webhook and
  secret boundaries, persistence/retries, and AI-provider boundaries;
- stage files for call-stack analysis, false-positive filtering, severity, and
  final inline formatting.

The profile intentionally omits `subsystem/subsystem.md`. Its small set of
guides is therefore loaded deterministically without a model-driven
preselection call.

## Compatibility

The kernel, systemd, and iproute profiles do not contain
`project-context.md`, so their generated shared context remains byte-for-byte
unchanged. CLI arguments, review stages, AI providers, tools, output protocol,
forge ingestion, databases, Git baselines, and worktree behavior are not
modified.

The new profile is bundled locally by the existing build script. It performs
no network access and does not enable itself automatically.

## Validation

Deterministic tests will prove:

- an absent optional project context leaves shared context unchanged;
- a present project context is loaded and identified in clean prompt logs;
- all required Sashiko files are embedded in the prompt bundle;
- the embedded profile can be materialized and loaded by `PromptRegistry`;
- the loaded context contains the Sashiko identity and critical security,
  async, Git, persistence, and AI-boundary guidance;
- the profile does not require a subsystem-selection model call.

No test invokes an AI provider or external service.

## Non-goals

This change does not make all hardcoded stages project-neutral, deploy a
Sashiko instance, change GitHub output formatting, support arbitrary
multi-repository review, or replace PR #188. Extracting the remaining
kernel-specific stage wording is a separate incremental refactor with its own
backward-compatibility tests.
