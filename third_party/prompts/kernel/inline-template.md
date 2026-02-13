Produce a report of regressions found based on this template.

- The report must be in plain text only.  No markdown, no special characters,
absolutely and completely plain text fit for the linux kernel mailing list.

- Any long lines present in the unified diff should be preserved, but
any summary, comments or questions you add should be wrapped at 78 characters

- Never include bugs filtered out as false positives in the report

- Always end the report with a blank line.

- The report must be conversational with undramatic wording, fit for sending
as a reply to the patch introducing the regression on the LKML mailing list
  - Report must be **factual**.  just technical observations
  - Report should be framed as **questions**, not accusations
  - Call issues "regressions", never use the word critical
  - NEVER EVER USE ALL CAPS

- Explain the regressions as questions about the code, but do not mention
the author.
  - don't say: Did you corrupt memory here?
  - instead say: Can this corrupt memory? or Does this code ...

- Vary your question phrasing.  Don't start with "Does this code ..." every time.

- If the bug came from SR-* patterns, it is a subjective review.  Don't put a big
  SUBJECTIVE header on it, simply say something similar to: "this isn't a bug, but ..."

- Ask your question specifically about the sources you're referencing:
  - If the regression is a leak, don't call it a 'resource leak', ask
    specifically about the resource you seek leaking.  'Does this code leak the
    folio?'
  - Don't say: 'Does this loop have a bounds checking issue?' Name the
    variable you think we're overflowing: "Does this code overflow xyz[]?"
- When the issue is in the commit message itself, quote the exact portions of
  the commit message that are incorrect, in the same way you're report a bug
  in the diff.
  - There's no need to include the diff hunks if the only issue is in the commit message.

- The issue description may include extra details such as later commits that fix 
  the bug, or lore discussions upstream.  These MUST be included in the summary,
  but should be reworded to fit the template requirements.

- You MUST include every issue sent, even if the additional details explain the
  issue was fixed in a later commit.  Your job is to format issues, not decide
  which ones are worth including.

- Do not add additional explanatory content about why something matters or what
  benefits it provides. State the issue and the suggestion, nothing more.

- Do not explain why typos or grammar mistakes are a problem. Just point them out.

## Ensure clear, concise paragraphs

**Never make long or dense confusing paragraphs, ask short questions backed up by
code snippets (in plain text), or call chains if needed.**

### AVOID
```
Can this sequence actually occur?  Looking at bt_accept_dequeue() in
af_bluetooth.c, if CPU1 already called bt_accept_unlink() which sets
bt_sk(sk)->parent = NULL, wouldn't CPU2 check parent at line 284,
detect it is NULL, and take the 'already unlinked' path with
release_sock/sock_put/goto restart instead of calling bt_accept_unlink()
again?
```

### USE INSTEAD
```
Can this sequence actually occur?  Looking at bt_accept_dequeue() in
af_bluetooth.c, if CPU1 already called bt_accept_unlink() and set
bt_sk(sk)->parent = NULL:

CPU1
bt_accept_unlink()
   bt_sk(sk)->parent = NULL;

CPU2 would see this in bt_accept_dequeue():
    if (!bt_sk(sk)->parent) {
        BT_DBG("sk %p, already unlinked", sk);
        release_sock(sk);
        sock_put(sk);
        ...
        goto restart;
    }

and take the goto restart path instead of calling bt_accept_unlink() again?
```

### AVOID

```
The commit message states the pending_release flag is used "to avoid
racing with vgic_put_irq() and causing a double-free." Is this
description accurate? Looking at vgic_put_irq(), it uses
refcount_dec_and_test() which atomically handles concurrent decrements
and returns false when refcount is already zero, so another
vgic_put_irq() call wouldn't trigger a double-free even without this
flag. The pending_release flag appears to be primarily for tracking
which LPIs need cleanup in vgic_release_deleted_lpis(), rather than
preventing races with vgic_put_irq(). Could the commit message be more
precise about what the flag actually protects against?
```

### USE INSTEAD

Dense paragraphs are hard to read.  Spread the information out so
it is easier to follow.

If you have a series of factual sentences, break them up into logical groups
with a blank line between each group.

If you have a series of statements followed by a question, put a blank line
before the question.

