# Design: Optional Bug Database and Economical Upstream Fix Tracking

## 1. Context & Motivation

Sashiko discovers pre-existing bugs in surrounding code while reviewing patches and tracks them in a dedicated bugs database (`bugs`, `bug_occurrences`, `bug_enrichments`), processed asynchronously by `BugWorker` (`src/worker/bug_worker.rs`) and `workflows::linux_bug` (`src/workflows/linux_bug.rs`) across both supported projects (`ProjectId::Linux` and `ProjectId::Sashiko`).

Two operational requirements have emerged:
1. **Optional Bug Database (Disabled by Default):** Not all Sashiko deployments track pre-existing upstream bugs (for example, lightweight local or self-hosted instances). By default, pre-existing issues discovered during patch review should be ignored rather than reported as patch findings or queued for standalone bug analysis.
2. **Economical Periodic Upstream Fix Tracking:** For deployments that enable the bug database (`sashiko.dev` for the Linux kernel and `sashiko.sashiko.dev` for Sashiko itself), open bugs verified at an earlier mainline commit (`verified_on_sha`) may subsequently be fixed upstream (`master` in Linus Torvalds's tree or `main` in `sashiko-dev/sashiko`). Re-running the full multi-stage bug verification LLM pipeline across all open bugs after every mainline pull would consume prohibitive token budgets because the vast majority of open bugs reside in files that have not been modified since `verified_on_sha`.

This design specifies:
- Making the bug database opt-in (`[linux_bug] enabled = false` by default), ensuring pre-existing issues are excluded from patch review findings and ignored when the bug database is disabled, and enabling it in the production deployment manifests (`deployment/sashiko.dev` and `deployment/sashiko.sashiko.dev`).
- Supporting both `ProjectId::Linux` (C kernel code, `MAINTAINERS`, `prompts/severity.md`, `torvalds/linux` `master`) and `ProjectId::Sashiko` (Rust/SQL/HTML/YAML code, module-based subsystems, `prompts/sashiko/severity.md`, `sashiko-dev/sashiko` `main`) in the bug triage and upstream fix verification workflows.
- A three-tier periodic upstream fix checker in `BugWorker` that uses deterministic git history filtering to handle $>95\%$ of open bugs with **zero LLM tokens**, invoking a lightweight single-stage LLM fix verifier only when commits in `<verified_on_sha>..<linus_sha>` actually touch a bug's source files or reference its introducing commit.

---

## 2. Optional Bug Database & Pre-existing Issue Handling

### 2.1 Configuration (`[linux_bug]`)

Add `LinuxBugSettings` to `Settings` (`src/settings.rs`), deserialized from `[linux_bug]` (with `#[serde(alias = "bugs")]` for backwards compatibility):

```toml
[linux_bug]
# Enable tracking and background analysis of pre-existing bugs (default: false)
enabled = false
# Lease TTL in seconds for in-progress bug claims
lease_ttl_seconds = 1800
# Maximum pipeline attempts before marking a bug failed
max_attempts = 3
# Interval in seconds between periodic upstream fix checks against Linus's tree (0 = disabled)
fix_check_interval_seconds = 21600
# Maximum number of open bugs to evaluate per upstream fix check cycle
fix_check_batch_size = 50
```

### 2.2 Exclusion of Pre-existing Issues from Patch Findings

During `linux_patch_review` (`src/workflows/linux_patch_review.rs`), `conflict_resolution_stage` already routes `preexisting: true` items exclusively to `state.concerns` while excluding them from `state.findings` (matching `sashiko_patch_review.rs`). However, `verification_stage` in `linux_patch_review.rs` previously appended `preexisting: true` items to `state.concerns` *and* left them in `state.findings`, causing pre-existing bugs to appear in inline patch review emails.

`verification_stage` is updated to match `conflict_resolution_stage` and `sashiko_patch_review.rs`:
- Any finding marked `preexisting: true` is converted into a concern in `state.concerns` and excluded from `state.findings`.
- In `src/reviewer.rs`, when `!ctx.settings.linux_bug.enabled`, `concerns` extracted from reviews are ignored instead of creating `bugs` and `bug_occurrences` rows.

### 2.3 Worker, API, UI, and Deployment Gating

- **Background Worker (`src/main.rs`):** `BugWorker` is spawned only when `settings.linux_bug.enabled` is `true`, configured with `.with_project(settings.project.kind)`.
- **REST API (`src/api.rs`):**
  - `GET /api/config` includes `"bugs_enabled": state.settings.linux_bug.enabled`.
  - `POST /api/bug/analyze` returns `404 Not Found` when `state.settings.linux_bug.enabled` is `false`.
- **Frontend (`static/index.html`):** The `#btn-bugs` navigation button is hidden when `config.bugs_enabled === false`.
- **Deployment Manifests:**
  - `Settings.toml` leaves `linux_bug.enabled = false` by default.
  - `deployment/sashiko.dev/base/app/sashiko-k8s.yaml` and `deployment/sashiko.sashiko.dev/base/app/sashiko-self-k8s.yaml` set `SASHIKO__LINUX_BUG__ENABLED: "true"` so both production instances track pre-existing bugs and periodic upstream fixes.

---

## 3. Economical Upstream Fix Tracking in Linus's Tree

### 3.1 Architecture & Three-Tier Filtering Pipeline

```mermaid
flowchart TD
    Timer[Periodic Timer in BugWorker] --> ResolveHEAD[Resolve Linus HEAD SHA: origin/master or master]
    ResolveHEAD --> QueryBugs[Query canonical open & succeeded bugs where verified_on_sha != linus_sha]
    
    QueryBugs --> Tier1{Tier 1: Watermark Check\nverified_on_sha == linus_sha?}
    Tier1 -- Yes --> Skip[Skip: 0 git calls, 0 LLM tokens]
    Tier1 -- No --> Tier2[Tier 2: Deterministic Git Pre-Filter]

    subgraph GitPreFilter [Zero-Token Git History Filter]
        Tier2 --> CheckFiles[git log verified_on_sha..linus_sha -- source_files]
        Tier2 --> CheckFixes[If introducing_commit_sha known:\ngit log verified_on_sha..linus_sha --grep=short_intro_sha]
        CheckFiles & CheckFixes --> HasCommits{Any candidate commits?}
    end

    HasCommits -- "No (0 commits)" --> AdvanceWatermark[Advance verified_on_sha = linus_sha\n0 LLM tokens]
    HasCommits -- "Yes (1..N commits)" --> Tier3[Tier 3: Single-Stage LLM Fix Verification]

    subgraph LLMVerify [Bounded Single-Stage LLM Check]
        Tier3 --> Prefetch[Prefetch candidate commit diffs on bug files\n+ current code at linus_sha]
        Prefetch --> RunSession[Run VerifyUpstreamFixSession\nmax_turns = 8, ToolScope::GitOnly]
        RunSession --> Verdict{Verdict Status}
    end

    Verdict -- "fixed + valid ancestor SHA" --> MarkFixed[Insert fix_candidate status=merged\nTransition lifecycle_status = fixed]
    Verdict -- "still_present" --> AdvanceAfterLLM[Insert verification enrichment\nAdvance verified_on_sha = linus_sha]
    Verdict -- "uncertain" --> LeaveUnchanged[Keep open for future retry]
```

### 3.2 Resolving Linus's Mainline Tree Reference

`BugWorker::resolve_master_sha` resolves `<mainline_remote>/master` (configured via `git.mainline_remote`, defaulting to `origin/master`, falling back to `master`, then `HEAD`). This guarantees that upstream fix checks exclusively inspect Linus's mainline branch rather than subsystem `-next` branches.

### 3.3 Tier 1 & Tier 2: Deterministic Zero-Token Git Pre-Filter

For each open canonical bug (`lifecycle_status = 'open'`, `pipeline_state = 'succeeded'`, `duplicate_of_id IS NULL`) up to `fix_check_batch_size`:

1. **Determine Baseline (`verified_on_sha`):**
   - Read `bug.verified_on_sha()`.
   - If `verified_on_sha == Some(linus_sha)`, skip immediately.
   - Verify that `verified_on_sha` exists and is an ancestor of `linus_sha` via `git merge-base --is-ancestor <verified_on_sha> <linus_sha>`.
2. **Extract Tracked Files & Introducing Commit:**
   - Extract affected file paths from `bug.source_files()` and `bug.locations()`.
   - Extract `introducing_commit_sha` from the bug's `origin_discovery` enrichment (if present).
3. **Query Git Range (`<verified_on_sha>..<linus_sha>`):**
   - Run `git log --oneline --no-merges <verified_on_sha>..<linus_sha> -- <files...>` using `git_cmd::in_dir_async`.
   - If `introducing_commit_sha` is available (at least 7 hex chars), also run `git log --oneline --no-merges --fixed-strings --grep=<short_sha> <verified_on_sha>..<linus_sha>` to catch fixes that refactored or moved code across files while citing `Fixes: <short_sha>`.
4. **Short-Circuit When Untouched:**
   - If both queries return zero commits, the bug's files and `Fixes:` references were untouched in `<verified_on_sha>..<linus_sha>`.
   - Insert a `verification` enrichment advancing `verified_on_sha` to `linus_sha` while preserving `locations` and `source_files` in `data_json` so projections and UI views remain intact.
   - Cost: **0 LLM tokens**.

### 3.4 Tier 3: Single-Stage LLM Fix Verification (`verify_upstream_fix`)

When one or more commits in `<verified_on_sha>..<linus_sha>` touch the bug's files or cite its introducing commit:

1. **Bounded Prefetching:**
   - Prefetch the commit log and per-file diffs for the candidate commits (capped at `MAX_FIX_CHECK_PREFETCH_CHARS = 24,000` characters, restricting diffs to the bug's affected files) plus the current code snippet around the bug's locations at `linus_sha`.
2. **Single-Stage Session (`VerifyUpstreamFixSession`):**
   - Equipped with `ToolScope::GitOnly` (`git_show`, `git_read_files`, `git_log`, `git_blame`, `git_grep`) and a tight turn budget (`max_turns = 8`).
   - Output schema (`UpstreamFixVerdict`):
     - `status`: `"fixed" | "still_present" | "uncertain"` (providing an explicit escape hatch per LLM Workflow Design guidelines).
     - `fixing_commit_sha`: `Option<String>` (required when `status == "fixed"`).
     - `explanation`: `String` citing concrete code changes in the fixing commit or explaining why the bug remains present at `linus_sha`.
3. **Deterministic Commit Validation:**
   - If the LLM reports `status == "fixed"`, validate `fixing_commit_sha` using `git rev-parse --verify <sha>^{commit}` and `git merge-base --is-ancestor <sha> <linus_sha>`.
   - If the SHA does not resolve to a valid ancestor of `linus_sha`, reject the `"fixed"` verdict and treat it as `"uncertain"` (preventing hallucinated commit SHAs from closing open bugs).
4. **Database State Transitions (`src/db.rs`):**
   - **Fixed:** `db.mark_bug_fixed_upstream(bug.id, &full_fixing_sha, &linus_sha, &explanation, ...)` atomically inserts a `fix_candidate` enrichment (`status: "merged"`, `commit_sha: full_fixing_sha`, `verified_on_sha: linus_sha`, `explanation`), inserts a `verification` enrichment updating `verified_on_sha`, and updates `lifecycle_status = 'fixed'` (`WHERE id = ? AND lifecycle_status = 'open'`).
   - **Still Present:** `db.advance_bug_verified_sha(bug.id, &linus_sha, ...)` inserts a `verification` enrichment advancing `verified_on_sha` to `linus_sha` (preserving existing `locations` and `source_files`) so the same commits are not re-evaluated on the next cycle.

---

## 4. Verification & Testing Plan

1. **Patch Review Pre-existing Exclusion:**
   - Verify `verification_stage` unit tests confirm `preexisting: true` findings are placed only in `state.concerns` and excluded from `state.findings`.
2. **Optional Bug Database Config & Gating:**
   - Verify `Settings::new()` defaults `linux_bug.enabled` to `false`.
   - Verify `Reviewer::process_issue` skips creating bugs when `linux_bug.enabled == false`.
   - Verify `/api/config` exposes `bugs_enabled` and `/api/bug/analyze` rejects requests when disabled.
3. **Upstream Fix Tracking Tests:**
   - **Zero-token path:** Create a temporary git repository with an open bug verified at commit A, add commit B touching an unrelated file, run the upstream fix check, and assert `verified_on_sha` advances to B with 0 AI calls.
   - **Fixed path:** Add commit C modifying the buggy file to fix the defect, run the upstream fix check with a mock AI provider returning `status = "fixed"` and commit C's SHA, and assert `lifecycle_status` transitions to `Fixed`, `fixed_in_commit()` equals C, and `verified_on_sha` advances to C.
   - **Hallucinated SHA guardrail:** Verify that if the AI returns `status = "fixed"` with a non-existent or non-ancestor SHA, the bug remains `Open`.
   - **Still-present path:** Verify that if commit C touches the file without fixing the bug (`status = "still_present"`), the bug remains `Open` and `verified_on_sha` advances to C while preserving `locations` and `source_files`.
4. **CI & Self-Review:**
   - Run `make check-pr` and `sashiko review --project sashiko --agent` across all commits.
