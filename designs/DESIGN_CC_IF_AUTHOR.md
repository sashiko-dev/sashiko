# Direct-to-Author Review Notifications

## 1. Problem Statement

Two related requests from developers who contribute to subsystems whose
mailing lists are currently **silent / dashboard-only** (Sashiko reviews
patches but sends no email, or pings the whole list):

1. "I develop for intel-xe. The list is currently silent/dashboard-only,
   but I would love to receive the reviews on my own patches. Add a
   mechanism so individual devs can opt-in to direct emails without
   spamming the whole list."
2. "We have people in the developer group who would like to get the review
   mails for their patches but don't need to see everything."

Both reduce to a single recipient-routing outcome: a subsystem can
**track a mailing list** (so its patches are ingested and reviewed) while
**not sending the review to that list** — instead the review goes **only to
the patch author**.

## 2. Decision: No New Flag — Use the Existing Combination

Investigation and empirical testing (all 8 `email_router` unit tests pass)
confirmed the existing flags in `email_policy.toml` already produce
author-only delivery. No new `cc_if_author` flag is required, so none is
added (per implementation economy).

The combination that yields "track the list, email only the author":

```toml
[defaults]
reply_all = false          # never send to public lists
reply_to_author = false    # (see note below)
cc_individuals = false     # drop non-list individuals

# --- per-subsystem (intel-xe case) ---
[subsystems.drm-intel]
lists = ["intel-xe@lists.freedesktop.org"]
reply_all = false
reply_to_author = true
cc_individuals = false
```

Why it works (`src/email_router.rs`, `resolve_recipients`):

- `reply_all = false` sets `is_private = true`, which strips every
  mailing-list address from the outgoing `To`/`Cc` (the list is still
  matched and tracked via `lists`, so its patches are reviewed).
- `reply_to_author = true` adds the patch author.
- `cc_individuals = false` drops non-list individual recipients.

Net result: the review goes **only to the author**. Because each review
targets exactly one author, this delivers "reviews on my own patches"
without pinging the list.

## 3. Known Interaction

**Multi-subsystem "union" rule** (`src/email_router.rs:415`): if a single
patch matches both a public subsystem (e.g. `cc_individuals = true`) and a
private one, the union keeps non-list individuals. This is an edge case,
not a blocker for author-only delivery on a single subsystem.

## 4. Deliverables

- A focused regression unit test in `src/email_router.rs` asserting the
  author-only combination (list stripped, author kept, individuals empty).
- Documentation updates: `docs/configuration.md`,
  `docs/examples/email_policy.toml`, `MAINTAINERS_GUIDE.md`,
  `email_policy.toml`.

## 5. Out of Scope

- A `cc_if_author` config flag (redundant with the existing combination).
- Self-service per-developer subscriptions via UI/API.
- Patchwork delivery changes.
