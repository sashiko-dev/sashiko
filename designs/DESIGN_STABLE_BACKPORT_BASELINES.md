# Stable backport baseline hints

## Problem

Bare version prefixes such as `[PATCH 5.10]` and explicit `.y` hints already select a stable branch before the original release tag. Subjects such as `[STABLE v6.6]` still select only the original `v6.6` release tag, even though the `STABLE` marker identifies a backport. Compact ranges such as `[STABLE v6.6-v6.1]` also lose the second named version.

Stable branches accumulate fixes and backports after the original release. A patch prepared for a stable branch can depend on code or file layout changes absent from the original release tag, causing application to fail against that tag.

## Design

Keep the existing bare-version, explicit `.y`, and release-tag behavior. When the bracketed subject metadata contains a complete, case-insensitive `STABLE` token, recognize the first complete stable-series version token or compact two-endpoint range in those brackets. Each endpoint must contain exactly two ASCII decimal components, with an optional `v` prefix and `.y` suffix, both case-insensitive. Point releases, release candidates, and arbitrary suffixes are not stable-series endpoints. Validate both range endpoints before accepting either.

For `[STABLE vX.Y]`, try `stable/linux-X.Y.y`. For `[STABLE vX.Y-vA.B]`, try `stable/linux-X.Y.y` and then `stable/linux-A.B.y`, preserving the written endpoint order. Do not enumerate intermediate releases or discover branches remotely. A range introduces at most two stable candidates; the existing candidate deduplication removes repeated endpoints. The reviewer still selects the first baseline on which the complete series applies rather than reviewing separately for each named version.

Explicit `base-commit` metadata remains first. All stable candidates precede the existing original-release fallback from the version extractor. The remaining subsystem, custom-remote, linux-next, and mainline candidates retain their order. If no new stable token is recognized, use the existing bare-version and `.y` rules without modification.

Without a bracketed `STABLE` marker, an explicit `vX.Y` token continues to name the exact release tag. Point releases such as `X.Y.Z` and release candidates such as `X.Y-rcN` also remain exact tags, including in `STABLE` subjects. Neither a stable-related word in prose nor a substring such as `stable-next` enables the new rule. Versions in prose or embedded in another token do not gain stable candidates through this extension.

Share the existing bracket/token tokenizer with the subject-prefix parser, exposing tokens in their original order for baseline selection. Keep the existing prefix API's filtering, sorting, and deduplication unchanged so subsystem matching is unaffected. Do not change prerequisite handling, release-tag extraction, Git fetching, or the database schema.

## Design review

The new candidates use the same fixed stable repository, remote name, and branch-fetch path as existing hints. Complete-token validation prevents partial matches on point releases, release candidates, and custom suffixes. Parsing endpoints as text avoids integer overflow and numeric range expansion from untrusted subjects. Restricting selection to the first matching token bounds the extension to two stable candidates, even if the subject contains many versions.

Preserving endpoint order avoids the lexical sorting performed by the existing subsystem-prefix API. Trying both named branches before the original release prevents the first release tag from taking precedence over the second stable branch. Keeping the release tag and all later candidates preserves fallback behavior if a stable branch is missing or rejects the patch. Explicit baseline metadata remains authoritative.

## Validation

Extend table-driven candidate-resolution tests for `STABLE` singles and ranges, uppercase markers and endpoints, cover letters, rerolls, separated subject tags, ascending and descending ranges, cross-major ranges, repeated endpoints, and bounded selection. Assert the complete candidate ordering with and without explicit `base-commit` metadata, including stable branches before the unchanged original-release fallback.

Retain tests for bare versions and explicit `.y` hints. Add negative cases for exact release/point/rc tags, malformed ranges, marker substrings, versions embedded in unrelated tokens, and stable/version words outside brackets. Verify that shared tokenization preserves the existing subject-prefix API's behavior.

Replay representative failing single-version and range backports against original release tags and corresponding stable snapshots, without changing the patches. Keep downloaded examples and replay fixtures outside the repository. These are application checks, not kernel build or runtime tests. Run `make check-pr` before each commit.
