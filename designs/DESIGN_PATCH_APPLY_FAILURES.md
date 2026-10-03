# Classified patch application failures

## Problem

Ordinary patch application discards the error returned by `GitWorktree::apply_patch`, leaving baseline logs with only a generic failure. Publishing raw Git output was rejected in [PR #393](https://github.com/sashiko-dev/sashiko/pull/393#issuecomment-5331306307) because of security concerns. Credential redaction cannot establish that arbitrary process output is safe for a public log.

## Design

Represent an unsuccessful `git am` exit with an internal typed error that retains the existing detailed error representation for existing callers. Attach a finite `PatchApplyFailure` enum: missing file, existing file, context mismatch, malformed patch, empty patch, or unknown failure. Extract the enum through the error type, not by reparsing an `anyhow` display string. Process-launch and I/O errors without this type map to unknown.

Run `git am` with `LC_ALL=C` so classification does not depend on the host's language. Recognize a small allowlist of complete or anchored diagnostic lines on stderr. Ignore incidental advice and warnings; unrecognized error/fatal lines or conflicting recognized categories yield unknown. Git reports an empty patch on stdout, so allow only its exact standalone `Patch is empty.` output, without an error/fatal diagnostic. Do not infer that an unsuccessful patch was already applied.

The reviewer writes only the enum's fixed, static explanation into the ordinary patch's baseline attempt log. It must never interpolate the error, its sources, stdout, stderr, filenames, subjects, URLs, or matched diagnostic fragments. The existing baseline, patch identification, failure summary, and web log viewer remain unchanged. Unknown failures have a fixed public message; there is no raw-output fallback. Classification is best-effort and informational: it never affects status, baseline ordering, retries, application, or worktree cleanup.

Keep the existing detailed `apply_patch` error display for local review and other existing callers. This work changes only the ordinary target-patch baseline diagnostic path; it neither expands raw logging elsewhere nor redesigns prerequisite/fetch diagnostics. No database migration, new UI component, configuration switch, or dependency is required.

## Validation

- Table-driven classifier tests cover recognized diagnostics, anchored matching, unknown/localized/malformed output, mixed categories, empty patches, and misleading subject echoes. Malformed-patch cases include both older line-only diagnostics and newer Git diagnostics containing a patch filename and line number; these locations must not reach public messages.
- Test error downcasting through context and the unknown fallback for unrelated errors.
- Real Git fixtures verify the categories and retain the existing detailed error display and abort behavior.
- Reviewer regressions assert exact fixed messages for missing-file and unknown failures, without arbitrary data from either output stream; a context-conflict failure followed by a successful baseline retains its category only in the failed attempt.
- Run `make check-pr` across all feature profiles, followed by local Sashiko self-review before proposing the PR.

## Design review

The public-output boundary accepts only a finite enum and returns static strings, so classifier mistakes cannot reflect process output into the UI. Subject echoes cannot be mistaken for diagnostics. Unknown and conflicting failures remain useful generic failures rather than guessed root causes. Existing callers retain their behavior, and no classification controls the review workflow.
