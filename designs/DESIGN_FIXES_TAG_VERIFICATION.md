# Design: `Fixes:` Tag Verification and Suggestion

## 1. Problem Statement & Goals

In the Linux kernel, Sashiko, and QEMU workflows, bug-fix commits carry a `Fixes:` trailer identifying the commit that introduced the defect:

```text
Fixes: <12-char sha> ("<subject>")
```

Accurate `Fixes:` trailers are essential for stable-tree backporting, regression tracking, and bisectability. Reviewers check two properties on every patch:
1. **Verify existing `Fixes:` tags**: Is the syntax canonical? Does the SHA exist in the repository (including fetched maintainer trees) and not point to an unmerged patch in the same series? Does the quoted subject match the commit's actual subject? And **semantically**, did that commit actually introduce the bug being fixed?
2. **Suggest missing `Fixes:` tags**: When a commit fixes a concrete bug without a `Fixes:` tag, can the specific commit in repository history that introduced the broken logic be identified with high confidence?

### Project Scope
- **Enabled**: `linux`, `sashiko`, `qemu`.
- **Disabled**: `gcc` (GCC commits use GNU ChangeLog blocks and Bugzilla `PR <component>/<number>` tags rather than `Fixes: <sha> ("<subject>")` trailers).

### Design Goals
- **Zero Hallucinated SHAs or Subjects**: Never suggest or "correct" a `Fixes:` tag with an invented SHA, wrong SHA length, or paraphrased subject line.
- **Maintainer-Tree Awareness**: A `Fixes:` SHA may reside in a maintainer branch rather than mainline (`Baseline SHA`). Resolve SHAs across the entire repository git object database (`<sha>^{commit}`), search `git log --all --fixed-strings --grep=<subject>` when a SHA is not directly found, and report completely unknown SHAs at `Low` severity.
- **High Precision / Benchmark Safety**: Never demand `Fixes:` tags on new features, refactorings, cleanups, documentation, hardware enablement, or ambiguous historical evolution.
- **Minimal Tool-Turn Overhead**: Pre-compute deterministic trailer checks and hunk `git blame` candidates in Rust so LLM stages do not burn extra tool turns on routine commits.
- **Lineage Integrity (`review_map`)**: Preserve 100% concern lineage accounting (`source_ids`) when the post-verification Rust guard filters out an invalid `Fixes:` suggestion.

---

## 2. Architecture: Hybrid Deterministic Git Pre-Analysis + Workflow Integration + Rust Guard

```mermaid
sequenceDiagram
    participant W as Worker::run (Rust)
    participant G as Git Worktree (git_cmd)
    participant Goal as Goal Stage (LLM)
    participant Ver as Verification / Post-Verification (LLM)
    participant Guard as sanitize_fixes_findings_with_dismissals (Rust)
    participant Rep as Report Stage (LLM)

    W->>G: 1. extract_fixes_trailers & validate SHA/series/subject
    W->>G: 2. Prefetch cited Fixes: commit diff & top git blame commits on modified hunks
    G-->>W: FixesAnalysis (<fixes_context>)
    W->>Goal: System prompt + {{fixes_block}} (when use_log == true)
    Goal->>G: git_show to confirm introducing commit
    Goal-->>Ver: Concern C* (or Dismissed D*) with canonical Fixes: line
    Ver->>G: Verify introducing commit diff if inconclusive
    Ver-->>Guard: Verified findings + dismissed_concerns
    Guard->>G: Canonicalize Fixes: <12-char sha> ("<subject>") or convert invalid suggestion to dismissal
    Guard-->>Rep: Canonicalized & lineage-complete findings
```

---

## 3. Component Design

### 3.1 Deterministic Pre-Analysis (`src/worker/fixes_check.rs`)

Executed during `Worker::run` (`src/worker/prompts.rs`) when `worktree_path` is available and stored in `state.fixes_analysis: FixesAnalysis`:

