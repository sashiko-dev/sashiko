# Persistence, Retry, and Recovery Boundaries

Trace each logical operation across database writes, queue sends, Git changes,
files, and remote publication. Identify which state is authoritative and prove
that partial failure is atomic, compensated, or safely recoverable.

Retries and duplicate webhooks must not create duplicate patchsets, publish a
result twice, delete state from a newer attempt, or repeat a non-idempotent
tool action. Check the identity key and snapshot revalidation used by every
retry.
