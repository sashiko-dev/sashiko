# Sashiko Prompt Review Report Template

Produce a report of prompt review findings based on this template.

- The report must be plain text.
- Summary and comments should be wrapped at 78 characters.
- Tone must be factual, polite, and constructive.
- Frame observations as clear technical questions or recommendations.
- Cite specific files, line numbers (if available), and prompt snippets.
- Group findings logically:
  1. Factual & Actionability Constraints (Stage 1)
  2. Kernel Source Codebase Verification (Stage 2)
  3. Index & Placement Consistency (Stage 3: api/, subsystems/, generic/, index.md)
- If no issues were found, state clearly that the prompt changes meet all Sashiko quality and architectural standards.
- Always end with a blank line.

Example Clean Report:
```text
Review Summary:
The proposed prompt modifications in 'api/locking.md' were reviewed across
all 4 stages. The content is factual, correctly references upstream Linux
kernel locking APIs, contains no unavailable action instructions, and is
properly indexed in index.md. No regressions or issues found.
```

Example Issue Report:
```text
Review Summary:
Reviewed proposed prompt changes in 'subsystems/mutex_usage.md'.
Found 2 issues regarding actionability constraints and folder placement.

> +Make sure to compile the kernel with 'make -j32' and verify that no
> +warnings are emitted by gcc.

Can this instruction be fulfilled by Sashiko? Sashiko operates in an
autonomous review environment with read-only git tools, and cannot compile
or execute code. Consider rephrasing this instruction into a static pattern check:
"Check that all variable declarations and struct initializers conform to
standard kernel compilation standards without missing initializers."

> [subsystems/mutex_usage.md]

This file describes caller usage rules for mutexes rather than internal
mutex implementation mechanics. Caller guidelines belong under 'api/locking.md',
whereas 'subsystems/locking.md' should be reserved for the internal locking
subsystem implementation. Additionally, ensure the corresponding entry in
'index.md' points to the 'api/' path.
```