```
The commit message states the pending_release flag is used "to avoid
racing with vgic_put_irq() and causing a double-free." Is this
description accurate?

Looking at vgic_put_irq(), it uses refcount_dec_and_test() which atomically
handles concurrent decrements and returns false when refcount is already zero,
so another vgic_put_irq() call wouldn't trigger a double-free even without this
flag.

The pending_release flag appears to be primarily for tracking
which LPIs need cleanup in vgic_release_deleted_lpis(), rather than
preventing races with vgic_put_irq().

Could the commit message be more precise about what the flag actually protects
against?
```

## NEVER EVER ALL CAPS

The only time it is acceptable to use ALL CAPS in the review-inline.txt
is when you're directly quoting code.

### AVOID
```
SYZKALLER-1: Inaccurate race diagram in commit message

The race diagram in the commit message shows CPU2 calling
bt_accept_unlink(sk) twice, with the second call being a use-after-free:
```

### USE INSTEAD

The all caps label wasn't useful, and didn't improve the formatting of the
review:

```
The race diagram in the commit message shows CPU2 calling
bt_accept_unlink(sk) twice, with the second call being a use-after-free:
```

## Don't over explain

Some bugs are extremely nuanced, and require a lot of details to explain.

Some bugs are just completely obvious, especially cutting and pasting errors,
or areas where the author clearly just missed updating some code.   If you
expect a reasonable maintainer to understand a short explanation, use
a short explanation.

## NEVER QUOTE LINE NUMBERS

- Never mention line numbers when referencing code locations, instead indicate
the function name and also call chain if that makes it more clear.  Avoid
complex paragraphs and instead use call chains funcA()->funcB() to explain.
  - The line numbers present in the code you're reading here are unique
    to the code base setup for review.  Your audience doesn't know exactly
    what code base you're reading, so line numbers are meaningless to them.
  - YOU MUST NOT REFERENCE LINE NUMBERS IN THIS REPORT
  - Instead, use small code snippets any time you feel the urge to say a line
    number out loud.

### AVOID
```
While this should be rare since the name cache is populated by
get_cur_inode_path() called earlier in send_write(), it can happen if the
LRU cache evicts the entry (see line 2327 in __get_cur_name_and_parent) or
if initial cache storage fails (see line 2407-2409 in
__get_cur_name_and_parent).
```

### USE INSTEAD
```
While this should be rare since the name cache is populated by
get_cur_inode_path() called earlier in send_write(), it can happen if the
LRU cache evicts the entry:

fs/btrfs/send.c:__get_cur_name_and_parent() {
    ...
	nce = name_cache_search(sctx, ino, gen);
	if (nce) {
		if (ino < sctx->send_progress && nce->need_later_update) {
			btrfs_lru_cache_remove(&sctx->name_cache, &nce->entry);
			nce = NULL;
    ...
}

It can also happen if initial cache storage fails:

fs/btrfs/send.c:__get_cur_name_and_parent() {
    ...
	nce_ret = btrfs_lru_cache_store(&sctx->name_cache, &nce->entry, GFP_KERNEL);
	if (nce_ret < 0) {
		kfree(nce);
		return nce_ret;
	}
    ...
}
```

## Structure
Create a TodoWrite for these items, all of which your report should include:

- [ ] git sha of the commit
- [ ] Author: line from the commit
- [ ] One line subject from the commit
- [ ] A brief (max 3 sentence) summary of the commit.
- [ ] Any Link: tags from the commit header
- [ ] A unified diff of the commit, quoted as though it's in an email reply.
  - [ ] The diff must not be generated from existing context.
  - [ ] You must regenerate the diff by calling out to semcode's commit function,
    using git log, or re-reading any patch files you were asked to review.
  - [ ] You must ensure the quoted portions of the diff exactly match the
    original commit or patch.

- [ ] Place your questions about the regressions you found alongside the code
  in the diff that introduced them.  Do not put the quoting '> ' characters in
  front of your new text.
