# Design: Consolidated Positive & Series Summary Emails

## 1. Problem Statement

When a mailing list enables `positive_review = "author"` or `"all"`, Sashiko
currently queues a separate "found no issues" email reply for every clean
patch in a series. For a 10-patch series where patches 1-6, 8, and 10 are
clean and patches 7 and 9 have findings, Sashiko sends 8 separate positive
emails plus 2 issue emails (10 replies in total). This creates excessive
mailing-list and inbox noise for authors and maintainers.

## 2. Goals

1. Minimize positive review emails for multi-patch series: send at most
   **one** consolidated series summary email replying to the cover letter
   (or patch 1 when no cover letter exists) instead of N separate positive
   emails.
2. Keep detailed inline findings emails as separate per-patch replies to
   each patch that has new findings or related pre-existing bug notes.
3. Design the consolidated email to be immediately scannable in plain-text
   kernel mail clients (72-column wrapped): compact patch ranges for clean
   patches, clear per-patch counts/severities for patches with issues, and
   explicit mention of any skipped patches.
4. Preserve single-patch behavior: a standalone 1-patch submission still
   receives a single concise positive reply directly on the patch.
5. Respect embargoes, `positive_review` recipient isolation on cross-posted
   series, `mute_all`, bot-loop guards, cancellations, reruns, and
   idempotent deduplication.

## 3. User-Facing Behavior & Email Format

### 3.1 When the Summary Email Is Sent

- **Single-patch submission (`total patches == 1`)**:
  - Clean patch -> existing per-patch positive email (1 email total).
  - Patch with findings or related pre-existing bugs -> existing per-patch
    inline review email (1 email total).
- **Multi-patch series (`total patches > 1`)**:
  - Individual clean patches do **not** queue per-patch positive emails
    (their per-patch `email_outbox` status is recorded as `"Skipped"` with
    note `"Covered by series summary email"`).
  - Individual patches with new findings or related pre-existing bugs still
    queue their detailed per-patch inline review emails as usual.
  - Once the patchset completes with status `"Reviewed"` (and after any
    embargo expires or is released early):
    - If **at least 1 patch is clean** (reviewed with 0 new findings and 0
      related pre-existing bugs, or inherited clean via
      `skip_unchanged_clean`) AND `EmailRouter::resolve_positive_recipients`
      returns `Some((to, cc))` for the series:
      - Queue **one** `EmailKind::PatchsetSummary` email replying to the
        cover letter (or patch 1 if no cover letter exists).
    - If **0 patches are clean** (every reviewed patch had issues and
      already received its own email):
      - Do **not** send an extra summary email.

### 3.2 Threading & Recipients

- **Reply target**: The cover letter message (`patchset.message_id`) if it
  exists in `messages` and is not one of the patches (or when it is the
  0/N cover letter); otherwise patch 1's message.
- **Headers**:
  - `Subject`: `Re: <target_subject>` (avoiding duplicate `Re: `).
  - `In-Reply-To`: `<target_message_id>`
  - `References`: `<target_references> <target_message_id>` (or just
    `<target_message_id>` if empty).
- **Recipients**: Resolved via `EmailRouter::resolve_positive_recipients`
  using the reply target's `To:`/`Cc:`/`From:` headers (falling back to
  `patchsets.to_recipients` / `cc_recipients` when the message row lacks
  headers). Because `resolve_positive_recipients` isolates cross-posted
  lists, lists with `positive_review = "none"` never receive the summary.

### 3.3 Plain-Text Layout (72-Column Wrapped)

1. **All patches clean (no issues, no skipped patches)**:
   ```text
   Sashiko has reviewed this 10-patch series and found no issues.

   -- 
   Sashiko AI review · https://sashiko.dev/#/patchset/<id>
   ```

2. **Mixed series (clean patches + patches with issues, optional skipped)**:
   ```text
   Sashiko has finished reviewing this 10-patch series:

   - Patches 1-6, 8, and 10: no issues found.
   - Patch 7 ("net: foo: fix bar"): 2 potential issues (1 High, 1 Medium)
   - Patch 9 ("net: foo: handle baz"): 1 potential issue (1 High)

   Detailed reviews have been sent as separate replies to patches 7 and 9.

   -- 
   Sashiko AI review · https://sashiko.dev/#/patchset/<id>
   ```
   - Compact range formatting (`format_patch_ranges`): contiguous runs of
     length >= 3 collapse to `start-end` (e.g., `1-6`), while runs of 1 or
     2 stay explicit (`8, and 10`, or `1 and 2`), producing natural English
     for every combination.
   - Patch titles strip leading `[...]` prefixes (e.g., `[PATCH v2 7/10] `)
     and are truncated cleanly with `...` if long.
   - Patches with 0 new findings and >= 1 related pre-existing bug note are
     listed with their related bug count (e.g. `1 related pre-existing bug
     noted`).
   - Non-inherited skipped patches (e.g. ignored files like `MAINTAINERS` or
     size-limit skips) get a single compact bullet (`- Patch 11: skipped
     (not reviewed).`) rather than being falsely counted as clean.
   - The closing sentence mentioning separate replies is included only when
     `EmailRouter::resolve_recipients` resolves to `Action::Send` (so we
     never claim replies were sent when findings mail was muted).

