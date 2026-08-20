# Missing Fixes: Tag Detection

This prompt identifies commits that appear to fix bugs but lack a Fixes: tag.

## Purpose

A Fixes: tag should be included when a patch fixes a bug in a previous
commit, even if the fix does not require stable backporting. Missing
Fixes: tags make it harder to:
- Track bug origins and regressions
- Determine stable backport scope
- Understand fix context during code review
- Correlate fixes with their original bugs

## When to Flag Missing Fixes: Tags

Flag commits as missing a Fixes: tag when the commit:
- Fixes a crash, oops, deadlock, memory leak, use-after-free, or data corruption
- Reverts a prior erroneous commit or repairs broken functionality introduced by an identifiable commit
- Fixes compiler warnings or build breakage caused by a specific earlier change

Do not flag missing Fixes: tags for:
- New feature implementation or hardware driver enablement
- Refactoring, cleanups, or code modernization
- Code where the bug has existed since initial git history (`1da177e4c3f4`)
- Changes where commit messages explicitly explain that the issue has always existed or predates git history

## Suggested Fixes: Format

When a missing Fixes: tag is identified for a known commit, suggest adding:
```
Fixes: 12-char-SHA1 ("Exact Commit Subject")
```
