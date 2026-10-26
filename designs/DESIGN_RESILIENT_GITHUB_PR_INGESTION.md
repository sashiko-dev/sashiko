# Design: Resilient GitHub Pull Request Ingestion and Review Delivery

## 1. Overview

[DESIGN_GITHUB_PR_COMMENTS.md](DESIGN_GITHUB_PR_COMMENTS.md) introduced
durable outbound delivery of pull request review summaries via
`forge_outbox` and `ForgeWorker`. Production operation on
`sashiko.sashiko.dev` exposed two remaining reliability gaps upstream of
`forge_outbox` that caused pull requests to receive no GitHub comment:

1. **Push-only, volatile pull request ingestion:**
   - GitHub delivers `pull_request` webhooks once without automatic
     retries when the target endpoint is unreachable (for example, HTTP
     `502` during a GKE node recreation or pod rollout). Unlike NNTP
     and Lore email ingestion—which track persistent watermarks in `meta`
     and catch up on missed messages upon startup—forge ingestion had no
     pull-based reconciliation loop.
   - Even after `POST /api/webhook/{provider}` or `POST /api/submit`
     (`Remote` / `RemoteRange`) returned `200 OK`, the resulting
     `FetchRequest` lived only in an in-memory `mpsc::channel(100)`
     inside `FetchAgent`. A process restart before `git fetch` finished
     dropped the in-flight request and marked all `status = 'Fetching'`
     patchsets as `Failed ("Interrupted before fetch completed")`.

2. **Tool-less summary stage failures and all-or-nothing PR comments:**
   - In `sashiko_patch_review.rs`, `summary_stage_with_effort` runs with
     `ToolScope::None` and `max_turns: 1`, yet inherited
     `sashiko_system_prompt(true)`, which instructs the model to call
     repository exploration tools whenever pre-fetched context is
     insufficient, and omitted `target_commit_diff` from its user prompt.
   - When Gemini attempted to invoke an undeclared tool, the API returned
     `finishReason: "MALFORMED_FUNCTION_CALL"` (or
     `"UNEXPECTED_TOOL_CALL"`) with empty text content.
     `translate_ai_response` treated this as `Ok(AiResponse { content:
     None, .. })`, allowing empty responses to be cached and causing
     format validation to fail across all retries with `"The change
     summary cannot be empty."`
   - In `Reviewer::review_patchset_task`, `queue_forge_pr_comment` was
     gated on `if review_success` (`failed_patches == 0`). On a large
     multi-commit pull request, a single failed commit suppressed the
     entire pull request review comment—hiding both the completed
     findings on other commits and any signal that review failed.

This design closes both gaps so GitHub pull requests are ingested with
the same watermark-backed durability as mailing-list emails and always
produce an accurate status/findings comment on GitHub when review
finishes.

---

## 2. Goals and Non-Goals

### 2.1 Goals

1. **Zero missed pull requests across restarts or outages:** Reconcile
   open GitHub pull requests on daemon startup and periodically via the
   GitHub Pulls REST API, persisting a per-repository watermark in
   `meta`.
2. **Crash-safe git fetching:** Persist remote fetch requests in SQLite
   (`fetch_queue`) before acknowledging `/api/webhook/{provider}` or
   `/api/submit`, resume interrupted fetches across daemon restarts, and
   retry transient `git fetch` failures with bounded backoff.
3. **Accurate deduplication on `(repo, mr_number, head_sha)`:** Never
   trigger duplicate reviews when a pull request's target branch (`main`)
   advances (`base.sha` changes while `head.sha` does not), and never
   loop infinitely on a revision whose LLM review failed, while still
   ingesting new force-pushes (`[v2]`, `[v3]`, ...) and recovering
   fetches interrupted before patch extraction completed.
4. **Draft pull request parity:** Review draft pull requests (`draft:
   true`) on open and synchronize identically to ready-for-review pull
   requests, matching webhook behavior while continuing to skip
   Dependabot pull requests.
