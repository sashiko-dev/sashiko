# Design: Sashiko-generated candidate fixup patches

## Motivation

Sashiko currently focuses on automated review findings: possible bugs,
regressions, security issues, or other concerns found in submitted patches. That
is useful, but review feedback often leaves the author to translate prose back
into a concrete code change.

For some classes of issues, especially small and localized ones, Sashiko can be
more useful if it produces a reviewable candidate fixup patch in addition to the
review text. A typo in documentation, a missing kerneldoc update, or a small
local cleanup should not require only a prose comment when the tool can also
show the exact patch it thinks would address the issue.

This document proposes extending Sashiko so it can generate candidate fixup
patches. The initial scope should be conservative, starting with minor fixes such
as spelling, comment, documentation, and kerneldoc updates. Over time, the same
mechanism can be expanded to more sophisticated localized fixes when validation
and policy controls are sufficient.

## Goals

- Allow Sashiko to generate reviewable candidate fixup patches.
- Start with low-risk, easy-to-review generated patches.
- Keep generated patches separate from review findings.
- Keep generated patches under human control: authors decide whether to apply,
  edit, test, or ignore them.
- Make generated patches optional and configurable.
- Avoid mailing-list noise by using conservative email defaults.
- Provide a path to more sophisticated generated fixes over time.

## Non-goals

- Do not make Sashiko an autonomous upstream patch submitter.
- Do not silently modify an author's patch series.
- Do not treat generated patches as maintainer-reviewed fixes.
- Do not require maintainers to review generated patches unless policy explicitly
  exposes them.
- Do not generate large rewrites by default.
- Do not generate subjective style churn.

## Terminology

Sashiko output should be separated into three categories:

- **Finding**: a possible correctness, security, regression, or other review
  issue. A finding says: "this may be wrong".
- **Suggestion**: optional design or code-improvement advice. A suggestion says:
  "this may be worth improving".
- **Candidate fixup patch**: a generated diff or patch that may address a
  finding or suggestion. A candidate fixup says: "here is one possible patch
  that might address it".

Candidate fixups are advisory. They are not authoritative and should not be
submitted automatically.

## User-visible behavior

When enabled, generated fixups should be displayed separately from findings and
suggestions. The UI, CLI, API, and email output must make it clear that a fixup
is generated and optional.

Example text output:

```text
Findings:
  - The new error path can return without dropping foo->lock.

Candidate fixup patches:
  - [high confidence] Add unlock on the err_free path
    Generated patch available. Review and test before applying.
```

For minor issues, the generated patch may be the primary useful artifact:

```text
Candidate fixup patches:
  - [high confidence] Fix spelling in comment in drivers/foo/bar.c
```

The author should be able to inspect, copy, download, or apply the generated
diff manually. Sashiko should not rewrite the submitted series in place.

## Staged rollout

Generated patches should be introduced in stages.

### Stage 1: trivial generated patches

The first implementation should focus on changes that are easy to review and
unlikely to alter behavior:

- spelling fixes in comments and documentation;
- comment typo fixes;
- documentation grammar fixes;
- small kerneldoc wording updates;
- obvious commit-message wording suggestions when operating in local mode.

These patches should be small and should normally touch only comments or
documentation.

### Stage 2: low-risk local generated patches

After the trivial path is working, Sashiko can support low-risk local code or
structure changes:

- small documentation additions for changed APIs;
- local cleanup suggestions;
- simple duplicated validation helper extraction;
- small mechanical fixups that are confined to one file.

### Stage 3: review-informed generated patches

Once Sashiko has confidence and validation controls, generated patches can be
based directly on verified review findings:

- missing cleanup on local error paths;
- missing unlocks on simple error paths;
- missing locking, lifetime, or ownership documentation;
- simple test additions;
- small local refactors that make a verified issue harder to repeat.

### Stage 4: design-informed candidate patches

Longer term, Sashiko may generate more sophisticated design-oriented candidate
patches:

- alternative API shape examples;
- patch split suggestions;
- helper introduction across a small local call set;
- small follow-up patches derived from a verified design concern.

Each stage should require stronger validation and stricter policy before being
enabled by default.

## Review pipeline integration

Candidate fixups should be generated after Sashiko has enough context to avoid
turning false positives into patches.

A conservative pipeline is:

```text
Patch and source context
  -> normal multi-stage review
  -> verified findings
  -> optional suggestions
  -> optional candidate fixup generation
  -> validation and filtering
  -> storage and display
```

For trivial Stage 1 fixes, Sashiko may also generate candidate patches from a
specialized typo/documentation pass. Those patches should still be subject to the
same output limits and labeling rules.

The fixup-generation stage should receive:

- the original patch or patch series;
- relevant source context;
- final verified findings, when applicable;
- optional suggestions;
- subsystem-specific review prompts and kernel rules;
- later patches in the series, so Sashiko does not generate a fix that is
  already addressed later.

The stage must not treat unverified intermediate concerns as facts.

## Generated patch output contract

Generated fixups should use structured output so they can be validated, stored,
and displayed safely.

Example JSON shape:

```json
{
  "candidate_fixups": [
    {
      "title": "Fix spelling in foo_bar() comment",
      "category": "spelling",
      "rationale": "The comment says 'recieve' instead of 'receive'.",
      "confidence": "high",
      "applies_to_finding_id": null,
      "applies_to_suggestion_id": null,
      "patch": "diff --git a/drivers/foo/bar.c b/drivers/foo/bar.c\n...",
      "files_touched": ["drivers/foo/bar.c"],
      "risk": "trivial",
      "requires_human_testing": false
    }
  ]
}
```

