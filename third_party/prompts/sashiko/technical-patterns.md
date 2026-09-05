# Sashiko Technical Review Patterns

## Async ownership and cancellation

- Trace every spawned task, channel sender, receiver, and subprocess to its
  owner. Prove how it terminates during success, error, timeout, and shutdown.
- Do not treat dropping a future as sufficient subprocess cleanup. Check that
  the child is killed when required and subsequently reaped.
- Identify synchronous filesystem, process, compression, or database work in
  async paths that can stall unrelated reviews.
- For bounded channels, inspect backpressure and closure. A failed send must not
  leave a database row claiming that work is queued when no consumer received
  it.

## Git and worktree state

- Treat repository URLs, commit IDs, ranges, patches, paths, refs, and remote
  output as untrusted input.
- Verify Git arguments remain separate process arguments and that protocol
  restrictions are preserved on network-facing fetches.
- Shared repository metadata operations require the existing synchronization.
  Check concurrent remote changes, worktree creation/removal, and pruning.
- Cleanup may remove only Sashiko-owned paths. Check path derivation, ownership
  markers, temporary-directory lifetimes, and partial-failure behavior.
- A review must use the intended base and head. Check range direction, SHA
  validation, patch order, baseline selection, and final worktree contents.

## Webhook and secret boundaries

- When a webhook secret is configured, signature verification must apply even
  when a reverse proxy makes the peer address look local.
- Preserve event-type checks, constant-time signature/token comparison, SHA
  validation, positive PR numbers, and repository URL checks.
- Host blocklists are best-effort SSRF defenses, not proof that DNS resolution
  is safe. Do not weaken primary authentication based on the blocklist.
- Never log credentials embedded in URLs, headers, provider errors, child
  arguments, settings, or Git remotes. Follow values through error formatting.

## Persistence and retries

- Multi-step state transitions must be atomic or explicitly recoverable. Look
  for a durable state update followed by a channel send, remote call, or file
  write that can fail independently.
- Retryable work must be idempotent. Check duplicate patch ingestion, repeated
  webhook delivery, outbox insertion, review attempts, and derived artifacts.
- Preserve the authoritative state before deleting or publishing derived data.
- Propagate errors with enough context to recover, without leaking secrets or
  turning recoverable failures into panics.

## AI-provider and review boundaries

- Preserve provider selection, request/response schemas, tool permissions,
  timeout behavior, quota accounting, and token-budget enforcement.
- Cached tokens are a breakdown of prompt tokens, not additional usage. Check
  arithmetic for underflow, double counting, and inconsistent provider fields.
- Classify rate-limit, transient, and fatal errors consistently. Retrying must
  respect cancellation and must not replay an unsafe side effect.
- Tests should stop at a deterministic fake provider or subprocess boundary.
  A unit or CI test must not consume credentials, quota, or paid API calls.

## Backwards compatibility

- Check Serde defaults, denied unknown fields, environment overrides, CLI
  defaults, persisted schemas, and old Settings.toml files.
- A project-specific change must not silently alter NNTP, GitLab, Patchwork,
  local review, kernel prompts, baselines, worktree semantics, or AI providers.
