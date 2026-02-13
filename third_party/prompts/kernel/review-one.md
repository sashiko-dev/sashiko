You're reviewing a kernel patch, which is the top commit
of the provided directory.

CRITICAL REQUIREMENTS:
- The review prompt directory is "../review" and it contains all of the prompts
you'll use
- You MUST follow the complete 'review-core.md' checklist from the review directory
- NO shortcuts, NO "quick analysis", NO abbreviated reviews

1: Use git show to load the commit message into context, make sure the patch matches the description in the commit summary
2: send the diff from git show into semcode to get a list of functions changed for context
3: Report any failures to obtain context with semcode
4: Report any context that you use from the kernel directory outside of semcode
5: Output a final verdict for the patch: "FINAL REGRESSIONS FOUND: <number>"
