# Revision-correct review prefetch

## Problem

Review tools virtualize HEAD to the target patch, but prefetch reads working
files and runs git grep without a revision. Historical commits, dirty local
checkouts, and concurrent reviews sharing a worktree can therefore receive
source from another revision, mislabeled as the target's source.

## Implementation

Require the target commit SHA when constructing prefetch context. A private
snapshot owns the repository path, verified commit SHA, and a per-invocation
cache of source blobs keyed by repository-relative path. The repository and
revision are fixed for the cache's lifetime. Read blobs from Git objects and
search that same commit; never consult working files or change the checkout.

Extraction, opaque-type filtering, definition scoring, and rendering reuse
the snapshot's contents. Parse revision-prefixed search results explicitly.
Sort symbols before applying the existing symbol limit so unchanged inputs
produce stable selections. Preserve current extraction limits and ranking.

Label emitted context with its source SHA. If prefetch fails, log the error
and instruct the review to retrieve context using revision-aware tools. Never
fall back to working files. Parent and series-final inspection remain explicit
tool operations, separate from target prefetch.

## Validation

Use a small committed C fixture with changed functions, external helpers, and
referenced structs. Assert identical target context from the baseline, target,
later, dirty, and missing-file checkouts. Cover added, deleted, and renamed
files, concurrent requests for different SHAs, invalid/missing objects, and
prompt provenance. Run worker reviews with a mock provider to inspect the
actual model requests without a daemon or database. Run make check-pr and
available review smoke checks before committing.

## Scope

This fixes source provenance. It does not expand the dependency graph, change
review stages, or alter the intentional series-final verification policy.
Keeping reads in Git objects preserves concurrent use of a shared worktree.
