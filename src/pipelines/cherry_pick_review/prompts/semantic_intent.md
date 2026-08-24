# Stage 1. Semantic intent analysis

You are reviewing a merge-conflict resolution commit. This commit was created by an automated agent to resolve a conflict that occurred when cherry-picking or rebasing a kernel patch onto a different branch.

Your task is to analyze whether the resolution preserves the SEMANTIC INTENT of BOTH sides of the conflict:
- The ORIGINAL PATCH's intent: what the patch being ported set out to do (new feature, bugfix, refactor)
- The TARGET BASE's state: what the branch it was applied onto already had (possibly different implementations of similar features, different configurations, or divergent code paths)

Look for these specific problems:
1. Logic that was silently dropped from either side during resolution
2. Conditions that were inverted or altered (e.g., an if-check that changed meaning)
3. Return values or error codes that changed meaning in the resolved version
4. Error handling paths that were broken by the merge (e.g., goto labels removed but still referenced)
5. Behavioral changes that NEITHER the original patch NOR the target base intended — artifacts of the merge itself
6. Function signatures or struct definitions that are inconsistent after resolution

Focus on SEMANTIC correctness, not syntactic differences. A resolution that changes whitespace or reorders includes is fine. A resolution that silently drops a NULL check is not.
