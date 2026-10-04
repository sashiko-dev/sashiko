# Stable backport baseline hints

## Problem

Subjects such as `[PATCH 5.10]` name a stable release series, but baseline resolution currently tries only the original `v5.10` release tag before falling back to subsystem and mainline trees. It selects `linux-5.10.y` only when the subject includes `.y`. The existing kernel-tree design already specifies that `[PATCH 6.1]` should select the stable branch.

Stable branches accumulate fixes and backports after the original release. A patch prepared for a stable branch can depend on code or file layout changes absent from the original release tag, causing application to fail against that tag.

## Design

Keep the existing version extraction and explicit `.y` behavior. Additionally, recognize a bare two-component version only when it is a complete subject-prefix token, using the existing subject-prefix parser. For `[PATCH X.Y]`, try the existing stable remote with branch `linux-X.Y.y`, followed by the original `vX.Y` tag as a fallback. Explicit `base-commit` metadata remains first, and the remaining subsystem, custom-remote, linux-next, and mainline candidates retain their order.

An explicit `vX.Y` token continues to name the exact release tag. Point releases such as `X.Y.Z` and release candidates such as `X.Y-rcN` also remain exact tags. Decimal numbers in ordinary subject text or embedded in another token do not gain a stable-branch candidate. This change does not rewrite the existing version extractor, infer versions from email recipients, choose several stable series from one subject, or alter prerequisite handling and Git fetch behavior.

## Design review

The new candidate uses the same fixed stable repository, remote name, and branch-fetch path as explicit `.y` hints. Only a complete bare version token enables the new behavior, so the change does not turn ordinary hardware or protocol versions in subject prose into stable-branch fetches. Keeping the release tag and all later candidates preserves fallback behavior if the stable branch is missing or does not accept the patch. Explicit baseline metadata remains authoritative.

## Validation

Add table-driven candidate-resolution tests for bare and explicit `.y` stable versions, cover letters, rerolls, separated subject tags, exact release/point/rc tags, and numbers outside subject metadata or embedded in unrelated tokens. Assert the complete candidate ordering, including explicit `base-commit` precedence and the original-release fallback. Validate representative backports against original release tags and corresponding stable branches using unchanged patches in temporary repositories. Run `make check-pr` before committing.
