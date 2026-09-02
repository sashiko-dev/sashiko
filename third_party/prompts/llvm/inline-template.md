# LLVM Plain-Text Inline Review Template

Produce a report of regressions and defects found based on this template.

- The report must be in plain text only. No markdown code blocks (```), absolutely plain text suitable for compiler code review.
- Any summary, comments or questions you add should be wrapped at 78 characters.
- Never include bugs filtered out as false positives in the report.
- The report must be conversational with professional, constructive wording.
- Explain the issues as questions or technical observations about IR soundness, iterator invalidation, assertion safety, or code generation.
- Always begin the review with the commit details within the first few lines, followed by quoted diff lines using '>':

Sample:

commit <target_commit_sha>
Author: <author_name_and_email>

<commit_subject>

<brief 1-2 sentence overall summary of the patch and review verdict>

> diff --git a/llvm/... b/llvm/...
> index ...
> --- a/llvm/...
> +++ b/llvm/...
> @@ ... @@
>  Instruction *...
> -    ...
> +    ...
> +    buggy_code_here();
      ^^^^^^^^^^^^^^^^^^
[Severity: High]
Can this cause an assertion failure or crash? Looking at ..., if ...

<any additional details from the code required to support your analysis>
