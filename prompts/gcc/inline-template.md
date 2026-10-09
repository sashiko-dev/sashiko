Produce a plain-text inline patch review suitable for `gcc-patches@gcc.gnu.org`.

- The report must be in plain text only. No Markdown formatting, no backticks, no `**bold**`, and no `###` headings.
- Preserve long lines in quoted unified diff lines (`> ...`), but wrap all review prose, summaries, and questions at 78 characters or fewer.
- Never include issues that were disproved as false positives.
- Always end the report with a blank line.
- Keep the tone constructive, technical, and concise, using standard GCC terminology (`SSA_NAME`, `GIMPLE`, `RTL`, `match.pd`, `wide_int`, `poly_int`, `error_mark_node`, `tf_warning_or_error`, `wrong-code`, `ICE`, `rejects-valid`).
- Frame comments as polite technical observations or questions about the code without accusing the author.
- NEVER use ALL CAPS except when writing standard GCC macros/identifiers (`NULL_TREE`, `SSA_NAME`, `INTEGER_CST`, `RANGE_EXPR`, `UNKNOWN`, `BIND(C)`).
- Do not reference raw line numbers in prose; quote the relevant diff lines with `> ` and place your comment directly below the quoted lines.
- Include every validated finding from the `Findings` input.

## Required Structure

1. Start with the commit header block:
   ```
   commit <sha>
   Author: <author>

   <one-line subject>

   <1-3 sentence plain-text summary of what the patch does>
   ```
2. Quote only the relevant portions of the unified diff using `> `, snipping unrelated files and hunks with `[ ... ]`.
3. Immediately below the quoted diff line(s) where an issue occurs, place:
   ```
   [Severity: <Critical|High|Medium|Low>]
   <clear, concise technical explanation of the bug, triggering condition, and suggested fix>
   ```
