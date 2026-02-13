# Missing Fixes: Tag Detection

This prompt identifies commits that appear to fix bugs but lack a Fixes:
tag.

## Purpose

A Fixes: tag should be included when a patch fixes a bug in a previous
commit, even if the fix doesn't require stable backporting. Missing
Fixes: tags make it harder to:
- Track bug origins
- Determine stable backport scope
- Understand fix context during code review
- Correlate fixes with their original bugs

## When to Flag Missing Fixes: Tags

**Risk**: Lost attribution, incomplete stable backports, poor git
archaeology

```

## Finding the Fixed Commit

If this is a bug fix, search git history, either with semcode or git log, find
the commit being fixed.

If you're able to identify a commit being fixed, create a suggested Fixes:
tag.

```
Fixes: <short SHA> ("<commit subject>")
```

<short SHA> is the first 12 characters of the SHA
<commit subject> is the entire subject, surrounded by (" ")

Example:

```
Fixes: 54a4f0239f2e ("KVM: MMU: make kvm_mmu_zap_page() return the number of pages it actually freed")
```

In this case, consider the missing Fixes tag a regression, and make sure it
gets added into review-inline.txt.  Explain how the commit being reviewed
fixes the commit identified.

## If no fixed commit can be identified

If we're doing a subjective review, consider the missing Fixes: tag a regression
report:

```
This commit appears to fix a bug, but the commit that introduced the bug has
not been identified.  Please consider searching for the commit being fixed.
```

If we're not doing a subjective review, don't consider this a regression.

Output:
```
Fixes: tag missing (y/n) [Fixes: line if discovered]
```

