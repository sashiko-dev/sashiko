# Structured Code Review

Produce a report of regressions found as a JSON array. Each object in the array must follow this schema:

```json
{
  "file": "path/to/file.c",
  "compromised_line": "exact code snippet from the file containing the issue",
  "approx_line": 42,
  "issue": "A conversational, factual, and concise description of the regression, framed as a question if possible."
}
```

## Constraints
- **JSON Only**: Your entire response must be a single valid JSON array. Do not include any text outside the JSON.
- **Exact Snippets**: The `compromised_line` must match the code in the provided diff exactly (ignoring leading/trailing whitespace).
- **No Line Numbers in Text**: Do not mention line numbers in the `issue` field.
- **Conversational Tone**: Use undramatic, technical observations. Frame issues as questions (e.g., "Can this corrupt memory?" instead of "You corrupted memory here").
- **Concise**: Do not over-explain. State the issue and the suggestion, nothing more.
- **Factual**: Only report technical observations.
- **Aggressive Filtering**: Never include bugs filtered out as false positives.

## Schema Details
- `file`: The relative path to the file being reviewed.
- `compromised_line`: A string containing the exact lines of code where the issue resides. If the issue spans multiple lines, include all of them.
- `approx_line`: (Optional) An approximate line number to help locate the snippet.
- `issue`: The review comment. Follow the conversational and stylistic guidelines of the Linux kernel community.