5. **Self-healing tool-free stages and partial-review reporting:** Give
   tool-free stages a tool-free prompt, convert `MALFORMED_FUNCTION_CALL`
   and `UNEXPECTED_TOOL_CALL` into actionable retry feedback rather than
   empty `Ok` responses, and post an explicit incomplete-review comment
   when some commits in a pull request fail review.

### 2.2 Non-Goals

- Polling non-GitHub forges (GitLab merge requests continue to use
  webhooks + durable `fetch_queue`; polling can be extended later behind
  the same watermark model).
- Diff-anchored inline review threads on GitHub (covered as a separate
  future layer on `forge_outbox`).

---

## 3. Durable Remote Fetch Queue (Migration `016_fetch_queue.sql`)

### 3.1 Schema

```sql
CREATE TABLE IF NOT EXISTS fetch_queue (
    id INTEGER PRIMARY KEY,
    repo_url TEXT NOT NULL DEFAULT '',
    commit_range TEXT NOT NULL,
    mr_url TEXT,
    mr_title TEXT,
    mr_number INTEGER,
    status TEXT NOT NULL DEFAULT 'Pending',
        -- Pending | Fetching | Done | Failed
    locked_at INTEGER,
    retry_count INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER,
    error_log TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE(repo_url, commit_range)
);

CREATE INDEX IF NOT EXISTS idx_fetch_queue_status
    ON fetch_queue(status, next_retry_at);
```

`repo_url` uses `TEXT NOT NULL DEFAULT ''` (representing the local
repository when `FetchRequest.repo_url` is `None`) rather than nullable
`TEXT`, because SQLite treats `NULL` values as distinct inside
`UNIQUE(repo_url, commit_range)`. No credentials or API tokens are ever
stored in `repo_url`; token injection for authenticated GitLab HTTPS
fetches continues to happen transiently in memory inside
`FetchAgent::ensure_remote`.

### 3.2 Enqueue, Claim, Retry, and Startup Recovery

1. **Enqueue before HTTP `200 OK`:**
   - Both `forge_webhook` (via shared `ingest_forge_pull_request`) and
     `submit_patch` (`SubmitRequest::Remote` / `RemoteRange`) call
     `db.enqueue_fetch_request(...)` right after
     `db.create_fetching_patchset(...)`, then send a best-effort wakeup
     on `fetch_sender`.
   - `enqueue_fetch_request` uses `INSERT INTO fetch_queue ... ON
     CONFLICT(repo_url, commit_range) DO UPDATE` so re-submitting a
     previously `Failed` or `Done` range resets its row to `Pending`
     (`retry_count = 0`, `next_retry_at = NULL`, `error_log = NULL`),
     while an already `Pending` or `Fetching` row is left undisturbed
     (updating metadata fields if newly supplied).

2. **Startup recovery (`fail_interrupted_fetching_patchsets`):**
   - On daemon startup, `fail_interrupted_fetching_patchsets` first
     resets any `fetch_queue` rows left in `status = 'Fetching'` back to
     `status = 'Pending', locked_at = NULL`.
   - It then transitions `patchsets` with `status = 'Fetching'` to
     `Failed ('Interrupted before fetch completed')` **only** when no
     active (`status IN ('Pending', 'Fetching')`) `fetch_queue` row owns
     that placeholder (`cover_letter_message_id` matching
     `mr-{mr_number}-{commit_range}@sashiko.local` or
     `{commit_range}@sashiko.local`). Unbacked one-shot fetches (such as
     `SubmitRequest::Thread` Lore fetches) still fail cleanly on
     restart, while `fetch_queue`-backed fetches remain in `Fetching`
     and immediately resume when `FetchAgent` starts.