- **Core Types**:
  - `FixesAnalysis`: holds `prompt_context: String`, `author_fixes_shas: Vec<String>`, `author_fixes_raw: Vec<String>`, `series_commit_shas: Vec<String>`, and `known_commits: Vec<ResolvedFixesCommit>`.
  - `ResolvedFixesCommit`: holds `full_sha`, `abbrev_sha`, `commit_date`, and `subject` (with `canonical_trailer()` and `matches_sha()`).
  - `ParsedFixesTrailer`: holds `raw`, `tag_prefix`, `is_multiline`, and `kind: FixesTrailerKind`.
  - `FixesTrailerKind`:
    - `BugTrackerId(String)`: valid Sashiko `<project>-<uuid>` bug reference when `project == ProjectId::Sashiko`.
    - `Url(String)`: external URL misplaced in a `Fixes:` trailer.
    - `CommitRef { sha_token, subject, canonical_parens_quotes, exact_single_space_before_paren }`: commit SHA reference with optional quoted subject and formatting flags.
    - `Malformed(String)`: unparseable trailer value in the commit trailer block.
  - `HunkBlameTarget` & `BlameCandidate`: `extract_diff_blame_targets` collects modified files and pre-image line ranges; `collect_blame_candidates_async` runs `git blame --porcelain -L <start>,<end> <parent_rev> -- <file>` and ranks the top 5 historical commits (`BlameCandidate { commit, score, touched_summary }`), excluding `INITIAL_KERNEL_ROOT_SHA` (`1da177e4c3f415667457b932fe38c6f7471e1a58`) and `series_commit_shas`.

### 3.2 Prompt Injection & Stage Instructions

- `render_fixes_block(use_log, &state.fixes_analysis)` is injected via `{{fixes_block}}` into `linux_system_prompt`, `sashiko_system_prompt`, and `qemu_system_prompt` whenever `use_log == true`.
- `STAGE_GOAL_INSTRUCTION`, `STAGE_VERIFICATION_INSTRUCTION`, and `STAGE_POST_VERIFICATION_INSTRUCTION` in `src/workflows/linux_patch_review.rs`, `src/workflows/sashiko_patch_review.rs`, and `src/workflows/qemu_patch_review.rs` (along with project prompt guides under `projects/{linux,sashiko,qemu}/prompts/`) instruct the workflow to:
  1. Audit all deterministic statuses in `<fixes_context>` and verify `SYNTAX & SHA VALID` commits with `git_show`.
  2. Report unknown `Fixes:` SHAs at `Low` severity noting the commit may reside in an unindexed maintainer tree or contain a typo.
  3. Suggest a missing `Fixes: <12-char sha> ("<subject>")` at `Low` severity (`"preexisting": false`) on genuine bug fixes only when `git_show` on the candidate commit proves it introduced the defect.
  4. Dismiss `Fixes:` suggestions on non-bugfix commits (features, refactorings, cleanups, docs, tests, style/warning fixes) or pointing to same-series commits.

### 3.3 Post-Verification Canonicalizer & Lineage Guard

`sanitize_fixes_findings_with_dismissals` runs inside `enrich_verification_output_for_stage` and `apply_post_verification_stage_output` (`src/workflows/linux_patch_review.rs`) before `VD*`/`VF*`/`PVD*`/`PVF*` IDs are assigned:
- Canonicalizes any valid `Fixes: <sha> ("<subject>")` suggestion in `findings` to the exact 12-char SHA and subject from git (`Fixes: <abbrev_sha> ("<subject>")`) while keeping bare `Fixes: <sha>` references in short `problem` titles concise (`Fixes: <abbrev_sha>`) and preserving verbatim quotes of the author's own trailer (`author_fixes_raw`).
- Enforces `"severity": "Low"` and `"preexisting": false` on `Fixes:` trailer findings (`is_fixes_trailer_finding`).
- Rejects any `Fixes:` suggestion whose suggested SHA is unresolvable in the repository or belongs to `series_commit_shas` (unless it is the author's own `Fixes:` SHA being reported as invalid), converting the rejected finding into a dismissal in `dismissed_concerns` with its `source_ids` and `locations` intact so `review_map` lineage accounting remains complete.
