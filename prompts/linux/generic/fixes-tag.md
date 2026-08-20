# Fixes: Tag Verification

This prompt provides detailed instructions for verifying Fixes: tags when they appear in commit messages.

## Purpose of Fixes: Tags

A Fixes: tag indicates that a patch fixes a bug in a previous commit. The tag:
- Makes it easy to determine where an issue originated
- Helps reviewers understand the bug fix context
- Assists the stable kernel team in determining which stable kernel versions should receive the fix
- Is used by automated backporting tools (e.g., AUTOSEL)
- Should be included even for bugs that don't require stable backporting

## Format Requirements [FIXES-001]

**Risk**: Parsing failures, incorrect stable backports

**Mandatory format validation:**

1. **SHA-1 Length Check**
   - Check that SHA-1 has a minimum of 12 characters (customary standard)
   - Verify hexadecimal characters only (0-9, a-f)
   - Example: `Fixes: c0cbe70742f4` (12 chars) ✓
   - Counter-example: `Fixes: c0cbe70` (7 chars) ✗

2. **Summary Format Check**
   - Verify the subject line is enclosed in double quotes `("...")`
   - Subject line should match the original commit's first line (subsystem prefix and summary)
   - Format: `Fixes: 12+char-SHA1 ("Original subject line")`
   - Example: `Fixes: 54a4f0239f2e ("KVM: MMU: make kvm_mmu_zap_page() return the number of pages it actually freed")` ✓

3. **Single Line Requirement**
   - Verify tag is NOT split across multiple lines
   - Fixes: tags are exempt from commit message line-wrapping rules to simplify automated tooling
   - Even if the line is very long, it must remain on a single unbroken line
   - Counter-example:
     ```
     Fixes: 54a4f0239f2e ("KVM: MMU: make kvm_mmu_zap_page()
       return the number of pages it actually freed")
     ```
     This is INCORRECT - tag must be on a single line

4. **Subject Line Accuracy**
   - Verify that the subject line is intact and accurately formatted
   - Common errors to flag:
     - Truncated subject line
     - Modified or paraphrased subject
     - Missing subsystem prefix
     - Missing surrounding double quotes

## Tag Placement [FIXES-002]

**Risk**: Tag not recognized by automated tools

**Mandatory placement validation:**

1. **Location in Commit Message**
   - Verify tag appears in the sign-off area (after the main commit description)
   - Typical order: Fixes: tag appears before author and reviewer attribution tags
   - Common ordering (from Documentation/process/submitting-patches.rst):
     ```
     <commit description>

     Fixes: <sha1> ("subject")
     Reported-by: <reporter>
     Signed-off-by: <author>
     Reviewed-by: <reviewer>
     ```

2. **Not in Comment Section**
   - Verify tag is placed above the `---` separator
   - Tags below `---` are treated as email comments and not included in git commit history

## Commit Verification [FIXES-003]

**Risk**: Invalid commit reference, incorrect attribution

1. **SHA-1 and Subject Verification**
   - Verify that the referenced commit SHA and subject line match historical kernel changes
   - Check if the SHA-1 is a plausible 12+ character commit hash rather than a placeholder

2. **Verify the Bug Actually Exists**
   - Analyze whether the patch under review actually addresses a regression or bug introduced by the cited commit
   - Common errors to check for:
     - Fixes: tag points to an unrelated commit
     - Fixes: tag points to a commit that did not introduce the bug
     - Multiple commits contributed to the bug, but only one is referenced

## Stable Kernel Considerations [FIXES-004]

**Risk**: Missing stable backports, incorrect backport scope

1. **Fixes: Tag Does Not Guarantee Backport**
   - A Fixes: tag alone does NOT automatically trigger stable backports in all subsystems
   - Verify whether `Cc: stable@vger.kernel.org` tag is also required or present
   - Some subsystems opt out of automatic Fixes: backporting and require explicit `Cc: stable`

2. **Stable Tag Verification**
   - Analyze if the bug affects released stable kernels
   - For regressions affecting released kernels, a stable tag should be present in the sign-off block

3. **Backport Prerequisites**
   - If the fix depends on prior commits, verify prerequisite commit notes are present:
     ```
     Cc: <stable@vger.kernel.org> # 6.6+: abc123: dependency description
     ```

## Common Patterns and Edge Cases

### When Fixes: Tag Should Be Present
- Fixing crashes, hangs, data corruption, memory leaks, or security vulnerabilities
- Fixing functional regressions introduced by a specific prior commit
- Fixing compiler warnings or build failures introduced by a specific commit

### When Fixes: Tag May Be Absent
- General performance optimizations without a preceding regression
- Code refactoring or cleanup (without fixing a bug)
- New hardware support or feature enablement
- Code that existed since initial git history (`1da177e4c3f4`)

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
