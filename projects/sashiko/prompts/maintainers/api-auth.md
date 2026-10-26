+++
title = "HTTP API, Authentication, and Access Control Policy"
paths = [
    "src/access.rs",
    "src/api.rs",
    "src/auth.rs",
    "src/server.rs",
]
symbols = [
    "AclSettings",
    "AuthUser",
    "BugAccess",
    "LocalToken",
    "Principal",
    "SectionTitle",
    "is_authorized",
    "presents_local_token",
    "resolve_bug_read_principal",
]
+++

## Authentication and Secret Comparison

- All bearer token and secret comparisons in `src/auth.rs` and `src/server.rs`
  must use constant-time equality (`subtle::ConstantTimeEq`) on raw byte slices.
  Flag any standard `==` or early-returning prefix comparison on secret values.
- `LocalToken` files must be created with mode `0600` before writing token
  bytes.
- When the server is started in `read_only` mode (`--read-only`), all state-
  mutating HTTP endpoints must be rejected even if a caller presents a valid
  token.

## Bug Access Control and Redaction Policy

- Every HTTP handler or database query that returns bug records, counts, facets,
  or review transcripts tied to restricted bugs must enforce `Principal` and
  `BugAccess` filtering consistently across both single-item lookups and bulk
  list/stats queries.
- Access checks must fail closed: when a bug or patchset has no resolved
  maintainer sections (and is not public or explicitly granted via a capability
  UUID), section-scoped maintainer roles must not gain access by default.
- Knowing a bug's public `<project>-<uuid>` identifier in a direct URL may
  grant read access (`BugAccess::Read` via `resolve_bug_read_principal`) to
  that specific bug when policy permits, but must never elevate the caller to
  `Comment` or `Manage` or expose unrelated restricted bugs in list endpoints.
- When redacting restricted fields from API responses for unprivileged callers,
  ensure every side channel in the response payload (summaries, inline logs,
  counts, and error messages) respects the same access boundary.
