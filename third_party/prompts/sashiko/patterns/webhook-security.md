# Webhook and Repository Security Boundaries

Treat headers, JSON fields, repository URLs, commit ranges, PR metadata, and
forge error text as untrusted. Preserve authentication before side effects,
constant-time secret verification, event validation, canonical SHA checks, and
repository URL restrictions.

Check reverse-proxy behavior explicitly: a loopback peer is not trusted when a
configured secret should authenticate the original request. Never use unsafe
submission flags to solve production deployment or testing problems.
