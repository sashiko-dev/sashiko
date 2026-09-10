# Design: Sashiko Review Profile

## Goal

Add a bundled prompt profile for reviewing Sashiko's Rust code. The existing
local review command can select it directly, and daemon prompt-directory
selection can use the same profile when that separate plumbing is available.
The profile should improve review of Sashiko-specific failure modes without
changing the Linux kernel profile or introducing model-backed tests.

## Current Constraint

The production worker constructs the named review stages in
`kernel_workflow`. Each stage builds and renders `PromptTemplate` values, then
sends the result through `AiProvider::generate_content()`.
The shared identity and stage instructions still contain Linux kernel defaults.
`review-core.md` is retained as the marker required by the prompt-directory
validation in dependent PR #447; current standalone local selection neither
validates nor renders it.

Changing every stage is outside this follow-up and overlaps the broader stage
configuration proposed by PR #188. A Sashiko profile nevertheless needs one
piece of guidance that is loaded for every review so kernel-specific examples
are not mistaken for project requirements.

## Proposed Change

Teach the system templates used by the active workflow, including the
prescreen request, to load an optional `project-context.md` alongside the
existing conditional guidance.

- Profiles without this file produce the same model request content as before.
- The Sashiko profile uses it to identify the project, make the Sashiko and
  Rust maintainer identity take precedence over generic kernel roles, establish
  service-review priorities, and mark inapplicable kernel examples as such.
- Existing stage-specific filenames remain unchanged.
- No stage configuration, remote prompt loading, template substitution, or
  custom tools are introduced.

Add `third_party/prompts/sashiko/` with:

- `review-core.md` as the marker expected by dependent PR #447 and as a
  standalone review protocol (current main does not validate or render it);
- `project-context.md` as always-loaded Sashiko guidance;
- focused guidance for async execution, Git/worktree safety, webhook and
  secret boundaries, persistence/retries, and AI-provider boundaries;
- lifecycle guidance that distinguishes supervised long-lived work from
  bounded detached tasks, avoiding findings based only on a dropped handle;
- deterministic unit and PR checks without forbidding separately authorized,
  opt-in integration or provider evidence;
- current token-budget accounting and response-cache identity boundaries;
- a small subsystem index that lets the existing prescreen select those
  focused pattern files on its already-required request;
- stage files for call-stack analysis, false-positive filtering, severity, and
  final inline formatting.

## Compatibility

The kernel, systemd, and iproute profiles do not contain
`project-context.md`, so their generated shared context remains byte-for-byte
unchanged. CLI arguments, review stages, AI providers, tools, output protocol,
forge ingestion, databases, Git baselines, and worktree behavior are not
modified.

The new profile is bundled locally by the existing build script. It performs
no network access and does not enable itself automatically.

## Integration Notes

This profile does not duplicate adjacent fixes that are already proposed
independently:

- PR #467 keys prompt extraction on bundle content. Until that lands, an
  existing extraction created for the same bundle revision may need a forced
  reinstall to expose newly bundled profile files. PR #467 should therefore
  land before or with this profile for upgrades; fresh installations are not
  affected.
- PR #484 corrects the base directory used by the model-facing `read_prompt`
  tool. The static profile includes added here do not depend on that tool, but
  ad hoc model reads should not be described as functional without that fix.
- PR #487 documents the conventional lowercase `commit <hash>` report header.
  The current validator normalizes the header before checking it, so other
  casing is accepted; the Sashiko inline template uses lowercase consistently.
- PR #493's local response-cache plumbing is independent of profile selection.

Provider-selected dynamic guide names are inherited from the existing renderer.
That renderer joins the names to the prompt base without path-containment
validation. Hardening this pre-existing trust boundary is a separate follow-up;
this profile neither expands the renderer nor claims the boundary is sealed.

Manual stage selection already skips the prescreen stage. Consequently, a
manual Sashiko review still receives `project-context.md` and each selected
stage's static files, but it does not receive prescreen-selected pattern files.
Changing that execution behavior is a separate workflow fix, not part of this
profile-only change.

## Validation

Deterministic tests will prove:

- an absent optional project context does not add content to provider requests;
- a present project context reaches every request issued by the real
  multi-stage workflow;
- all required Sashiko files are embedded in the prompt bundle;
- the embedded profile can be materialized and loaded by the production
  `PromptTemplate` renderer;
- stage-specific Sashiko call-path and technical guidance reaches the actual
  provider request;
- prescreen-selected Sashiko pattern files reach subsequent provider requests;
- the recording fake returns through the normal structured-result path.

No test invokes a live, external, or metered AI provider or service. A
deterministic recording fake exercises the `AiProvider` interface.

## Non-goals

This change does not make all hardcoded stages project-neutral, deploy a
Sashiko instance, change GitHub output formatting, support arbitrary
multi-repository review, or replace PR #188. Extracting the remaining
kernel-specific stage wording is a separate incremental refactor with its own
backward-compatibility tests.
