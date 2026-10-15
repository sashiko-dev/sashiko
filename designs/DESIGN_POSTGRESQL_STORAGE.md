# Configurable PostgreSQL storage

## Goal

Keep the current local libSQL file as the default. Allow an operator to opt in
to PostgreSQL for either all daemon data or the shared bug catalog. Both modes
must preserve bug identity, analysis history, access checks, and review links.
Local reviews continue to work without any daemon database.

## Configuration

Use an explicit database mode rather than inferring a backend from a URL:

| Mode | Primary data | Bug catalog | Default |
| --- | --- | --- | --- |
| `libsql` | libSQL | libSQL | yes |
| `postgresql` | PostgreSQL | PostgreSQL | no |
| `libsql_shared_bugs` | libSQL | PostgreSQL | no |

`[database]` keeps its existing `url` and `token` for libSQL. Add `[storage]`
with `mode` and `postgres_url` for the PostgreSQL connection. Require it in either
PostgreSQL mode. Reject a PostgreSQL URL passed as the libSQL URL, and reject
mixed mode without a local or remote libSQL URL. Do not log credentials.

## Database boundary

Move SQL execution, query rows, and transactions behind a backend-neutral
interface owned by `src/db/`. The interface must support typed parameters,
nullable values, returned rows, and explicit transaction ownership. Keep
SQLite-specific SQL in the libSQL implementation and use native PostgreSQL
queries in the PostgreSQL implementation. The execution boundary translates
the application's closed query vocabulary with a SQL lexer: positional
parameters, case-insensitive LIKE, string aggregation, and native PostgreSQL
date and JSON helper functions. Values stay bound through the PostgreSQL wire
protocol. Native migrations separately handle identity columns, bytea payloads,
audit and projection triggers, and transaction behavior.

Full PostgreSQL configuration:

```toml
[storage]
mode = "postgresql"
postgres_url = "postgresql://sashiko:password@database.example/sashiko"
postgres_schema = "sashiko"
# postgres_ca_file = "/etc/sashiko/postgres-ca.pem"
```

TLS is required, with certificate and hostname verification by default.
`postgres_tls_require_only = true` explicitly opts into encryption without
certificate verification for environments using an untrusted certificate.
Connection URLs are secret and should come from deployment secret storage
through `SASHIKO__STORAGE__POSTGRES_URL`, using the existing settings loader's
environment convention. PostgreSQL schemas isolate independent deployments.

Callers use the backend-neutral connection rather than a public libSQL
connection. Keep the no-default-features local review build independent
of both daemon database drivers.

## Full PostgreSQL mode

Port the complete daemon schema, including patchsets, reviews, bugs, outboxes,
and metadata. Preserve the same logical tables and public bug IDs. Maintain
backend-specific, versioned migrations. PostgreSQL schema versioning must be
transactional and protected against concurrent daemon startups. Translate
SQLite triggers and JSON projections into PostgreSQL triggers or explicit
transactional writes, with tests proving equivalent behavior. Use PostgreSQL
an advisory transaction lock for worker claims and outbox leases, preserving
the existing single-writer transaction contract across daemon instances.
Canonical bug deduplication also takes a PostgreSQL session lock across
the candidate check and final write. Keep a single database
transaction for operations that currently depend on SQLite atomicity.

## Shared bugs mode

PostgreSQL owns the shared catalog; each libSQL daemon keeps a local bug mirror
alongside its reviews and patchsets. A stable instance UUID and public bug IDs
identify synchronization records. Local row IDs never cross the catalog
boundary. Discovery commit references remain part of the shared evidence.

SQLite triggers maintain a coalescing outbox in the bug mutation's transaction.
The synchronizer publishes a typed snapshot with stable event identities, then
imports changes in bounded batches through a durable sequence cursor. PostgreSQL
serializes catalog changes so sequence order follows commit order. The local
base snapshot permits a three-way merge: independent edits and evidence combine,
while concurrent triage or assignment conflicts retain the catalog decision and
record a conflict event. A failed publication stays queued for retry.

A local mirror preserves its review links; imported records have no foreign keys
to another instance's patches or reviews. Analysis remains with the originating
instance. The shared deduplication lock imports the latest catalog before checking
candidates and publishes the canonical result before releasing the lock. Bugs
remain readable during an outage, and the UI shows pending synchronization.

Bug permissions remain enforced by the Sashiko API. Catalog participants must
share a trusted permission and subsystem attribution policy. Project-scoped
catalog queries prevent accidental mixing of independent projects.

## Migration and rollout

1. Add config parsing and validation, preserving the existing default.
2. Hide direct libSQL connection access behind database methods.
3. Add backend-neutral operations and a full PostgreSQL schema and migration
   path. Port each query family with parity tests.
4. Add shared bug catalog routing, public-ID links, and a retryable outbox.
5. Test fresh databases, upgrades, worker claims, duplicate merges, concurrent
   writers, authorization, and failure/retry behavior for all three modes.

Existing libSQL data is not silently copied. A separate, explicit migration
tool may be added later; selecting a new mode starts against that mode's own
database. Operators must retain their original database for rollback.

## Review checkpoints

- A PostgreSQL mode is ready only when `make check-pr` passes and integration
  tests run against an actual PostgreSQL server.
- Shared bugs mode is ready only when a failed cross-database write is retried
  without duplicate bugs or lost review links.
- Default libSQL behavior and the minimal local-review feature profile must
  continue to pass their existing checks.
