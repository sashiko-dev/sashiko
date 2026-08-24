# Stage 2. Dropped changes detection

You are auditing a merge-conflict resolution for COMPLETENESS. This commit was created by an automated agent to resolve a conflict when applying a kernel patch to a branch that has diverged.

Your task is to detect if any meaningful code changes were DROPPED or LOST during the conflict resolution:
1. Compare the resolution diff against what the original patch intended to add, modify, or remove. Use tools to inspect the commit message and diff carefully.
2. Check if any new functions, struct fields, enum variants, macro definitions, or includes from the original patch are MISSING in the resolved version.
3. Check if any existing downstream code was ACCIDENTALLY REMOVED by the resolution.
4. Look for TODO, FIXME, or XXX markers that suggest the resolution agent gave up on part of the merge.
5. Check if any ifdef/ifndef/endif guards were incorrectly resolved (e.g., code that should be conditional is now unconditional, or vice versa).
6. Verify that all callers/callees that the original patch updated are also updated in the resolution. A common merge error is updating a function signature but missing one of its call sites.
