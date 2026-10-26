+++
title = "Outbound Email Routing and Mailing List Policy"
paths = [
    "projects/linux/mailing_lists.toml",
    "src/email_policy.rs",
    "src/email_router.rs",
    "src/patchwork.rs",
    "src/worker/email.rs",
    "src/worker/patchwork.rs",
]
symbols = [
    "DefaultListPolicy",
    "EmailPolicyConfig",
    "EmailRouter",
    "PatchworkPolicy",
    "PositiveReviewPolicy",
    "ReplyTarget",
    "resolve_positive_recipients",
    "resolve_recipients",
]
+++

## Outbound Email Safety and Default-Closed Routing

- Outbound email routing (`EmailRouter::resolve_recipients` and
  `EmailRouter::resolve_positive_recipients`) must default to non-delivery
  (`Action::Mute`) or embargo holding when a patchset's mailing lists are
  unknown, disabled, or under embargo; a fallback path must never widen delivery
  to public `Cc` or mailing-list recipients.
- When `ReplyTarget` restricts recipients (such as `ReplyTarget::Author` or
  `ReplyTarget::List`), ensure neither `To` nor `Cc` headers leak addresses
  outside the allowed set.
- `EmailPolicyConfig` defaults in `src/email_policy.rs` and the shipped policy
  in `projects/linux/mailing_lists.toml` must stay consistent when adding or
  renaming policy fields.
- Outbox state transitions (`Pending` -> `Sending` -> `Sent` / `Failed` /
  `Skipped`) must remain idempotent so worker restarts or transient SMTP errors
  cannot send duplicate review emails for the same patchset.
- Do not report missing TLS certificate pinning on standard `lettre` SMTP client
  builders unless a change explicitly disables TLS verification.
