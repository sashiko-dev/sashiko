Produce a report of regressions found based on this template.

## The governing rule

Every line and every word you write must do one of two jobs:

- help the reader understand the problem: what is wrong, where, what
  triggers it, and what happens, or
- show why it is worth fixing: the consequence and impact that the
  finding establishes.

Delete anything that does neither: restating the quoted diff,
paraphrasing the commit message, proof steps the reader does not need,
generic remarks about a class of bug, alternative paths that add no new
insight, lists of alternative fixes, and closing summaries.

The findings have already been verified and rated by earlier stages.
Do not re-judge them here: do not downplay, upgrade, or question whether
a finding matters.  Convey what the finding says, clearly.

The reader is the patch author or a subsystem maintainer who knows this
code better than you do.  Following the rule above, an obvious bug gets
a one or two line comment, and a subtle race gets a diagram and a couple
of paragraphs.
Length follows from the rule above, never from a quota, and comments in
one report are expected to differ a lot in length.

## Format

- Plain text only, fit for the linux kernel mailing list.  No markdown,
  no backticks, no special characters.
- The ``` lines in this file only mark where examples start and end.
  Never output them.
- Preserve long lines in quoted diff and quoted code, but wrap your own
  text at 78 characters.
- Never use ALL CAPS, except in quoted code, CPU labels and lockdep's
  "*** DEADLOCK ***" line.  Do not add labels such as "SYZKALLER-1:" or
  "SUBJECTIVE:" in front of comments.
- Never mention line numbers.  They refer to a tree the reader does not
  have.  Use function names, call chains (funcA() -> funcB()) and short
  code snippets instead.
- Your own text, snippets and diagrams start at the left margin, without
  a "> " prefix.
- Refer to commits the way kernel developers do.  For a commit with a
  stable sha (merged upstream or in a maintainer tree), write the 12
  character short sha followed by the subject in parentheses and double
  quotes: 1a2b3c4d5e6f ("foo: fix bar handling").  For a commit whose sha
  is not stable, such as an earlier patch in the series under review,
  write commit "foo: add bar helper" with the subject only.  Never write a
  bare sha or a sha without its subject, and never guess a subject: look
  it up with the git tools.
- End the report immediately after your last review comment.  No
  trailing quoted diff, no [ ... ] marker and no closing remarks after it.

## Tone

- Conversational, factual and undramatic, fit for a reply on LKML.
- Frame issues as questions about the code, not accusations, and do not
  address the author:
  - don't say: Did you corrupt memory here?
  - instead say: Can this corrupt memory?  or  Does this code ...
- Vary the phrasing; do not start every comment with "Does this code".
- Call bugs introduced by the patch "regressions".  Never use the word
  critical in prose; it only appears in the severity tag.
- If the finding came from SR-* patterns it is a subjective review.  Say
  so lightly, for example "this isn't a bug, but ...".

## Writing a comment

Write each comment for a reader who will look at the quoted diff next to
it.  By the end of the comment that reader must be able to tell what is
wrong, where, what makes it happen, and why it is worth fixing.  This is
a test the comment must pass, not a list of
sentences to write: often the question alone passes it.

- Lead with a question that names the concrete object: "Does this leak
  the folio?", not "is there a resource leak?"; "Can this overflow
  xyz[]?", not "is there a bounds checking issue?".  Subjective remarks,
  typos and missing tags may be plain statements.
- Add a sentence only if the reader would otherwise have to work
  something out for themselves: a trigger that is not visible in the
  quoted diff, or a consequence that is not obvious from the question.
- Give one trigger, the most likely or most convincing one, not a list of
  every input that hits the bug.
- Give the consequence that matters most, not every object or code path
  affected.
- Add evidence from outside the quoted diff only when the reader could
  not check the claim without it, and then only the smallest piece that
  settles it (see Evidence below).  Many comments need none.
- State the consequence in prose.  A snippet supports the text; it does
  not replace it, so never end a comment on a snippet that is left to
  speak for itself.
- When the fix is not obvious, suggest one in a single sentence.

How much text a finding needs depends on how hard it is to explain, not
on how serious it is.  A missed unlock or a wrong constant is one
question, perhaps with a short sentence on the effect.  A race needs a
scenario diagram and a paragraph.  Both can be critical.

If a finding contains several distinct problems, keep each of them; put
the secondary ones after the main one, one short sentence each.  Never
fold two findings into one comment and never drop a finding.

## Evidence

### Code snippets

Quote code only when it shows something prose cannot say as briefly.
Never quote code to show a fact that fits in one sentence: that
something is absent ("this table entry has no .foo field"), or that an
object lives on the stack ("foo_cmd() allocates ctl on the stack").
Never quote a whole function when two lines carry the point. When a
snippet is needed, quote just the lines that matter, labelled with the
file and function, instead of describing them or citing line numbers.
This evidence fragment shows the format:

AVOID:

```
While this should be rare since the name cache is populated by
get_cur_inode_path() called earlier in send_write(), it can happen if the
LRU cache evicts the entry (see line 2327 in __get_cur_name_and_parent) or
if initial cache storage fails (see line 2407-2409 in
__get_cur_name_and_parent).
```

USE INSTEAD:

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
```

