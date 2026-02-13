---
name: systemd-verify
description: Verify findings against false positive patterns
---

Using the prompt REVIEW_DIR/false-positive-guide.md, verify that a
reported issue is not a false positive.

This command helps validate potential bugs found during review or debugging.

For each issue:
1. Trace the exact code path that triggers it
2. Verify the path is actually reachable
3. Check if validation happens elsewhere
4. Confirm this is production code, not test/debug
5. Provide concrete evidence (code snippets, call chains)

Output:
- VERIFIED ISSUE: if the bug is real
- ELIMINATED: if the issue is a false positive
