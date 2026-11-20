+++
title = "General"
paths = ["*"]
symbols = []
+++
# General: maintainer notes

Generic subjective code-quality and commit-message review guidance for all non-trivial Linux kernel commits.

- Do not flag isolated grammar slips (e.g. "its" vs "it's") unless there are enough of them to make the commit message or comments hard to understand.
- Judge whether the code is clean and easy to understand, comparing the new changes with nearby code since much of the kernel is inherently complex.
- Consider whether a less complex approach could solve the same problem.
- Look for code duplicated within the change itself.
- Look for existing related functions that the diff duplicates instead of reusing.
- When new APIs are introduced, check that they are consistent in structure and expectations with existing APIs, including function naming and argument order.
- When new APIs are introduced, check that their locking and allocation/free expectations are clearly documented.
- When existing APIs are used, flag usage that reaches into internal/private fields not meant for consumption by callers.
- Check that the commit title and message match the actual code changes.
- Check that the changelog is complete and describes all significant changes.
- Check that the changelog is concise, without unnecessary verbosity.
- Check that the changelog explains why the change is made, not just what it does.
- Flag missing context that would help reviewers and maintainers understand the change.
- Verify existing `Fixes: <12-char sha> ("<subject>")` trailers against repository history and suggest adding a missing `Fixes:` trailer at `Low` severity on bug-fix commits when the introducing commit in the repository can be identified with confidence from git history (never on new features, refactorings, cleanups, documentation, or commits in the same patch series).
- When reviewing a stable backport patch (such as `[PATCH 5.10 / 6.1.y / 6.6.y / 6.12.y / 6.18.y]` or a commit carrying `commit <sha> upstream`, `[ Upstream commit <sha> ]`, or `(cherry picked from commit <sha>)`), check only backport adaptation defects (missing prerequisite commits, broken surrounding context, API/locking/struct differences in the target stable tree, or bugs newly introduced while adapting the patch); do NOT flag pre-existing limitations, minor statistics inaccuracies, or edge cases that behave identically in the upstream mainline commit, because `-stable` patches intentionally preserve upstream commit semantics.
