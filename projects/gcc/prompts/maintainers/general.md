+++
title = "General"
paths = ["*"]
symbols = []
+++
# General: maintainer notes

Generic subjective code-quality, testsuite, and commit-message review guidance for GCC commits.

- Do not flag isolated grammar slips or GNU Coding Standards whitespace issues (such as missing two-space sentence breaks in comments or space-before-parenthesis formatting) unless they make the code or commit message hard to understand.
- Do not flag missing or placeholder dates in inline ChangeLog blocks inside commit messages (`gcc-verify` and server-side hooks validate ChangeLog entries at push time).
- Prefer RAII containers (`auto_vec`, `auto_bitmap`, `auto_sbitmap`) over manual `vec::create`/`release` or `BITMAP_ALLOC`/`BITMAP_FREE` lifecycles in new code when an early return or error path is present.
- Prefer target hooks (`targetm.*`) over adding new target-specific `#ifdef` conditional compilation blocks in generic middle-end or front-end files.
- Check that the commit title and message match the actual code changes and explain why the transformation or fix is needed, not just what it does.
