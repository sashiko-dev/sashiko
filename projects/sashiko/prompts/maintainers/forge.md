+++
title = "External Forge Webhooks and Review Posting Policy"
paths = [
    "src/forge.rs",
    "src/worker/forge.rs",
]
symbols = [
    "ForgeProvider",
    "ForgeSettings",
    "ForgeWorker",
    "verify_github_signature",
    "verify_secret_token",
]
+++

## Webhook Authentication and Trust Boundaries

- Every incoming GitHub (`X-Hub-Signature-256`) or GitLab (`X-Gitlab-Token`)
  webhook request must be cryptographically verified in constant time before the
  payload is parsed into actionable ingestion state.
- Treat all fields inside webhook JSON payloads (branch names, commit SHAs, PR
  titles, author names, body text, and repository URLs) as untrusted input:
  - validate commit SHAs before passing them to git commands;
  - never interpolate untrusted PR text into shell commands or unescaped prompt
    directives.
- Installation tokens and forge API credentials must never appear in error
  strings, tracing logs, or database rows; format reqwest errors with secret
  redaction.
- When posting review comments back to GitHub or GitLab, respect provider body
  length limits by truncating on a valid UTF-8 character boundary and preserving
  a clear truncation notice.
