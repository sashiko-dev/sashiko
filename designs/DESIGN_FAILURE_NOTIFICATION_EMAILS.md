# Design: Opt-In Failure Notification Emails

## 1. Problem Statement

When a patchset fails to apply to every candidate baseline (`FailedToApply`)
or fails during AI review after exhausting all retries (`Failed`), Sashiko
records the status in the database and Web UI, but sends no email
notification. Authors and maintainers who rely on email workflows have no
indication that Sashiko could not review their submission (for example,
because it targeted a different tree or needed a `base-commit:` trailer).

## 2. Goals

1. Provide an opt-in per-mailing-list policy in
   `projects/<project>/mailing_lists.toml` to send a single notification
   email whenever a patchset fails to be successfully reviewed (either
   `FailedToApply` or `Failed` after all retries).
2. Default to `"none"` (strictly opt-in) so no existing mailing list
   receives unexpected emails.
3. Mirror `positive_review` semantics (`"none" | "author" | "all"`),
   including strict cross-posting isolation so lists that did not opt in
   never receive failure emails.
4. Keep failure emails concise, actionable, 72-column wrapped, and strictly
   safe: never leak raw git stderr/stdout, internal error messages, stack
   traces, secrets, or embargoed findings from other patches.

## 3. Configuration Schema (`projects/<project>/mailing_lists.toml`)

Add `failed_review` to `[defaults]` and per-list blocks, reusing the same
recipient scope enum as `positive_review`:

```toml
[defaults]
# When a patchset fails to apply or fails review after all retries, who
# receives the failure notification email:
# - "none": do not send failure emails (default)
# - "author": send only to the patchset author (To: author, no list/CCs)
# - "all": send to the full resolved recipient set of opted-in lists
failed_review = "none"

["example@vger.kernel.org"]
reply_to = ["author", "list"]
failed_review = "author"
```

### 3.1 Recipient Resolution (`EmailRouter::resolve_failure_recipients`)

- Respects `is_ignored_author`, self-address bot-loop guard, and `mute_all`.
- If effective `failed_review` is `None`, returns `None`.
- If `Author`: returns `To: [author], Cc: []` (even if a list has
  `reply_to = []` for normal findings, as long as `mute_all` is false and
  author is non-empty and not ignored/bot).
- If `All`: constructs `To:` and `Cc:` using **only** matched lists whose
  effective `failed_review == All` (plus `author` if any matched list opted
  into `failed_review == Author` with `ReplyTarget::Author`, and
  `defaults.cc`), applying the same email-aware deduplication and
  `MAX_RELAYED_RECIPIENTS` cap as `resolve_positive_recipients`.
- Forge pull requests (empty `To:`/`Cc:` and `mr_url.is_some()`) never send
  mailing-list failure emails (they already report partial failures via
  `forge_outbox`).

## 4. Trigger Points & Lifecycle

A patchset failure notification (`EmailKind::PatchsetFailure`) is queued in
`email_outbox` with `patchset_id = Some(patchset.id)` and `patch_id = None`
when a non-forge mailing-list patchset reaches a terminal unsuccessful
review state:

1. **`FailedToApply`**:
   - Triggered in `Reviewer::review_patchset_task` immediately after
     `complete_patchset_review_status_with_retry(..., "FailedToApply", None)`
     returns `Ok(true)`.
   - Never triggered if the patchset was `Cancelled` or superseded
     (`Ok(false)`).
   - Sent immediately regardless of `embargo_hours` (no findings exist).

2. **`Failed` (after all patch retries exhausted)**:
   - Triggered in `Reviewer::review_patchset_task` after `drain_review_queue`
     completes (every patch has already retried up to
     `settings.review.max_retries`) and
     `complete_patchset_review_status_with_retry(..., "Failed", ...)`
     returns `Ok(true)`.
   - Also triggered if an unrecoverable pre-review failure marks the
     patchset `Failed` via `mark_patchset_failed_unless_cancelled` (only
     when the DB transition actually updated the row and the patchset was
     not `Cancelled`).
   - Because `is_patchset_claim_active_for_outbox` validates the worker
     claim, extend its allowed status set to include `'Failed To Apply'`
     (and `'FailedToApply'`) alongside `'In Review'`, `'Reviewed'`, and
     `'Failed'`.
   - When a patchset ends in `Failed`, do **not** also queue a
     `PatchsetSummary` email.

## 5. Email Threading & Safe Content

- **Reply target**: Cover letter message (`patchset.message_id`) if present
  in `messages`, otherwise patch 1's message.
- **Headers**: Standard `Re: <subject>`, `In-Reply-To`, and `References`
  chain for the thread.

### 5.1 `FailedToApply` Body

Uses only structured attempt data: candidate baseline names and the fixed,
static `PatchApplyFailure::public_message()` strings (or a static fallback
like `"baseline ref unavailable"` for fetch/ref errors). Never interpolates
raw git stderr/stdout, error strings, or local paths.

```text
Sashiko was unable to apply this patch series cleanly to any candidate
baseline, so it could not be reviewed.

Baselines tried:
- bpf-next/master: patch 2/3 failed to apply (context does not match)
- bpf/master: patch 2/3 failed to apply (context does not match)
- linux-next/HEAD: patch 2/3 failed to apply (context does not match)
- origin/master: patch 1/3 failed to apply (target file does not exist)

Tip: sending patches with git format-patch --base=<commit> (adding a
base-commit: trailer) or targeting the expected subsystem tree helps
Sashiko select the right baseline.

-- 
Sashiko AI review · https://sashiko.dev/#/patchset/<id>
```

At most 6 candidate baseline attempts are listed inline, with `... and N
more` when additional candidates were tried. Single-patch submissions say
`"this patch"` instead of `"this patch series"`.

### 5.2 `Failed` (Review Error) Body

Never interpolates raw internal errors, stack traces, provider responses,
or embargoed findings from sibling patches:

```text
Sashiko could not complete the review of this 5-patch series after
multiple attempts.

Patches that could not be reviewed: 3 and 5.

You can check the review status at the link below.

-- 
Sashiko AI review · https://sashiko.dev/#/patchset/<id>
```

For single-patch submissions or whole-patchset failures where no specific
subset of patch indices failed, omit the per-patch line and state simply
that Sashiko could not complete the review after multiple attempts.

## 6. Validation Plan

- Unit tests in `src/email_policy.rs` and `src/email_router.rs` for
  `failed_review` parsing, defaults (`none`), `author`, `all`, cross-posted
  isolation, `mute_all`, and bot-loop guards.
- Unit tests for `FailedToApply` and `Failed` email formatting verifying
  72-column wrapping, baseline list truncation, and absence of raw git or
  internal error text.
- Reviewer integration tests:
  - `FailedToApply` with `failed_review = "none"` -> no email queued.
  - `FailedToApply` with `failed_review = "author"` / `"all"` -> 1
    `PatchsetFailure` email replying to cover letter (or patch 1) with
    classified baseline reasons.
  - `Failed` after retries exhausted -> 1 `PatchsetFailure` email naming
    failed patch indices without leaking internal errors or embargoed
    findings, and no `PatchsetSummary` email.
  - Cancelled patchset -> no failure email queued.
  - Forge PR (`mr_url.is_some()`) -> no failure email queued.
