# Fixes: Tag Verification

This prompt provides detailed instructions for verifying Fixes: tags when they appear in commit messages.

## Purpose of Fixes: Tags

A Fixes: tag indicates that a patch fixes a bug in a previous commit. The tag:
- Makes it easy to determine where an issue originated
- Helps reviewers understand the bug fix context
- Assists the stable kernel team in determining which stable kernel versions should receive the fix
- Is used by automated backporting tools (e.g., AUTOSEL)
- Should be included even for bugs that don't require stable backporting

**TodoWrite format** (one entry per Fixes: tag):
```
Fixes tag: [full tag text]
SHA-1: [commit ID] - length [N chars], exists ✓/✗ (git cat-file -t), reachable ✓/✗ (git merge-base)
Format: quotes ✓/✗, single line ✓/✗, location [sign-off area/below ---/other]
Subject: matches original ✓/✗ - [show both if different]
Bug fixed: ✓/✗/unclear - [reasoning]
Stable tag: present ✓/✗ / not needed - [reasoning]
Issues: [none OR list]
```

## Format Requirements [FIXES-001]

**Risk**: Parsing failures, incorrect stable backports

**Mandatory format validation:**

Track each Fixes: tag in the TodoWrite and verify:

1. **SHA-1 Length Check**
   - Check that SHA-1 has minimum 12 characters
   - Verify hexadecimal characters only
   - Example: `c0cbe70742f4` (12 chars) ✓
   - Counter-example: `c0cbe70` (7 chars) ✗
   - Record SHA-1 length in TodoWrite

2. **Summary Format Check**
   - Verify subject line is enclosed in double quotes
   - Subject line should match the original commit's first line
   - Format: `Fixes: 12+char-SHA1 ("Original subject line")`
   - Example: `Fixes: 54a4f0239f2e ("KVM: MMU: make kvm_mmu_zap_page() return the number of pages it actually freed")` ✓
   - Record quote presence in TodoWrite

3. **Single Line Requirement**
   - Verify tag is NOT split across multiple lines
   - Tags are exempt from the "wrap at 75 columns" rule to simplify
     parsing scripts
   - If the line is very long, it should still remain on one line
   - Counter-example:
     ```
     Fixes: 54a4f0239f2e ("KVM: MMU: make kvm_mmu_zap_page()
       return the number of pages it actually freed")
     ```
     This is INCORRECT - tag must be on a single line
   - Record line wrapping status in TodoWrite

4. **Subject Line Accuracy**
   - Use `git log -1 --format=%s <commit-id>` to get original subject
   - Compare with subject in Fixes: tag
   - Common errors:
     - Truncated subject line
     - Modified or paraphrased subject
     - Missing subsystem prefix
   - Record comparison result in TodoWrite

## Tag Placement [FIXES-002]

**Risk**: Tag not recognized by automated tools

**Mandatory placement validation:**

Track tag location in TodoWrite and verify:

1. **Location in Commit Message**
   - Verify tag appears in the sign-off area (after main commit
     description)
   - Typical order: Fixes: tag appears before other attribution tags
   - Common ordering (from maintainer-tip.rst):
     ```
     <commit description>

     Fixes: <sha1> ("subject")
     Reported-by: <reporter>
     Signed-off-by: <author>
     Reviewed-by: <reviewer>
     ```
   - Record tag location in TodoWrite

2. **Not in Comment Section**
   - Verify tag is above the `---` separator
   - Tags below `---` are not included in the git commit
   - Record separator position in TodoWrite if present

## Commit Verification [FIXES-003]

**Risk**: Invalid commit reference, incorrect attribution

**Mandatory commit validation:**

Track commit verification in TodoWrite:

1. **Commit Existence**
   - Run: `git cat-file -t <commit-id>`
   - Verify it returns "commit"
   - If commit doesn't exist in current tree, check if it's in Linus's
     tree
   - Record existence check result in TodoWrite

2. **Commit Reachability**
   - Run: `git merge-base --is-ancestor <commit-id> HEAD`
   - Verify commit is in mainline history
   - Note: For fixes targeting recent commits, they may be in linux-next
     or subsystem trees
   - Record reachability check result in TodoWrite

3. **Verify the Bug Actually Exists**
   - Read the referenced commit using git show or git log
   - Analyze whether current patch actually fixes a bug introduced by
     that commit
   - Common errors to check for:
     - Fixes: tag points to wrong commit
     - Fixes: tag points to a commit that didn't introduce the bug
     - Multiple commits contributed to the bug, but only one is
       referenced
   - Record bug relationship analysis in TodoWrite

## Stable Kernel Considerations [FIXES-004]

**Risk**: Missing stable backports, incorrect backport scope

**Mandatory stable backport validation:**

Track stable considerations in TodoWrite:

1. **Fixes: Tag Does Not Guarantee Backport**
   - Note: A Fixes: tag alone does NOT automatically trigger stable
     backports in all subsystems
   - Verify whether `Cc: stable@vger.kernel.org` tag is also present
   - Some subsystems (e.g., KVM x86) opt out of automatic Fixes:
     backporting
   - Record stable tag presence in TodoWrite

2. **Stable Tag Verification**
   - Analyze if bug affects released kernels
   - For regressions in the past 12 months, stable tag should be present
   - Verify stable tag is in the sign-off area (NOT as an email Cc
     recipient)
   - Record stable tag assessment in TodoWrite

3. **Backport Prerequisites**
   - Check if fix depends on other commits
   - Verify prerequisite commits are noted if present:
     ```
     Cc: <stable@vger.kernel.org> # 5.10.x: abc123: dependency description
     Cc: <stable@vger.kernel.org> # 5.10.x
     ```
   - Record dependency analysis in TodoWrite

## Common Patterns and Edge Cases

### When Fixes: Tag Should Be Present

1. **Bug Fixes**
   - Fixing crashes, hangs, data corruption, security issues
   - Fixing incorrect behavior introduced by a specific commit
   - Even for bugs that don't need stable backporting

2. **Regressions**
   - Any user-visible regression should have a Fixes: tag
   - Performance regressions
   - Functionality regressions

### When Fixes: Tag May Be Absent

1. **Improvements Without Specific Bug**
   - General optimizations
   - Code refactoring (without fixing a bug)
   - New features

2. **Fixes for Very Old Code**
   - Bug existed since initial git history
   - Alternative: Note in commit message "bug existed since ..."

3. **Multiple Contributing Commits**
   - If multiple commits contributed to a bug, typically reference the most direct/recent cause
   - Can include multiple Fixes: tags if necessary (rare)

## Git Configuration for Reviewers

To make Fixes: tag generation easier, configure git:

```
[core]
    abbrev = 12
[pretty]
    fixes = Fixes: %h (\"%s\")
```

Usage: `git log -1 --pretty=fixes <commit-id>`

## Mandatory Self-verification gate

**After analysis:** Issues found: [none OR list]

## Quick Reference

**Correct Format:**
```
Fixes: 54a4f0239f2e ("KVM: MMU: make kvm_mmu_zap_page() return the number of pages it actually freed")
```

**Common Errors:**
- Too short: `Fixes: 54a4f02 (...)`  ✗
- Missing quotes: `Fixes: 54a4f0239f2e (KVM: MMU: ...)` ✗
- Line wrapped: `Fixes: 54a4f0239f2e ("KVM:\n    MMU: ...")` ✗
- Wrong section: Tag appears below `---` separator ✗
- Missing stable tag for released bug ⚠
