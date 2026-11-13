# Code history review stage

## Problem

The review stages judge a patch against the current source tree. None of
them is told to read the history of the code a patch changes, so a review
cannot see that a removed check was added to fix a reported bug, that the
same change was merged and reverted before, or that a design choice was
argued for on the mailing list. The only history a review reads is the
`git blame` candidates in `<fixes_context>`, and those serve the `Fixes:`
trailer check alone. Projects without generated code notes, such as QEMU,
have no other source for this knowledge.

## Resolution

A deterministic collector, `src/workflows/code_history.rs`, reads the
history of the code a patch changes and, when the build has the `server`
feature, the mailing list threads where that history was reviewed. A
project's review workflow opts in by adding a `history` analysis stage that
receives both as `<history_context>` and `<history_discussions>` blocks and
reports concerns that the rest of the pipeline verifies like any other.

QEMU is the first project with the stage. Shared code gains one state field
and one call site; the lore client, mbox splitting, and mail parsing are
reused unchanged.

## Project profiles

What counts as code and where patches were reviewed differ per project, so
`collect_code_history` takes a `ProjectId` and reads its `HistoryProfile`:

- `skipped_dirs` and `skipped_files`: paths whose history is not collected.
  Tests, documentation, and build files change with nearly every feature
  and would crowd out the history of the code itself.
- `discussions`: an optional `DiscussionSource`. `Lore { lists }` fetches
  threads from lore.kernel.org; `lists` names the archives a `Link:` trailer
  may point at.

`HistoryProfile::for_project` returns a profile only for projects whose
workflow has a `history` stage, and the collector does nothing for the
rest. Enabling another project means adding its profile, its stage, and, if
its reviews are not on lore, a new `DiscussionSource` variant.

| Project | Skipped paths | Discussions |
|---|---|---|
| QEMU | `tests/`, `docs/`, `meson.build`, `Kconfig`, `MAINTAINERS`, `meson_options.txt` | lore: `qemu-devel`, `qemu-riscv`, `qemu-arm`, `qemu-rust`, `qemu-trivial` |

## History collection

All git commands run in the review worktree through `git_cmd::in_dir_async`
with a timeout and a capped stdout, against the parent of the target commit,
so line numbers in the diff match the history being read. The target must
be an object name (7 to 64 hexadecimal digits), so it cannot be read as a
git option; every other revision comes from git output. Commits of the
series under review (`FixesAnalysis::series_commit_shas`) are excluded.

1. Modified regions: for every `HunkBlameTarget` from
   `fixes_check::extract_diff_blame_targets` on a code path, `git log -L
   s,e:file --no-patch -n 6`.
2. Removed lines: for each run of contiguous deleted lines, at most 12 runs
   per patch, `git log -L a,b:file --no-patch -n 3`, which names the
   commits that last wrote them.
3. Removed identifiers: identifiers that appear only on deleted lines and no
   longer occur in the modified files at the target commit (`git grep -w`),
   at most three, each resolved to the commit that introduced it with
   `git log -S<ident> --reverse`.
4. Fix and revert chains: for the 12 highest-ranked commits found above,
   `git log --grep=<8-char sha> <sha>..<parent>`. A result whose message has a
   `Fixes:` line naming the commit is recorded as a later fix; one whose
   message says `This reverts commit <sha>` is recorded as a revert.

Commits are ranked by relation (removed-line and removed-identifier origins
first, then commits later fixed or reverted, the fixing and reverting
commits, and other region history), then by date. At most 12 are kept, each
with its full message (2 KiB) and its diff restricted to the modified files
(1.5 KiB); the block is capped at 24 KiB. Truncation happens on UTF-8
boundaries and is stated in the output. Steps 1 to 4 stop after two
minutes and keep the commits found so far. Object names of 40 (SHA-1) or
64 (SHA-256) hexadecimal digits are accepted.

`CodeHistoryContext` lists only the commits the block shows, with their
subjects as git reports them, and discussions are fetched only for those.

Insertions into an existing code file are traced through the lines around
them. When the patch touches no existing code file outside the profile's
skipped paths, or git finds no history, the collector returns nothing and
the stage does not run.

## Discussion collection (`server` feature, `Lore` source)

For the four highest-ranked commits whose relation is a removed-line or
removed-identifier origin, a later-fixed commit, or a reverted commit:

1. The Message-ID comes from the commit's `Message-ID` trailer, read with
   `git log -1 --format=%(trailers:key=Message-ID,valueonly)`, or from a
   `Link:` trailer into one of the profile's archives (or `r`/`all`),
   parsed with `reqwest::Url`.
2. With a Message-ID, `LoreMboxClient::fetch_thread` returns the thread of
   the merged version.
