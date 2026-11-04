# Skipping Unchanged Patches That Already Reviewed Clean

## 1. Problem Statement

Large series are reposted many times. A 30-patch series at v7 has
typically changed in two or three patches since v6; the other
twenty-odd patches carry the same diff, often the same commit message,
and sometimes only a rebased `index` line. Sashiko reviews every patch
of every posting from scratch. When the prior review of an unchanged
patch produced no findings, the new review almost always produces no
findings again, at full token cost.

Today the daemon has no way to avoid that spend:

- A patch is deduplicated only by `message_id`
  (`ON CONFLICT(message_id) DO NOTHING` in `src/db.rs`). A repost is
  a new message, so it is a new patch row and a new review.
- `prior_context.rs` already detects the unchanged case and tells the
  model `(patch diff unchanged)` alongside the prior findings. That
  improves consistency but does not shorten the review: every stage
  still runs with the full series as context.
- The `Skipped` patch status exists, but it is only set for size
  limits (`review.max_lines_changed`, `review.max_files_touched`) and
  for the benchmark runner's `skip_patch_ids` override.

The motivating workload is the kernel mailing lists, where repeated
postings of sparse-change series are routine and the token budget is
finite.

## 2. Goals

- Skip the LLM review of a patch whose normalized diff is identical to
  a patch Sashiko already reviewed to completion with zero findings.
- Keep the skipped patch visible in the series: it must still be
  present in the `patches_json` input of the patches that are
  reviewed, so cross-patch context is unchanged.
- Record why the patch was skipped and which prior review it inherits
  from, so the dashboard and the outbox can explain the outcome.
- Default off. Operators opt in per deployment.

## 3. Non-Goals

- Reusing a prior review that had findings. A finding needs a fresh
  verification pass against the current baseline and the author's
  response; that is the existing prior-version feedback flow in
  `prior_feedback.rs` and is unaffected here.
- Skipping based on commit message changes alone. Only the diff is
  compared. A reworded commit message on an identical diff still
  skips (the commit message is not where bugs live), and that is the
  intended trade-off.
- Cross-project matching. A daemon database serves one project (the
  `project` key in the `meta` table), so the hash never meets a patch
  from another project.
- Any change to the local `sashiko review` command. It has no
  database and reviews whatever commits it is given.

## 4. Anti-Goals

- Silently dropping a patch from the series without a record. Every
  skip must leave a `reviews` row and an outbox entry a maintainer can
  find.
- Skipping when the prior review did not finish (`Failed`,
  `Cancelled`) or when it reported pre-existing findings only. "No
  findings" means a `Reviewed` review with zero `findings` rows of any
  kind.

## 5. Design

### 5.1 Normalized diff hash

Add a `diff_hash TEXT` column to `patches` with a partial index on
`(diff_hash) WHERE diff_hash IS NOT NULL`, via a new migration under
`src/migrations/`. The hash is SHA-256 over the output of the existing
`normalize_patch_diff_lines` in `src/prior_context.rs`, which already
drops the pre-`diff --git` header and every `index` line so a rebase
does not change the result. That function becomes `pub(crate)` and
gains a sibling `patch_diff_hash(diff: &str) -> String` next to it.

The hash is computed when the patch row is created (ingestor, forge,
Patchwork, and backfill paths all funnel into `create_patchset`), and
backfilled lazily: the review worker computes and stores it for any
patch it loads with a NULL hash. No bulk backfill migration is run;
old rows become hashable the first time they are touched.

### 5.2 Lookup

Before building `valid_jobs` in `src/reviewer.rs` (the loop that today
applies the size limits), the worker asks the database for a reusable
prior review:

```sql
SELECT r.id, r.patchset_id, p.id
FROM patches p
JOIN reviews r ON r.patch_id = p.id
WHERE p.diff_hash = ?1
  AND p.id != ?2
  AND r.status = 'Reviewed'
  AND r.created_at >= ?3
  AND NOT EXISTS (SELECT 1 FROM findings f WHERE f.review_id = r.id)
ORDER BY r.created_at DESC
LIMIT 1
```

`?3` is `now - max_age`.

Two candidate sources are deliberately treated the same way: a patch
linked through `patches.previous_patch_id` (same series, earlier
version) and an unrelated patch anywhere in the project with the same
hash (a patch carried across two different series, or a resend under a
new thread). The hash is the identity; lineage is only used to pick
the label shown to humans.

### 5.3 Skip conditions

All of the following must hold:

1. `review.skip_unchanged_clean = true`.
2. A matching prior review exists per 5.2.
3. The prior review's `model` and `prompts_hash` match the current
   worker's, unless `review.skip_unchanged_clean_any_model = true`.
   The default is strict: a prompt or model change is exactly when a
   re-review of an old clean patch is worth paying for.
4. If `review.skip_unchanged_clean_require_same_baseline = true`, the
   current baseline is the same commit as the prior review's baseline
   (`reviews.baseline_id` resolves to the same SHA). Off by default.