3. **Worker loop & transient backoff (`FetchAgent`):**
   - `FetchAgent` accepts an optional `(Arc<Database>, max_retries)` via
     `with_db(...)` so standalone unit tests and offline benchmark
     runners without a database continue to operate unchanged via the
     in-memory channel.
   - When backed by `Database`, `FetchAgent::run` drains due `Pending`
     rows (`next_retry_at IS NULL OR next_retry_at <= now`) on startup,
     on channel wakeup, and every 5 seconds, sweeping stale `Fetching`
     locks older than 10 minutes (`sweep_stale_fetch_queue`).
   - Transient `git fetch` errors increment `retry_count` and reschedule
     with exponential backoff (`5s`, `30s`, `180s`). Once
     `retry_count >= max_retries` (or on permanent git revision/extraction
     errors after fetch succeeds), the row is marked `Failed` and
     `Event::IngestionFailed` is emitted with the exact `article_id`
     (`mr-{mr_number}-{commit_range}` when `mr_number` is present, else
     `{commit_range}`) so the corresponding `Fetching` placeholder
     patchset transitions to `Failed` with the redacted error message.

---

## 4. Hybrid GitHub Pulls API Reconciliation

### 4.1 Configuration (`[forge]`)

`ForgeSettings` gains two optional `#[serde(default)]` fields (preserving
`#[serde(deny_unknown_fields)]` compatibility):

```toml
[forge]
enabled = true
provider = "github"
# Repositories ("owner/repo" or GitHub URLs) polled for missed PR updates.
# Can also be set via SASHIKO__FORGE__REPOS="owner/repo1,owner/repo2".
# When empty and provider = "github", auto-detected from git remote "origin".
repos = ["sashiko-dev/sashiko"]
# Poll interval in seconds for open pull request reconciliation (0 disables).
poll_interval_secs = 120
```

`repos` uses `deserialize_string_or_vec` so operators can configure it
either as a TOML array or as a comma-separated environment variable
(`SASHIKO__FORGE__REPOS`). When `repos` is empty and `provider` is
`"github"` (or unset when `origin` points to `github.com`), startup
inspects `git remote get-url origin` in `settings.git.repository_path`
via `git_cmd::in_dir_async` and extracts `{owner}/{repo}` if it refers to
GitHub.

### 4.2 Authentication & HTTP Discipline

- Polling shares `ForgeWorker`'s in-memory GitHub App installation token
  cache (`resolve_optional_token`).
- Resolution order:
  1. GitHub App installation token (`app_id` + `installation_id` +
     private key), cached until 5 minutes before its 1-hour expiry.
  2. `forge.api_token` if non-empty.
  3. `None` (unauthenticated `GET /repos/{owner}/{repo}/pulls`), which
     allows public repositories (`60 requests/hour`, requiring 30
     requests/hour at `poll_interval_secs = 120`) to reconcile missed
     webhooks even when `post_mode = "off"` and no credentials are
     configured.
- If GitHub responds with `401 Unauthorized`, the cached installation
  token is invalidated immediately.
- All HTTP requests use `crate::worker::outbox_client()` (10s connect
  timeout, 60s total timeout).

### 4.3 Watermark & Pagination

For each configured `{owner}/{repo}`:

1. **Watermark bootstrap (`meta` key
   `forge_github_poll_watermark:{owner}/{repo}`):**
   - If present in `meta`, parse the stored Unix timestamp.
   - If absent (first run after upgrade or fresh deployment):
     - If `patchsets` already contains at least one row, bootstrap to
       `now - 86_400` (24-hour lookback) so any pull requests missed
       during the deployment window are caught automatically without
       ingesting months-old open pull requests.
     - If `patchsets` is completely empty, bootstrap to `now - 300`
       (5-minute grace window), matching empty-DB NNTP startup cutoff
       semantics.
   - Persist the bootstrapped watermark immediately in `meta`.

2. **Incremental Pulls API scan:**
   - Request `GET {api_base}/repos/{owner}/{repo}/pulls?state=open&sort=updated&direction=desc&per_page=50&page={page}`
     (up to 5 pages max per cycle as an upper bound).
   - Because results are ordered by `updated_at DESC`, stop paginating
     as soon as an item has `updated_at < watermark - 60` (a 60-second
     clock-skew overlap window).
   - Validate every candidate pull request with the exact same security
     checks as `GitHubForge::parse_payload`: `pr_number > 0`,
     `is_valid_git_sha(&base_sha)`, `is_valid_git_sha(&head_sha)`, and
     `is_safe_repo_url(&clone_url)`.
   - Skip pull requests whose author matches `is_dependabot_author`.
   - Do **not** skip draft pull requests (`draft == true`).

