---
name: systemd-review
description: Review systemd commits for regressions
---

Using the prompt REVIEW_DIR/review-core.md, run a deep dive regression
analysis of the specified commit or range.

If no commit is specified, analyze the top commit (HEAD).

Load REVIEW_DIR/technical-patterns.md first, then follow the complete
review protocol in review-core.md.

For the commit being analyzed:
1. Understand the commit's purpose
2. Identify all changed files and functions
3. Load relevant subsystem files based on what changed
4. Analyze for regressions following the protocol
5. Create review-inline.txt if issues found