Condition 4 is relaxed by default on purpose. A series reposted a week
later lands on a newer `-next` tip, so the baseline nearly always
moves, and a strict check would save nothing on the motivating
workload. The same diff on a different tree can be a different patch,
which is why the check exists at all; operators reviewing against a
fast-moving tree where that matters can turn it on. The age bound in
5.6 limits how far the baseline can drift under the default.

### 5.4 What happens on a skip

- The patch gets status `Skipped`, as size-limited patches do today.
- A `reviews` row is inserted with `status = 'Reviewed'`, zero
  findings, `summary` set to a fixed string naming the inherited review
  (`Unchanged since <label>; prior review <id> found no issues`), and a
  new nullable column `reviews.inherited_from_review_id` pointing at
  the prior review. `model`, `prompts_hash`, and `provider` are copied
  from the prior review so the row is honest about which run produced
  the verdict.
- No AI interaction is created and no tokens are counted.
- The email outbox receives the existing `Skipped due to no findings`
  entry, so delivery behaviour is identical to a fresh clean review.
- Forge posting: the patch contributes nothing to the PR comment, same
  as a fresh clean review.
- The patch stays in `patches_json` for the other jobs with its
  `prior_version_context` intact.

The `<label>` is the version label from `PriorVersionContext` when the
match came through lineage (`v6 3/30`), else the prior patchset's
subject.

### 5.5 Whole-series skip

If every patch in a patchset is skipped, the patchset still completes
as `Reviewed` through the normal path (zero valid jobs is already a
handled state for size-limited series). No special case.

### 5.6 Settings

```toml
[review]
# Skip the LLM review of a patch whose diff is byte-identical, modulo
# index lines, to a patch already reviewed with no findings.
skip_unchanged_clean = false
# Only reuse a review produced within this many days.
skip_unchanged_clean_max_age_days = 90
# Reuse a clean review even when the model or prompts have changed.
skip_unchanged_clean_any_model = false
# Reuse a clean review only when the baseline commit is the same.
skip_unchanged_clean_require_same_baseline = false
```

These live on `ReviewSettings` in `src/settings.rs` and are documented
in `docs/configuration.md`. `sashiko review` ignores all four.

### 5.7 Observability

- `info!` on every skip, naming patchset, index, the inherited review
  id, and which of the relaxed flags were required for the match.
- A `reviews_skipped_unchanged_total` counter in `src/metrics.rs`.
- The dashboard patch view shows the inherited review link where it
  would otherwise show findings. The web UI change is a single
  conditional in `static/index.html` keyed on
  `inherited_from_review_id`.

## 6. Risks

**A clean review was wrong, and the skip propagates the miss.** A
false negative in v3 now also suppresses v4 through v9. Mitigations:
the `max_age` bound, the strict model/prompt default so a prompt fix
re-reviews everything, and `sashiko-cli rerun` (which already exists) to
force a fresh run on a patchset. This is the accepted cost of the
feature and the reason it is off by default.

**Baseline drift changes the meaning of an identical diff.** Accepted
by default: the age bound caps the drift, and condition 4 offers a
strict mode for trees where it is not acceptable.

**Hash collisions.** SHA-256 collisions within one database are not a
practical concern.

**Lazy backfill produces no skips for the first repost after
deployment.** Correct and acceptable: the first posting after the
feature ships is hashed when reviewed, and the next repost skips.

## 7. Alternatives Considered

**Reuse reviews with findings too, by replaying the prior findings.**
Rejected. Prior findings need re-verification against the author's
changelog and the discussion thread; that is what `prior_feedback.rs`
does and it needs the LLM.

**Match on `previous_patch_id` lineage only.** Simpler, but misses the
common case of a patch moved between series or resent under a new
thread, and lineage reconciliation is itself heuristic. The hash is
cheaper and more reliable; lineage becomes a labeling aid.

**Compare the full patch including the commit message.** Would skip far
less often (authors touch commit messages on nearly every repost) for
no safety gain, since the review targets the code.

**Rely on the AI response cache.** The cache keys on the full request,
which includes the whole series as context, so a one-patch change in
the series invalidates every entry. It also still runs the workflow
engine and counts cached tokens.

## 8. Implementation Steps

Each step is one commit.

1. Migration: `patches.diff_hash` with partial index,
   `reviews.inherited_from_review_id`.
2. `patch_diff_hash` helper in `prior_context.rs` with tests covering
   the rebase-only case and a one-line change.
3. Store the hash on patch creation; lazy backfill in the worker.
4. `ReviewSettings` fields, defaults, `Settings.toml` comments,
   `docs/configuration.md`.
5. `Database::find_reusable_clean_review` with the query from 5.2 and
   a test exercising each predicate.
6. Reviewer: the skip branch, the inherited review row, outbox entry,
   metrics, logging. Tests: skip taken, skip refused on each
   condition, series with all patches skipped.
7. Dashboard conditional.

## 9. Open Decisions For The Operator

These do not block implementation; the defaults above are the proposed
answers.

- Whether 90 days is the right default age bound. With the baseline
  check relaxed by default, this bound is the only limit on how stale
  a reused clean verdict can be.
