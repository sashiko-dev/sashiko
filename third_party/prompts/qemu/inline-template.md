# QEMU Plain-Text Inline Review Template

Produce a report of regressions and defects found based on this template.

- The report must be in plain text only. No markdown code blocks (```), absolutely and completely plain text fit for the QEMU mailing list (qemu-devel@nongnu.org).
- Any summary, comments or questions you add should be wrapped at 78 characters.
- Never include bugs filtered out as false positives in the report.
- The report must be conversational with professional, constructive wording, fit for sending as an inline email reply to the patch on qemu-devel.
- Call issues "regressions" or "potential defects", avoid dramatic phrasing.
- Explain the issues as questions or technical observations about the virtual hardware, QOM, memory/DMA, or locking models.
- Always begin the review with the commit details within the first few lines, followed by quoted diff lines using '>':

Sample:

commit <target_commit_sha>
Author: <author_name_and_email>

<commit_subject>

<brief 1-2 sentence overall summary of the patch and review verdict>

> diff --git a/hw/... b/hw/...
> index ...
> --- a/hw/...
> +++ b/hw/...
> @@ ... @@
>  static void ...
> -    ...
> +    ...
> +    buggy_code_here();
      ^^^^^^^^^^^^^^^^^^
[Severity: High]
Does this allow a guest to trigger an out-of-bounds access? Specifically, when
the guest executes ...

<any additional details from the code required to support your analysis>
