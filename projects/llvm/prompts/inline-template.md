Produce inline review comments suitable for a GitHub pull request on
`llvm/llvm-project`.

- The report is Markdown. Short fenced code blocks are fine; use only `###`
  headings (no `#`, `##`, or `####` and deeper) and do not produce tables.
- Reference the code as `path:line` (for example
  `clang/lib/Sema/SemaExpr.cpp:7933`) and quote only the lines that matter.
- Keep each comment to a few sentences: what is wrong, the input that
  triggers it, and the suggested fix. Longer analysis belongs in the summary.
- Never include findings that were disproved as false positives.
- Use LLVM terminology (`CastKind`, `QualType`, `Sema`, `CodeGen`,
  `llvm_unreachable`, `poison`, `undef`, `atomicrmw`, `FileCheck`,
  `-verify`), and frame comments as technical observations, not as
  accusations.
- Do not cite line numbers that the patch does not touch.
- Do not mention Sashiko or earlier automated reviews.

## Required structure

1. A one-paragraph summary: what the patch does, whether the approach is
   sound, and how many findings follow.
2. One bullet per finding, most severe first:

   ```
   - [Severity: <Critical|High|Medium|Low>] `path:line`: <one-line summary>
   ```

3. For every finding, an inline-style block:

   ```
   [Severity: <Critical|High|Medium|Low>]
   <What the code does now and what it should do. Include the smallest
   reproducer (source + flags) and the expected vs actual behaviour, then
   the suggested fix.>
   ```

4. If any finding has `"preexisting": true`, say explicitly at the start of
   its comment that the problem was not introduced by this patch.
5. If nothing survived verification, end with a single sentence saying so and
   do not pad the report.