3. **Advancing the watermark:**
   - Record `cycle_started_at` before page 1.
   - Advance `forge_github_poll_watermark:{owner}/{repo}` to
     `cycle_started_at` **only** if all fetched pages and all candidate
     `ingest_forge_pull_request` database writes succeeded without
     error. Any transient GitHub API or database error leaves the
     watermark unchanged so the next cycle retries cleanly.

### 4.4 Deduplication on `(repo, mr_number, head_sha)`

GitHub updates a pull request's `updated_at` on comments, labels, review
requests, and draft toggles—not just pushes—and dynamically advances
`pull_request.base.sha` in `GET /pulls` responses whenever the target
branch (`main`) moves forward, even when the pull request's own commits
(`head.sha`) have not changed.

Deduplicating by `mr-{n}-{base}..{head}` alone would therefore trigger
spurious `[v2]`, `[v3]` re-reviews of unchanged pull requests whenever
`main` advances. Instead, `db.has_forge_pr_head_ingested(repo,
mr_number, head_sha)` queries `patchsets` using the existing
`idx_patchsets_mr_number` index (`WHERE mr_number = ?`), filters rows in
Rust via literal `extract_owner_repo_from_mr_url(mr_url) == Some(repo)`
(avoiding SQL `LIKE` wildcard hazards on `_` or `%`), and checks whether
any row for that PR already targets `head_sha`
(`cover_letter_message_id` containing `..{head_sha}@` or ending with
`..{head_sha}`):

- **Already ingested (`true` — skip):**
  - Any row for `(repo, mr_number, head_sha)` in `Pending`, `In Review`,
    `Reviewed`, `Cancelled`, `Incomplete`, `Failed To Apply`, or
    `Failed` with any reason *other* than `'Interrupted before fetch
    completed'`. (A review-time failure is not retried automatically by
    the poller, preventing infinite LLM retry loops.)
  - Any row in `Fetching` that has an active (`Pending` or `Fetching`)
    `fetch_queue` entry for `..{head_sha}`.
- **Needs ingestion (`false` — enqueue):**
  - No row exists for `(repo, mr_number, head_sha)` (brand-new PR or new
    push/force-push changing `head_sha`).
  - Every matching row for `(repo, mr_number, head_sha)` failed with
    `failed_reason = 'Interrupted before fetch completed'` (or is
    orphaned in `Fetching` without an active `fetch_queue` row),
    allowing seamless recovery of pre-upgrade interrupted PRs.

Both `forge_webhook` and the poller route accepted PRs through a single
shared helper (`ingest_forge_pull_request`) that creates/updates the
`Fetching` placeholder (computing `[vN]` and rotating `slug`), writes to
`fetch_queue`, and nudges `fetch_sender`.

---

## 5. Tool-Free Summary Stage & Partial-Failure PR Comments

### 5.1 Tool-Free Prompt for `summary_stage_with_effort`

`summary_stage_with_effort` (`src/workflows/sashiko_patch_review.rs`) is
a single-turn (`max_turns: 1`), tool-free (`ToolScope::None`) synthesis
stage. Its prompt is decoupled from `sashiko_system_prompt(true)`:

- **System prompt:** Concise role prompt stating that no repository or
  git tools are available in this stage and all output must be derived
  strictly from the commit message and diff provided in the user prompt.
- **User prompt:** Includes `<patch>\n{{target_commit_diff}}\n</patch>`
  followed by `STAGE_SUMMARY_INSTRUCTION`, explicitly instructing the
  model to include `Before:` and `After:` blocks only when user-visible
  behavior changes are clear from the provided commit message and diff
  (omitting them rather than guessing or attempting tool calls when a
  change is internal or ambiguous).

### 5.2 Recovering from `MALFORMED_FUNCTION_CALL` / `UNEXPECTED_TOOL_CALL`