- [ ] Place your questions as close as possible to the buggy section of code.
- [ ] Snip portions of the quoted content unrelated to your review
  - [ ] Create a TodoWrite with every hunk in the diff.  Check every hunk
        to see if it is relevant to the review comments.
  - [ ] ensure diff headers are retained for the files owning any hunks keep
    - Never include diff headers for entirely snipped files
  - [ ] Replace any content you snip with [ ... ]
  - [ ] aggressively snip entire files unrelated to the review comments
  - [ ] aggressively snip entire hunks from quoted files if they are unrelated to the review
  - [ ] aggressively snip entire functions from the quoted hunks unrelated to the review
  - [ ] aggressively snip any portions of large functions from quoted hunks if unrelated to the review
  - [ ] ensure you only keep enough quoted material for the review to make sense
  - [ ] aggressively snip trailing hunks and files after your last review comments unless
        you need them for the review to make sense
  - [ ] The review should contain only the portions of hunks needed to explain the review's concerns.

Sample:

```
commit 06e4fcc91a224c6b7119e87fc1ecc7c533af5aed
Author: Kairui Song <kasong@tencent.com>

mm, swap: only scan one cluster in fragment list
    
<brief description>

> diff --git a/mm/swapfile.c b/mm/swapfile.c
> index b4f3cc7125804..1f1110e37f68b 100644
> --- a/mm/swapfile.c
> +++ b/mm/swapfile.c

[ ... ] <-- only if you've snipped text

> @@ -926,32 +926,25 @@ static unsigned long cluster_alloc_swap_entry(struct swap_info_struct *si, int o
>  		swap_reclaim_full_clusters(si, false);
>  
>  	if (order < PMD_ORDER) {
> -		unsigned int frags = 0, frags_existing;
> -
>  		while ((ci = isolate_lock_cluster(si, &si->nonfull_clusters[order]))) {
>  			found = alloc_swap_scan_cluster(si, ci, cluster_offset(si, ci),
>  							order, usage);
>  			if (found)
>  				goto done;
> -			/* Clusters failed to allocate are moved to frag_clusters */
> -			frags++;
>  		}
>  
> -		frags_existing = atomic_long_read(&si->frag_cluster_nr[order]);
> -		while (frags < frags_existing &&
> -		       (ci = isolate_lock_cluster(si, &si->frag_clusters[order]))) {
> -			atomic_long_dec(&si->frag_cluster_nr[order]);
                        ^^^^

Is it ok to remove this atomic_long_dec()?  It looks like the counter
updates are getting lost.

<any additional details sent when the prompt was executed>

<any additional details from the code required to support your question>


> -			/*
> -			 * Rotate the frag list to iterate, they were all
> -			 * failing high order allocation or moved here due to
> -			 * per-CPU usage, but they could contain newly released
> -			 * reclaimable (eg. lazy-freed swap cache) slots.
> -			 */
> +		/*
> +		 * Scan only one fragment cluster is good enough. Order 0
> +		 * allocation will surely success, and large allocation
> +		 * allocation will surely success, and large allocation
                 ^^^^^^^^ this isn't a bug, but you've duplicated this line

> +		 * failure is not critical. Scanning one cluster still
> +		 * keeps the list rotated and reclaimed (for HAS_CACHE).
> +		 */
> +		ci = isolate_lock_cluster(si, &si->frag_clusters[order]);
> +		if (ci) {
>  			found = alloc_swap_scan_cluster(si, ci, cluster_offset(si, ci),
>  							order, usage);
>  			if (found)
>  				goto done;
> -			frags++;
>  		}
>  	}
>  
```

Sample commit message issue:

In this case, we keep the header and the summary of the commit and then
directly quote the part of the commit message that are incorrect.

```
commit 535a36aad18ce99e3270486fdb073bb5eb1f1c59
Author: SeongJae Park <sj@kernel.org>

Docs/mm/damon/maintainer-profile: fix wrong MAITNAINERS section name

This commit fixes the documentation to reference the correct MAINTAINERS
section name after commit 9044cbe50a70 renamed the DAMON section from
"DATA ACCESS MONITOR" to "DAMON".

Link: https://lkml.kernel.org/r/20260118180305.70023-8-sj@kernel.org

> Docs/mm/damon/maintainer-profile: fix wrong MAITNAINERS section name

This isn't a bug, but there's a typo (MAITNAINERS) in the subject line.
```
