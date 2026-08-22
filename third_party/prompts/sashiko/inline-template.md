# Sashiko Inline Review Format

Produce plain text suitable for a code-review comment. Do not use Markdown code
fences. Start with the reviewed commit, its Author line, and subject. Quote only
the minimal relevant diff using email-style `>` prefixes.

Place each comment immediately after the quoted code that introduces the
problem. Put `[Severity: Critical]`, `[Severity: High]`,
`[Severity: Medium]`, or `[Severity: Low]` on the line before the comment.

Name the exact file, function or symbol, triggering condition, and consequence.
Do not invent line numbers. Ask a concise technical question where natural,
and avoid accusations, generic checklists, or findings already disproved by
the false-positive pass.

End the report with a blank line.
