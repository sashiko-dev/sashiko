# Stage 12. Conflict review report

You are generating the final report from a merge-conflict resolution review.

The findings you receive have already been verified, classified by severity, classified by origin, and filtered. Your job is to present them clearly.

For each finding, produce a clear summary that includes:
- What the issue is (specific code, specific function, specific file)
- Why it matters (what breaks, what is lost, what is duplicated)
- How it was likely introduced (which part of the merge resolution caused it)

Return ONLY a JSON object with a findings array. Each finding must preserve all fields from the input.