Suggested categories include:

- `spelling`
- `documentation`
- `kerneldoc`
- `comment`
- `test`
- `cleanup`
- `error-handling`
- `locking`
- `lifetime`
- `helper-extraction`
- `patch-organization`
- `design-example`

Suggested risk levels include:

- `trivial`
- `low`
- `medium`
- `high`

Only `trivial` and `low` risk fixups should be considered for default display.
Higher-risk fixups should require explicit configuration or local-only use.

## Validation and filtering

Generated patches should pass basic validation before being shown.

At minimum, Sashiko should verify that:

- the generated diff parses as a patch;
- the patch applies to the reviewed tree or worktree;
- the patch only touches paths allowed by policy;
- the patch size is below the configured limit;
- the generated patch does not modify unrelated files;
- the confidence and risk level meet policy requirements.

For code-changing fixups, Sashiko should prefer additional validation when
available:

- build or compile checks for the affected area;
- relevant test execution;
- static checks or linters where appropriate;
- re-running targeted review on the generated fixup.

Failure to validate a generated patch should cause the patch to be hidden or
marked unavailable. It should not fail the original review.

## Safety rules

Sashiko-generated patches should follow these rules:

- never auto-submit generated patches upstream by default;
- never silently modify an author's patch series;
- clearly label generated patches as AI-generated candidates;
- keep generated patches separate from findings;
- limit generated patch size;
- require high confidence for generated diffs;
- prefer prose suggestions when confidence is not high;
- allow maintainers and operators to disable generated patches;
- allow subsystem-specific policy;
- make authors responsible for review and testing before use.

## Storage model

Candidate fixups should be stored separately from findings.

A future schema could add a `candidate_fixups` table with fields such as:

- `id`;
- `review_id`;
- `patchset_id`;
- `patch_id`;
- `finding_id`;
- `suggestion_id`;
- `title`;
- `category`;
- `rationale`;
- `confidence`;
- `risk`;
- `patch`;
- `files_touched`;
- `validation_status`;
- `created_at`.

Linking fixups to `review_id` preserves the exact review run that generated
them. Optional links to findings or suggestions make it clear why a generated
patch exists.

## API behavior

Generated fixups should be available through explicit API fields or endpoints.
They should not be mixed into the existing findings list.

Possible endpoints:

```text
GET /api/fixups?review_id=<id>
GET /api/fixups?patchset_id=<id>
```

Existing review endpoints may include separate fields:

```json
{
  "findings": [],
  "suggestions": [],
  "candidate_fixups": []
}
```

## CLI behavior

The CLI should show generated fixups separately from findings.

Possible interface:

```bash
sashiko-cli show latest --fixups
sashiko-cli local HEAD --generate-fixups --force-local
```

Local mode is a natural first target because the author can immediately inspect
and apply generated patches without exposing them to public mailing lists.

## Web UI behavior

The web UI should render candidate fixups in a separate section.

Recommended behavior:

- label generated patches as candidate fixups;
- show category, confidence, risk, and validation status;
- display the patch in a collapsible diff view;
- provide a copy/download action;
- avoid presenting the fixup as an accepted or required change.

## Email behavior

Generated patches should not be included in outbound email by default.

A future email policy may allow generated fixups in limited cases, for example:

- no generated fixups in email;
- trivial documentation/spelling fixups only;
- high-confidence fixups only;
- links to generated fixups in the web UI instead of inline diffs.

If generated fixups are included in email, they must be clearly separated from
findings and labeled as optional AI-generated candidate patches.

## Configuration

Possible future review settings:

```toml
[review]
generate_fixups = false
fixup_mode = "trivial" # "off", "trivial", "local", "review-informed", "all"
max_fixups_per_patchset = 3
max_fixup_lines = 50
min_fixup_confidence = "high"
max_fixup_risk = "low"
```

Email policy should remain separate from generation policy. For example:

```toml
[email]
include_generated_fixups = false
generated_fixup_email_mode = "none" # "none", "trivial", "link-only"
```

The exact location of email settings should follow the existing email policy
configuration conventions.

## Failure handling

Candidate fixup generation should be best-effort. Failure to generate or
validate fixups must not fail the original review.

If fixup generation fails:

- log the error;
- store review results normally;
- mark fixups as unavailable or omitted;
- do not change the patchset status to failed solely because fixup generation
  failed.

## Implementation plan

A possible implementation sequence is:

1. Add this design document and reach agreement on scope and policy.
2. Add schema and database methods for candidate fixups.
3. Add Rust types for generated fixup records and LLM output parsing.
4. Add validation for generated patch format, size, paths, and applyability.
5. Add a trivial spelling/documentation fixup generator.
6. Expose generated fixups through the API.
7. Display generated fixups in the CLI and web UI.
8. Add local-mode support for downloading or applying generated fixups.
9. Add email-policy support, disabled by default.
10. Expand to review-informed code fixups after validation and policy are proven.

## Open questions

- Should Sashiko generate candidate fixup patches at all?
- Should the first implementation be limited to spelling, comments, and docs?
- Should generated patches initially be local-mode only?
- Should generated fixups ever appear in outbound review email?
- Should generated fixups require successful apply validation before display?
- Should code-changing fixups require build or test validation before display?
- What patch size limit is acceptable?
- Should subsystems be able to disable generated fixups entirely?
- How should generated patches be labeled to avoid confusion with human-authored
  or maintainer-reviewed patches?
- What would make generated patches useful enough for authors to adopt them?