1. **`src/ai/gemini.rs` (`translate_ai_response`):**
   - Treat `finish_reason == "MALFORMED_FUNCTION_CALL"` and
     `finish_reason == "UNEXPECTED_TOOL_CALL"` as explicit `Err` results
     (`"Gemini candidate failed tool call (finish reason: ...)"`) rather
     than `Ok(AiResponse { content: None, tool_calls: None })`.
   - Returning `Err` prevents `CachingAiProvider` from persisting empty
     malformed-call responses in the SQLite response cache.
2. **`src/workflow/stage.rs` & `src/ai/session.rs`
   (`handle_provider_error`):**
   - Match `MALFORMED_FUNCTION_CALL` and `UNEXPECTED_TOOL_CALL` and
     return `ErrorAction::RetryWithFeedback(...)`:
     - When no tools are available (`ToolScope::None`), instruct the
       model: *"No tools are available in this stage. Respond directly
       in the required output format without attempting to call any
       tools."*
     - When tools are available, instruct the model either to emit a
       valid tool call matching one of the declared tool schemas or to
       respond directly in the required output format without calling
       tools.

### 5.3 Partial-Failure GitHub PR Review Comments

1. **`PatchReviewSummaryItem` & `compose_pr_review_comment`
   (`src/forge.rs`):**
   - Add `pub failed: bool` to `PatchReviewSummaryItem`.
   - When one or more commits have `failed == true`, **never** emit `✓
     **No issues found**`. Instead emit an incomplete-review warning
     after the heading:
     ```markdown
     ### Sashiko review

     ⚠ **Review incomplete** — 1 of 3 commits failed to complete review (`2/3`).

     [Full review and stage logs on sashiko.sashiko.dev](https://sashiko.sashiko.dev/#/patchset/sashiko-733)
     ```
     If other commits in the same pull request *did* produce findings,
     render the warning banner followed by the series summary (if
     present) and every completed commit's findings section so authors
     still receive all discovered bugs immediately.
2. **`Reviewer::review_patchset_task` (`src/reviewer.rs`):**
   - Call `queue_forge_pr_comment` whenever the patchset was not
     `Cancelled` (both `Reviewed` and `Failed`), marking each non-cover
     patch as `failed = true` if it has no completed `Reviewed` entry
     and its patch status is not `Skipped`.
   - Keep immediate embargo release (`release_patchset_results`) gated on
     `if review_success` so embargoed patchsets with unreviewed commits
     never lift their embargo early.
   - Update `insert_forge_outbox` (`src/db.rs`) so that if a patchset
     whose review previously failed is later re-run and reaches
     `Reviewed`, an existing `Pending`/`Sent` incomplete-review comment
     does not permanently block the completed review comment: when
     `patchsets.status == 'Reviewed'` and the existing `forge_outbox`
     row was queued for an incomplete review (`body` containing
     `Review incomplete`), refresh/re-queue or allow replacement while
     preserving strict single-comment idempotency for normal retries.

---

## 6. Implementation Plan

Each step is a self-contained, tested commit:

| # | Commit | Files |
|---|---|---|
| 1 | `designs: specify resilient GitHub pull request ingestion and delivery` | `designs/DESIGN_RESILIENT_GITHUB_PR_INGESTION.md` |
| 2 | `workflows: use tool-free prompt in sashiko summary stage` | `src/workflows/sashiko_patch_review.rs` |
| 3 | `ai/gemini, workflow: recover from MALFORMED_FUNCTION_CALL responses` | `src/ai/gemini.rs`, `src/ai/session.rs`, `src/workflow/stage.rs` |
| 4 | `forge, reviewer: post PR review comments when some commits fail` | `src/forge.rs`, `src/reviewer.rs`, `src/db.rs` |
| 5 | `db, fetcher, server: persist remote git fetch requests in fetch_queue` | `src/migrations/014_fetch_queue.sql`, `src/db.rs`, `src/fetcher.rs`, `src/server.rs`, `src/main.rs` |
| 6 | `forge, worker: reconcile open GitHub pull requests via periodic polling` | `src/settings.rs`, `src/forge.rs`, `src/db.rs`, `src/server.rs`, `src/worker/forge.rs`, `src/main.rs`, `docs/configuration.md`, `docs/GITHUB_SETUP.md`, `docs/examples/Settings.example.toml` |
