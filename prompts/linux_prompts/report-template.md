# Sashiko Prompt Review Report Template

Produce a report of prompt review findings based on this template.

- The report must be plain text without Markdown headers (#, ##) or bullet headers.
- Do NOT include email header greetings (e.g. 'On <date>, <author> wrote:') or sign-off signatures (e.g. 'Thanks,\nSashiko Review Team').
- All lines (summary, comments, questions, quoted lines) must be strictly hard-wrapped at no more than 75 characters.
- Tone must be factual, polite, conversational, and constructive.
- Format issues as an inline reply quoting the patch diff lines using '> +...' prefixes.
- Frame observations as clear technical questions or recommendations directly below the quoted hunk.
- For files in 'generic/', remember that index.md registration is not required since they are loaded by review stages.
- If no issues were found, state clearly: "Review Summary:\nNo issues found in the proposed prompt changes."
- Always end with a blank line. Do NOT append a signature, sign-off, or greeting.

Example Clean Report:
```text
Review Summary:
The proposed prompt modifications in 'api/locking.md' were reviewed across
all 4 stages. The content is factual, correctly references upstream
Linux kernel locking APIs, contains no unavailable action instructions,
and is properly placed. No regressions or issues found.
```

Example Issue Report:
```text
Review Summary:
Reviewed proposed prompt changes in 'generic/technical-patterns.md'.
Found 2 issues regarding basic C syntax and subsystem scoping.

> +- for(init; condition; advance) { body } -- checks 'condition'
> +  BEFORE executing 'body'

Is it necessary to include basic C loop syntax explanations? Prompt
guidelines specify that prompts should contain only kernel-specific
invariants and avoid basic C syntax tutorials. Please consider removing
this section.

> +- css_get() adds an additional reference

Does css_get() belong in generic/technical-patterns.md? This is an API
specific to the cgroups subsystem. Generic prompts should stay focused on
universal kernel patterns. Please consider moving this rule to
subsystems/cgroup.md.
```