3. `LoreMboxClient::search_patch_id` with the commit's stable patch ID
   (`git patch-id --stable`) returns every thread carrying the same diff,
   which adds the earlier versions of the patch and covers commits without
   trailers. A diff that reached the git read limit is not searched,
   because a cut diff has a different patch ID.

The mails are split with `split_mbox` and parsed with `patch::parse_email`
inside `spawn_blocking`, since a search can return several MiB.
Each mail is a posting (`is_patch_or_cover` with a diff), a cover letter
(`is_patch_or_cover` without one, whether top level or sent in reply to an
earlier version), or a reply (neither, even when it quotes or suggests a
diff). A mail is kept when its subject, cleaned with `patch::clean_subject`,
equals the commit subject and its tags do not include `PULL`; the cover
letter a kept posting replies to is kept as well. Identical bodies are kept once.
From postings only the notes below the `---` line are kept, without the
diffstat; cover letters are cut at the shortlog; replies lose the
signature, the quote attribution, and quoted text except the three
non-empty quoted lines before each reply paragraph, and replies that only
carry `Reviewed-by`/`Acked-by`/`Tested-by` tags are folded into one line.
Each thread is capped at 6 KiB and all discussions at 16 KiB, and only the
Message-IDs whose header survives the cut can be cited.

Requests run one at a time with at least one second between them. A thread
is cached per project and commit under
`$XDG_DATA_HOME/sashiko/code-history/v1/<project>/`, because the discussion
of a merged commit does not change, but only when every request behind it
succeeded: lore answers a patch ID search without a match with an error
status that the client does not tell apart from a failure. Entries are
written to a temporary file and renamed into place. A failed request is
logged, and the stage continues with what was fetched.

Commit messages, diffs, and mail are untrusted text. Every tag-shaped
token in them is escaped with `prior_context::sanitize_untrusted_discussion_text`
before they reach a prompt, so none can close the block it is quoted in,
and the stage prompt tells the model to treat the mail as data.

## QEMU stage

`history` is a core `AnalysisStage` of `qemu_patch_review`, like `goal`,
`implementation`, and `execution-flow`: it runs on every review, with the
commit message in its system prompt and the series context in its user
prompt. It is left out of the run only when the collector returned
nothing: the patch touches no existing code file outside the profile's
skipped paths, or git finds no history for the touched lines. Leaving it
out, rather than skipping it, keeps a failure of the stages beside it
from being counted against a skipped success. In cheap mode it runs
beside the combined stage, because the combined stage cannot carry the
history blocks.

The instruction asks the model to act as the long-term maintainer of the
modified code. For a fix it checks whether the fix covers the earlier
occurrences of the same defect and keeps earlier fixes intact; for a feature
it checks whether the patch removes a check or special case whose history
states why it exists, repeats a reverted change, or breaks a compatibility
constraint recorded in history. It reports only defects the history makes
visible; Fixes trailers, commit message wording, comments, tests, and style
stay with the other stages, and this scope overrides the shared directive to
report every pre-existing bug spotted.

Every concern carries a `history_category` (`removed_safeguard`,
`reverted_approach`, `incomplete_fix`, `recorded_constraint`,
`revert_regression`, or `other`), a `history_dependence` naming the
historical fact it relies on, and a `historical_commits` array of
`{sha, subject, relation, discussion_msgid}` objects; a cited Message-ID
must be one shown in `<history_discussions>`. A dedicated validator
enforces these on top of the common QEMU concern validator, which Zephyr
shares and which stays unchanged.

When the stage reports, each cited commit is grounded in the rendered
history: a commit the block showed gets its SHA and subject from git, any
other commit gets a null subject, and the concern records
`cites_commits_outside_history_context`. Verification routes such
concerns, and concerns resting on mail alone, to post-verification with
`signal_reason` `insufficient_evidence`. After verification and each
post-verification batch, the grounded commits are copied from the source
concerns onto inconclusive items, findings, and pre-existing findings,
replacing anything the model wrote there, so post-verification sees which
commits to read and the report names them. Post-verification reads every
cited commit with `git_show` and keeps the concern only when the historical
reason still applies to the code at the series end; the report reads the
subject of a commit the history did not show. Severity follows the
consequence of the defect, not its history.

## Duplicate findings in post-verification

Post-verification batches run after verification and see neither each other
nor the findings verification validated directly, so a batch could confirm
under a new ID a defect already reported. The post-verification prompt lists
the validated findings with their pre-existing status in
`<validated_findings>`; a candidate with the same root cause and the same
status is dismissed, citing the validated finding, and each batch reports
only the mechanism it was asked to verify. Verification classifies each
root cause once, never into both findings and inconclusive items.