3. **All reviewed patches clean, some patches skipped**:
   ```text
   Sashiko has finished reviewing this 10-patch series:

   - Patches 1-9: no issues found.
   - Patch 10: skipped (not reviewed).

   -- 
   Sashiko AI review · https://sashiko.dev/#/patchset/<id>
   ```

## 4. Database & Outbox Changes

### 4.1 Migration `023_patchset_email_outbox.sql`

Add a nullable `patchset_id` foreign key column to `email_outbox` for
patchset-level notifications (`PatchsetSummary` and `PatchsetFailure`):

```sql
ALTER TABLE email_outbox ADD COLUMN patchset_id INTEGER REFERENCES patchsets(id);
CREATE INDEX IF NOT EXISTS idx_email_outbox_patchset_id ON email_outbox(patchset_id);
```

Applied idempotently in `Database::migrate` (checking `PRAGMA
table_info(email_outbox)` before executing the `ALTER TABLE`, matching
migration 022).

### 4.2 `EmailKind` & `EmailWorker`

Add `EmailKind::PatchsetSummary` (`"patchset_summary"`) and
`EmailKind::PatchsetFailure` (`"patchset_failure"`):
- Both return `None` from `transactional_list_label()` so they remain
  normal threaded mailing-list replies without `Auto-Submitted` / `List-Id`.
- Both receive `smtp.reply_to` when configured (via a shared
  `EmailKind::is_review_thread_reply()` helper).

### 4.3 Outbox Deduplication, Cancellation, Merge & Cleanup

- `Database::insert_patchset_email_outbox(patchset_id, kind, status, ...)`:
  - Runs inside `BEGIN IMMEDIATE` transaction.
  - Verifies active patchset claim (`is_patchset_claim_active_for_outbox`)
    and checks `patchsets.status != 'Cancelled'`.
  - Deduplicates on `(patchset_id, kind)` with `patch_id IS NULL`. Unsent
    rows (`Pending | Embargoed | Skipped | Muted | Disabled | Dry-Run |
    Cancelled`) are updated in place; `Sent | Sending | Locked` rows are
    never overwritten.
- `cancel_outbox_for_patchset` and `lock_pending_email` cancel pending
  patchset-level rows (`email_outbox.patchset_id = ?`) alongside per-patch
  rows when a patchset is cancelled.
- `cleanup_aborted_review_attempts` cleans unsent `patchset_id` outbox rows
  while preserving `Sent`/`Sending`/`Locked` rows.
- `merge_duplicate_patchset` transfers or deduplicates `patchset_id` outbox
  rows from `merge_from_id` to `target_id`, preserving `Sent`/`Sending`.

## 5. Embargo Interaction

- If a multi-patch series is embargoed (`embargo_until > now`):
  - Per-patch reviews do not queue emails during `process_patch_review`.
  - If all patches are clean, `review_patchset_task` immediately calls
    `release_patchset_results` -> `queue_patchset_notifications`, which
    queues the single `PatchsetSummary` email and clears `embargo_until`.
  - If any patch has findings, nothing is sent until the embargo expires or
    is released early; then `queue_patchset_notifications` queues the
    per-patch issue emails and the single `PatchsetSummary` email.
- If not embargoed:
  - Per-patch issue emails are queued as patches finish (and clean patches
    in a multi-patch series record `Skipped`).
  - When `review_patchset_task` finishes with `ReviewStatus::Reviewed`, it
    calls `queue_patchset_summary_email` once for the series.

## 6. Validation Plan

- Unit tests for `format_patch_ranges` and summary body composition (all
  clean, mixed with ranges + severities + related bugs, skipped patches,
  72-column line wrapping).
- DB tests for migration 023, `insert_patchset_email_outbox` deduplication
  and claim fencing, cancellation in `cancel_outbox_for_patchset` and
  `lock_pending_email`, and duplicate patchset merge.
- Reviewer integration tests:
  - Multi-patch all-clean series -> 1 cover-letter summary email, 0
    per-patch positive emails.
  - Multi-patch mixed series (e.g. clean + findings + inherited clean +
    skipped) -> 1 cover-letter summary email + per-patch emails ONLY for
    patches with issues.
  - Multi-patch all-issues series -> per-patch emails only, NO summary
    email.
  - Single-patch clean series -> preserves single per-patch positive email.
  - Embargoed multi-patch mixed series -> holds all emails until release,
    then queues per-patch issue emails + 1 summary email.
