# Standalone prerequisite message IDs

## Problem

Some series declare `prerequisite-message-id` without any `prerequisite-patch-id` entries. These currently resolve no prerequisites even when the referenced series supplies the missing patch context.

## Resolution and precedence

1. Valid `prerequisite-patch-id` entries remain authoritative, including their order. Message IDs remain optional search hints in this mode; never expand them into whole series or fall back after a lookup failure.
2. Recognized `prerequisite-patch-id` entries with no valid patch IDs are an error, even when no series metadata is present. Do not fall back to either `prerequisite-message-id` or `Based-on`, or silently discard the declared dependencies.
3. Without patch-ID metadata, `prerequisite-message-id` entries select complete prerequisite series in declaration order, ahead of `Based-on`.
4. Otherwise use `Based-on`.

Quoted, indented, and diff-added lines are not active metadata. Preserve the existing patch-ID parser's filtering of malformed values when at least one valid patch ID is present; the resulting valid list remains authoritative.

Reuse the existing whole-series resolver: load a complete local series or fetch its thread from Lore, validate the exact series head and all numbered parts, resolve nested dependencies, and deduplicate patches by stable patch ID. Do not collect arbitrary patches or replies from a thread, change the requested version, or infer dependencies from prose.

Share message-ID validation with `Based-on`. Standalone references must be valid and bounded; unlike optional hints, malformed entries and excess unique references are errors rather than silently discarded dependencies. Preserve optional hint behavior when explicit patch IDs exist.

Keep the existing limits of eight Lore operations, eight dependency levels, 500 messages per thread, and 128 unique prerequisite patches. Bound standalone references and the total number of distinct loaded series to 128 as well, including locally cached series. Memoize successfully resolved series to avoid repeatedly traversing shared dependency graphs. Active-series tracking continues to detect cycles.

No database, network endpoint, baseline-selection, or public API changes are needed.

## Validation

Cover precedence, ordered multiple references, local and remote resolution, incomplete local series fallback, malformed and quoted metadata, unavailable dependencies, shared dependencies, mixed dependency cycles, and all resource bounds. Verify that all-invalid patch-ID metadata fails before either whole-series fallback or any Lore lookup, including when no fallback metadata exists. Keep explicit patch-ID tests proving that extra thread patches are not applied.

Use synthetic committed fixtures for application tests. Separately replay a real failing series and its referenced prerequisites through the resolver and Git application path; keep downloaded emails and diagnostic artifacts outside the repository.
