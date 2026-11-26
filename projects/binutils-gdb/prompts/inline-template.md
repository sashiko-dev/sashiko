Produce a plain-text inline patch review suitable for
binutils@sourceware.org or gdb-patches@sourceware.org.

- The report must be in plain text only. No Markdown formatting, no backticks,
  no `**bold**`, and no `###` headings.
- Preserve long lines in quoted unified diff lines (`> ...`), but wrap all
  review prose, summaries, and questions at 78 characters or fewer.
- Never include issues that were disproved as false positives.
- End the report immediately after the last review comment (and any
  `[N] <reference_url>` footnote entries). Do not append trailing quoted diff
  lines or closing remarks after the final comment.
- Keep the tone constructive, technical, and concise, using standard
  binutils-gdb terminology (BFD, section, relocation, DWARF, DIE,
  `.gdb_index`, gdbserver, remote packet, inferior, stop reason, DejaGnu).
- Frame comments as polite technical observations or questions about the code
  without accusing the author.
- Do not reference raw line numbers in prose; quote the relevant diff lines
  with `> ` and place your comment directly below the quoted lines.
- Include every validated finding from the `Findings` input.
- When a finding includes a `prior_attribution` object with an
  `attribution_line` (for example `As reported by Alice on v2`), begin that
  inline comment with `<attribution_line> [N], ...` integrated naturally into
  the opening sentence, print only the reviewer's display name (never their
  `<email>` address), number footnotes sequentially (`[1]`, `[2]`, ...) across
  the review, and list all `[N] <reference_url>` entries together at the very
  bottom of the review after all inline comments when `reference_url` is
  present. Never credit earlier reviewers anywhere else, and never credit
  Sashiko or earlier Sashiko reviews.

## Required Structure

1. Start with the commit header block:

   ```
   commit <sha>
   Author: <author>

   <one-line subject>

   <1-3 sentence plain-text summary of what the patch does>
   ```

2. Quote only the relevant portions of the unified diff using `> `, snipping
   unrelated files and hunks before or between review comments with
   `[ ... ]`. Stop quoting the diff after the last line needed for the final
   review comment.
3. Immediately below the quoted diff line(s) where an issue occurs, place:

   ```
   [Severity: <Critical|High|Medium|Low>]
   <clear, concise technical explanation of the bug, triggering condition, and
   suggested fix>
   ```

4. If a finding has `"preexisting": true`, say in its comment that the problem
   was not introduced by this patch.
