# Email Routing, Policy, and Delivery Invariants

This guide covers `src/email_policy.rs`, `src/email_router.rs`,
`src/worker/email.rs`, `src/patchwork.rs`, `src/worker/patchwork.rs`, and
`email_policy.toml`.

This subsystem sends automated emails to public mailing lists (such as LKML) and
posts status checks to Patchwork. A defect here can spam thousands of kernel
developers or leak embargoed findings. Every change to recipient routing or
outbox state transitions requires maximum scrutiny.

## 1. The Outbox State Machine & `dry_run` Safety

When a review finishes, `Reviewer::queue_notifications` resolves recipients and
writes rows to `email_outbox` and `patchwork_outbox`.
- **Outbox Statuses**:
  - `Disabled`: SMTP is not configured (`settings.smtp.is_none()`).
  - `Dry-Run`: `smtp.dry_run = true` (default safe state). The row is recorded
    in full in `email_outbox` for inspection, but `EmailWorker` must *never*
    transmit it over SMTP.
  - `Muted`: Policy (`EmailAction::Mute`) or ignored author rules suppressed the
    notification.
  - `Skipped`: 0 findings and positive reviews (`send_positive_review`) are not
    enabled for the subsystem.
  - `Pending`: Queued for actual delivery by `EmailWorker::run`.
  - `Sent` / `Failed`: Terminal states after delivery attempts.
- **Invariant**: Any change to `EmailWorker::run` (`src/worker/email.rs`) or
  `lock_pending_email` (`src/db.rs`) must preserve the strict guarantee that
  only `Pending` rows are selected for transmission, and that `smtp.dry_run`
  acts as a second, independent kill-switch inside the worker loop.

## 2. Loop Prevention and Ignored Authors

Automated bots replying to each other on public lists cause catastrophic mail
loops (`designs/DESIGN_LOOP_PREVENTION.md`).
- **Ignored Authors (`EmailRouter::is_ignored_author`)**: Suppresses reviews for
  patches authored or sent by Sashiko's own `sender_address`, known bots (e.g.,
  kernel test robot / lkp, syzbot), or configured blocklisted addresses.
- **Defect to Catch**: Any modification to `resolve_recipients` that bypasses
  `is_ignored_author` or adds the bot's own address to `To`/`Cc` output lists.

## 3. Embargo Enforcement (`subject_embargo.md`)

Security-sensitive or embargoed patchsets must not have their findings mailed
out or posted to public Patchwork instances until the embargo is explicitly
released (`release_embargoed_results`).
- **Invariant**: In `Reviewer::complete_review`, if a patchset is embargoed,
  `queue_notifications` must be skipped completely until embargo release.

## 4. Patchwork Check Delivery & Secret Resolution

`PatchworkWorker` (`src/worker/patchwork.rs`) polls `patchwork_outbox` and posts
check statuses via HTTP (`patchwork::post_patchwork_check`).
- **Runtime Token Resolution**: `patchwork_outbox` stores `api_url` and `msgid`,
  *never* the API token. `PatchworkWorker::resolve_token` looks up the token from
  `email_policy.toml` at delivery time.
- **Bounded Retries & Backoff**: Failed HTTP deliveries increment retry counts
  with exponential backoff (5s, 30s, 180s) and transition to a terminal failure
  state after max retries so a down server does not cause an infinite retry storm.

## Checklist for Diffs Touching Email or Patchwork Delivery

1. **Blast Radius**: Could this change widen the `To`/`Cc` recipient set or cause
   a `Muted`/`Dry-Run` message to transition to `Pending`?
2. **Bot Loop Guard**: Is `is_ignored_author` checked against both the patch
   author and the message sender before queuing any notification?
3. **Embargo Check**: Does every new notification path check whether the review
   or patchset is under embargo before inserting outbox rows?
4. **No Stored Secrets**: Are API tokens or SMTP passwords kept strictly in
   memory and never serialized into database outbox rows or logs?