### Scenario diagrams for concurrency

Every race, deadlock or reentrancy finding where the order of events
across contexts matters is shown the way kernel developers draw it on
LKML.  Prose alone ("Meanwhile, the work item dereferences ... while
concurrently ...") is hard to follow.

- Time flows downward, one step per line.
- Each CPU or context gets its own column, labelled CPU0, CPU1, ... or
  with the actual context (kworker, <hardirq>, <softirq>).
- When everything happens on one CPU (an interrupt, softirq or
  preemption nesting into the same code), use a single column with no
  CPU label and no header, and mark where the nesting happens.
- Keep only the steps a reader needs to see the interleaving: the entry
  point of each context, the key steps (lock, publish, clear, free) and
  the bad access.  Collapse intermediate callers with "...".
- One diagram shows one concrete interleaving.  No alternatives inside
  it; describe other outcomes in the prose.
- Mark the failing step with a short arrow note such as
  "<-- use-after-free" or "<-- spins forever".  Any note inside a
  diagram is one short line; explanations go in the prose below.
- Every line fits in 78 columns.  Elide arguments with "..." and put the
  arrow on its own line when needed.

Skip the diagram when it adds nothing over one sentence: a hang where one
side just waits for a slow or stuck peer, a symmetric race where every
context runs the same code ("two concurrent callers both reach X"), or a
single-context problem such as sleeping in atomic context, where a call
chain is enough.

Two contexts racing on an object's lifetime:

```
CPU0                              CPU1
foo_remove()
  cancel_work(&foo->work)
                                  foo_irq_handler()
                                    schedule_work(&foo->work)
  kfree(foo)
                                  foo_work_fn()
                                    foo->state = 0;  <-- use-after-free
```

An ABBA deadlock, in lockdep's format, followed by the real call paths:

```
CPU0                              CPU1
----                              ----
lock(&dev->mutex)
                                  lock(&bar_mutex)
lock(&bar_mutex)
                                  lock(&dev->mutex)

 *** DEADLOCK ***

CPU0: foo_unregister() -> bar_unregister()
CPU1: bar_sync_work() -> bar_set_state() -> foo_set_state()
```

One CPU interrupted while holding something:

```
foo_update()
  write_seqcount_begin(&p->seq)
  <hardirq>
    foo_complete()
      ...
        read_seqcount_begin(&p->seq)  <-- spins forever
```

## Paragraphs and wording

- Short sentences, one idea each.  Paragraphs of at most four lines.  Put
  a blank line between groups of facts, and before the question when
  facts lead up to it.
- Use established terms: use-after-free, double free, leak, ABBA
  deadlock, NULL pointer dereference, data race.  Avoid "can lead to a
  potential situation in which".
- One "can" or "could" in the question is enough; state the explanation
  as facts and do not stack hedges.
- Refer to each code element by one name and keep using it.

AVOID:

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

USE INSTEAD:

```
Is the commit message accurate about what pending_release protects
against?

vgic_put_irq() uses refcount_dec_and_test(), so a second vgic_put_irq()
cannot double free even without this flag.  The flag seems to track which
LPIs vgic_release_deleted_lpis() must clean up.
```

## Special cases

- Commit message issues.  Keep the commit and Author header lines, then:
  - for a missing, malformed or inaccurate trailer (such as Fixes:) or a
    problem with the message as a whole, quote the entire commit message
    as the first quoted block, the way a normal mailing list reply does.
    Fixes: tag issues use [Severity: Low].  The quote already contains
    the subject, so no separate subject or summary is needed.
  - for a specific wrong sentence or typo, keep the subject, summary and
    Link: lines as usual, then quote only the incorrect part.
  Put the question after the quoted text.  No diff hunks are needed when
  the only issue is in the commit message.

- Extra details in a finding, such as a later commit that fixes the bug
  or an upstream discussion, must be mentioned, in one sentence.

- Include every finding you were given, even if its details say it was
  fixed later.  Your job is to format findings, not to decide which are
  worth reporting.  Never include findings filtered out as false
  positives.

- Pre-existing findings and prior-version attribution are covered in the
  instructions above.  Never credit earlier reviewers unless the finding
  has a prior_attribution with an attribution_line, and never credit
  Sashiko or earlier Sashiko reviews.  When both apply to one finding,
  the attribution comes first: "As reported by Jane Doe on v2 [1], this
  wasn't introduced by this patch, but ...".

## Structure

Create a note for these items; your report must include all of them, except
where Special cases say otherwise:

- [ ] The commit header, in the form git show prints it:
  - [ ] First line: the word commit, one space, then the full git sha.
    No colon; commit is the only header line without one.  Write
    "commit 06e4fcc9...", never "commit: 06e4fcc9...", "commit:06e4fcc9..."
    nor "Commit 06e4fcc9...".
  - [ ] Second line: "Author: $Name <$email>", taken from the commit.
- [ ] The one line subject of the commit.
- [ ] One or two sentences on what the commit does.  Describe it; do not
  evaluate it.
- [ ] Any Link: tags from the commit.
- [ ] The unified diff of the commit, quoted as in an email reply.
  - [ ] Obtain the diff with git tools such as git_show, or by re-reading
    the patch files you were asked to review.  Never reconstruct it from
    context.
  - [ ] The quoted portions must exactly match the original commit.
- [ ] Never write a question before the first quoted diff or commit
  message block.
- [ ] Each comment sits right below the quoted code that introduced the
  problem.
- [ ] A severity tag on its own line right above each comment, exactly
  "[Severity: <level>]" where <level> is Critical, High, Medium or Low.
- [ ] Snip everything the comments do not need:
  - [ ] Check every hunk for relevance to the comments.
  - [ ] Keep diff headers only for files that keep at least one hunk.
  - [ ] Drop whole files, hunks, functions and parts of long functions
    that are unrelated to the comments.
  - [ ] Replace snipped content before or between comments with [ ... ].
  - [ ] Keep only enough quoted material for each comment to make sense.
  - [ ] Nothing after the last comment.

Sample (a made-up driver), with placeholders in angle brackets:

```
commit 0123456789abcdef0123456789abcdef01234567
Author: Jane Doe <jane.doe@example.com>

foo: cache the device lookup in foo_open()

<one or two sentences on what the commit does>

> diff --git a/drivers/misc/foo.c b/drivers/misc/foo.c
> index 1a2b3c4d5e6f..6f5e4d3c2b1a 100644
> --- a/drivers/misc/foo.c
> +++ b/drivers/misc/foo.c

<[ ... ] only if you snipped text here>

> @@ -120,9 +120,20 @@ static int foo_open(struct inode *inode, struct file *file)
>  {
>  	struct foo_dev *dev = container_of(inode->i_cdev, struct foo_dev, cdev);
> +	struct foo_cache *cache;
>
>  	mutex_lock(&dev->lock);
> +	cache = kzalloc(sizeof(*cache), GFP_KERNEL);
> +	if (!cache)
> +		return -ENOMEM;

[Severity: High]
Does this return leave dev->lock held when kzalloc() fails?  Every later
foo_open() would then block forever on dev->lock.

[ ... ]

> +	/* Cache the the lookup result for later calls. */

[Severity: Low]
This isn't a bug, but "the the" is doubled in this comment.
```

Sample commit message issue with a specific wrong part:

```
commit 535a36aad18ce99e3270486fdb073bb5eb1f1c59
Author: SeongJae Park <sj@kernel.org>

Docs/mm/damon/maintainer-profile: fix wrong MAITNAINERS section name

This commit fixes the documentation to reference the correct MAINTAINERS
section name after commit 9044cbe50a70 renamed the DAMON section.

Link: https://lkml.kernel.org/r/20260118180305.70023-8-sj@kernel.org

> Docs/mm/damon/maintainer-profile: fix wrong MAITNAINERS section name

[Severity: Low]
This isn't a bug, but there's a typo (MAITNAINERS) in the subject line.
```

Sample commit message issue about the message as a whole (a missing
Fixes: tag):

```
commit 533c6d48ce4a00a5deb5eea6c921b5d019838b5e
Author: Yonghong Song <yonghong.song@linux.dev>

> bpf: Add precision marking and backtracking for stack argument slots
>
> Extend the precision marking and backtracking infrastructure to
> support stack argument slots (r11-based accesses). Without this,
> precision demands for scalar values passed through stack arguments
> are silently dropped, which could allow the verifier to incorrectly
> prune states with different constant values in stack arg slots.
>
> Signed-off-by: Yonghong Song <yonghong.song@linux.dev>

[Severity: Low]
This looks like a bug fix for stack argument support.  Should this
include:

  Fixes: 3330ee8db989 ("bpf: Support stack arguments for bpf functions")
```
