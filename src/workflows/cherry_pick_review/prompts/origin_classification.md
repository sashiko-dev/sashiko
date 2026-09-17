# Stage 11. Origin Classification

You are classifying the origin of each finding from a merge-conflict resolution review.

For each finding, determine its origin:
- resolution_introduced: The merge/resolution CREATED this issue. It did not exist in the original patch, and it did not exist in the target base branch; combining them introduced it.
- original_patch_preexisting: This issue already existed in the ORIGINAL PATCH being ported (commit 1). The conflict resolution did not cause it; it was already present in that patch.
- base_preexisting: This issue already existed in the TARGET BASE branch (commit 2) before the resolution was applied. The resolution did not cause it.

IMPORTANT: Use tools to check the code BEFORE the resolution commit to determine origin. Compare the state at the parent commit with the resolution commit. If the problematic code existed identically before the resolution, it is preexisting.

Return ONLY a JSON object with a 'findings' array. Each finding must preserve ALL fields from the input and ADD an 'origin' field with one of the three values above.
