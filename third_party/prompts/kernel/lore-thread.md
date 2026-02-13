# Lore Thread Analysis for Kernel Patch Reviews

## Purpose

Kernel patches are discussed on lore.kernel.org. Developers often fail to address
comments from one version to another. This guide explains how to effectively
process lore threads to identify unaddressed review feedback.

You're doing a deep dive analysis, which requires carefully searching lore
indexes for past review comments.  You must not skip any part of this prompt,
for any reason.

The output of this patch review will be used by maintainers when deciding if a
given patch is ready to include.  It's important that we make sure
all email comments on the patches are addressed, especially those from maintainers.

## Step 1: Find All Versions

Use `dig` to find emails related to the commit:
```
dig(commit="HEAD", show_all=true)
```

From the results, identify:
- Patch submissions from the author (different versions: v1, v2, v3, etc.)
- Review replies from maintainers/reviewers
- Author responses to reviews

## Step 2: Process Large Threads Efficiently

Lore threads can be very large. Do NOT fetch entire threads with `show_thread=true`.

### Correct approach:

1. **List reviewer reply Message-IDs** from the dig results
   - Look for "Re:" emails from people other than the patch author
   - Note the Message-ID for each reviewer comment

2. **Fetch individual review emails** without thread context:
   ```
   lore_search(message_id="<id>", verbose=true)  # NO show_thread
   ```
   This returns just that one email, making it manageable.

3. **Fetch author responses separately**:
   ```
   lore_search(message_id="<author-reply-id>", verbose=true)
   ```

4. **Compare review comment with author response** to determine if addressed.

### If output is still too large:

Use targeted extraction with jq and grep:
```bash
# Extract specific email from JSON output
jq -r '.[] | .text' file.txt | awk '/Message-ID: <specific-id>/,/--- End Message ---/'

# Find review patterns (quoted code + reviewer commentary)
jq -r '.[] | .text' file.txt | grep -B2 -A10 "^   >"
```

## Step 3: Identify Review Comments

Review comments typically appear as:
- Quoted patch code (lines starting with `> +` or `> -` or `>  `)
- Followed by reviewer text (not starting with `>`)
- Keywords: "nit:", "please", "should", "instead", "why", "consider", "missing"

## Step 4: Track Comment Resolution

For each review comment found:

- Add each review comment to TodoWrite

| Comment | Addressed in reply? | Addressed in next version? | Status |
|---------|---------------------|---------------------------|--------|
| ... | Yes/No | Yes/No | Resolved/Unaddressed |

## Step 5: Verify Against Current Code

For comments marked as "will fix" or "Ack" by author:

- Add each comment to TodoWrite
- Check if the fix actually appears in the current HEAD commit
- Strictly verify the fix is correct, try to disagree with any comments
  or commit messages as though you are the reviewer.  Loosen the false-positive-guide.md
  rules around trusting the author and bias toward reporting incomplete, incorrect, or
  partial fixes.
- Search for the reviewer in the MAINTAINERS file.  If present, consider any unaddressed
  request or partially addressed comment a regression, even if it is only a
  style suggestion or does not fix a bug.  Consider these through a pedantic analysis lens.
  - If it's not a bug, just label it correctly as a requested change.
  - Don't use the word maintainer, just name the person that asked for the change.
- Search for the author in the MAINTAINERS file.  If present, trust their expertise more
- Output:
```
comment <comment>
Addressed <yes/no>
Expected response from original reviewer: <fix is sufficient / fix is not sufficient>
```
- If you expect the original reviewer would not find the fix sufficient, consider
this a potential regression
- Author agreement alone does not mean the fix was implemented
- Do not complete the TodoWrite entry until each comment has output described above

## Step 6: MANDATORY Final validation

```
Found older version: <date> <version> <subject>
Found older version: <date> <version2> <subject>

FINAL UNADDRESSED COMMENTS: NUMBER
Original reviewer expected responses to new patch
```

If unaddressed comments exist, include lore links in review-inline.txt:
```
https://lore.kernel.org/bpf/<message-id>/
```

- Did you output analysis of each prior review comment as required? [ y / n]
- If there are any unaddressed TodoWrite entries, YOU MUST GO BACK AND CHECK THEM
