# Reviewing changes to `src/ai/`

What this subsystem owns: every byte that leaves Sashiko for a model and
every byte that comes back. It guarantees four things to the rest of the
program, and a defect here is invisible in the diff that causes it and
visible three layers away as a wrong verdict, a wedged worker, or a bill.

1. A request reaches exactly one provider, subject to a global
   concurrency ceiling and a global rate-limit gate.
2. A failure arrives at the caller classified as retryable or fatal, with
   a delay attached if it is retryable.
3. Token usage reported back is in one unit system, so budgets and cost
   accounting hold.
4. A response that was cut short by the provider is never mistaken for a
   complete one.

Nothing in this module is enforced by the type system beyond the
`AiProvider` trait signature. Every invariant below is a convention held
up by review, by the unit tests named where they exist, and by nothing
else. Say so out loud when you rely on one.

---

## 1. The `AiProvider` contract, and which parts are load-bearing

`AiProvider` (`src/ai/mod.rs`) has four methods. Two are required, two
have defaults, and the defaults are where new providers go wrong.

```rust
async fn generate_content(&self, request: AiRequest) -> Result<AiResponse>;
fn get_capabilities(&self) -> ProviderCapabilities;
fn cache_stats(&self) -> Option<CacheStats> { None }
fn cache_identity(&self) -> String { self.get_capabilities().model_name }
```

### `AiResponse` field by field — what each consumer does with it

- **`truncated`** — the single most load-bearing field. `SessionRunner::run`
  bails the whole session the moment it sees `resp.truncated`:
  `anyhow::bail!("LLM output was truncated by provider ...")`. If a new
  provider leaves this `false` when the model hit its output cap, the
  session accepts a half-written JSON verdict and hands it to
  validation, which either fails opaquely or — worse — parses. The three
  HTTP providers derive it from the provider's own stop signal:
  `gemini::translate_ai_response` from `finish_reason == "MAX_TOKENS"`,
  `claude::translate_ai_response` from `stop_reason == "max_tokens"`,
  `openai::translate_ai_response` from `finish_reason == "length"`.
  **Every `*_cli` provider hardcodes `truncated: false`** — see the six
  construction sites in `claude_cli::parse_inner_response` /
  `parse_single_json`. That is a known blind spot, not a guarantee: a CLI
  that silently stops at its own cap looks complete. If a diff adds a CLI
  provider, check whether the CLI reports a stop reason and wire it; if
  it does not, say so in a comment rather than leaving a bare `false`.

- **`usage.prompt_tokens` / `usage.cached_tokens`** — the contract is
  stated on `AiUsage` and is easy to get backwards: **`cached_tokens` is
  a breakdown of `prompt_tokens`, not an addend.** A consumer subtracts
  it to get uncached input. A provider whose API reports the cached
  prefix *outside* its prompt total must fold it in before filling these
  fields. Getting this wrong is the single most repeated bug in this
  module's history:
  - `42c27917b537` "bedrock: fix input token counting to include
    cache_read and cache_write" — Bedrock's `input_tokens` excludes both.
  - `0d86ff63a1f7` "ai: Count the cached prefix in claude-cli's prompt
    tokens" — the CLI reports `input_tokens` Anthropic-style (excluding
    the cached prefix); `parse_usage` now folds `cache_read_input_tokens`
    *and* `cache_creation_input_tokens` into `prompt_tokens`, and reports
    only the read as `cached_tokens`, because cache *creation* is input
    the model actually processed.
  - `4c107c0f0c36` "ai: Report cached prompt tokens for OpenAI-compatible
    providers" — the field did not exist; every review on that provider
    recorded zero cache hits. Note the defensive clause it added:
    `.filter(|&c| c <= resp.usage.prompt_tokens)`, which *drops* a count
    larger than the prompt rather than clamping, because clamping would
    leave uncached-input at zero and silently disable the token budget.
  - `f399f9a3b7b4` "ai: Fix the cached-token count on a response cache
    hit" — the local cache was *adding* a stored count to the prompt.

  The consumer that makes this matter is in `reviewer.rs`: it computes
  `uncached_input = usage.prompt_tokens.saturating_sub(cached)` and
  charges `uncached_input + completion_tokens` against
  `review.max_total_tokens`, aborting the review with
  `ReviewError::BudgetExceeded` when it trips. A provider that
  over-reports `cached_tokens` charges nothing and the budget never
  fires; one that under-reports `prompt_tokens` does the same.

  **When a diff touches usage parsing, verify by hand:**
  `cached_tokens <= prompt_tokens` for every path, and
  `prompt_tokens` includes the cached prefix.

- **`tool_calls[].id`** — the id is what `SessionRunner` writes back as
  `tool_call_id` on the `AiRole::Tool` message, and what each provider's
  request translator matches on. Gemini has no call ids in v1beta, so
  `gemini::translate_ai_response` uses `function_call.name` as the id.
  That means two parallel calls to the *same* tool in one turn collide.
  If a diff adds parallel-tool support to a provider, check that ids are
  distinct within a turn.

- **`thought` / `thought_signature`** — opaque provider state.
  `scrub_thought_signatures` (`src/ai/mod.rs`) must run on anything
  persisted (`ec08a998d1ab`) and is also applied to the cache key, so a
  signature does not poison cache lookups.

### `get_capabilities().context_window_size`

Only two things read it: `vllm::fit_messages_to_budget` (which will
actually trim the prompt) and metadata. `claude_cli::context_window_for_model`
says as much in its own comment. Do not assume a wrong value here is
caught anywhere.

### `cache_identity` — the default is almost always wrong

Its job: name every setting that **shapes the response but does not
travel in the request body**, because `CachingAiProvider` hashes the
request body and would otherwise replay an answer produced under a
different setting. Build it with `cache_identity_with(model, &[...])`,
which skips `None` knobs so an unconfigured provider keys on the bare
model name.

What history says must be in there:
- `max_tokens` (`claude`, `openai`) — a reply cut off at 4096 must not
  be replayed after the cap is raised. The test is
  `cache_identity_tracks_the_knobs_outside_the_request` in `claude.rs`.
- `base_url` (`gemini`, `claude`, `openai`, `ollama`, `vllm`) — two
  servers can serve different weights under one model name.
- reasoning/thinking knobs (`thinking`, `effort`, `enable_thinking`).
- anything that changes the *shape* of the reply: `vllm`'s `guided_json`
  and `enable_tools` (`17725fcef7b9`).
- for `goose_cli`, a **digest** of the env table, not its contents,
  because the table may hold an API key (`90b3372332fe`), plus
  `GOOSE_CONTEXT_LIMIT`, which does reach the backend — unlike
  `kiro_cli`'s `context_window_size`, which only budgets the prompt the
  cache already hashes and is deliberately excluded.

**Check on any diff that adds a provider setting:** does the setting
reach the model without appearing in `AiRequest`? If yes it belongs in
`cache_identity`, and there should be a test asserting two configs
produce different identities.

---

## 2. `SessionRunner` and turn accounting

`SessionRunner::run` (`src/ai/session.rs`) is the only loop that drives a
multi-turn conversation. `StageSession` in `src/workflow/stage.rs` is its
main implementation of `LlmSession`; `src/workflows/linux_bug.rs` has
four more.

The loop state a reviewer must keep straight:

```
turns                  bumped at the top of every iteration; > max_turns => bail
validation_attempts    bumped on FormatViolation;  >= max => bail
transient_retries      bumped on RateLimit/Transient;  > max => bail
provider_error_retries bumped on ErrorAction::RetryWithFeedback; > max => bail
```

**Invariant: a retry must not consume a turn.** Every retry path does
`turns = turns.saturating_sub(1)` before `continue`, so the turn budget
measures model *investigation* steps, not attempts. A diff that adds a
new retry path and forgets this silently shortens every review. Note the
asymmetry that already exists: validation uses `>=` and the other two use
`>`, so `max_validation_attempts: 3` permits two feedback retries and the
others permit their full count.

**Invariant: the final turn is tool-free.** `is_final_turn = turns == max_turns`.
On it, `tools` is forced to `None` and a `"TURN BUDGET EXHAUSTED: ... Do
NOT call any tools. Synthesize your final JSON verdict now"` user message
is appended — but only `if is_final_turn && turns > 1`, so a stage
configured with `max_turns: 1` gets tools disabled and no explanation.
If the model emits tool calls anyway they are logged and *ignored*, and
the response falls through to `validate`. This came from `a15f6cd9e318`;
before it, hitting max turns was a hard failure. The test is
`test_session_runner_forces_synthesis_on_max_turns`.

A second-order effect worth checking when a diff changes the validation
path: a validation failure on the final turn decrements `turns`, so the
next iteration is *also* the final turn and appends a *second* turn-budget
message. Bounded by `max_validation_attempts`, but the history grows.

**Invariant: `truncated` is checked before usage is accumulated.** The
`bail!` on `resp.truncated` happens above the `total_prompt_tokens +=`
block, so a truncated turn's tokens never reach `SessionResult::usage`.
The daemon's budget counter in `reviewer.rs` counts them anyway, because
it meters at the IPC boundary before the worker sees the response. If a
diff moves the truncation check, it moves that discrepancy.

**Invariant: a tool error is data, not a failure.** `SessionRunner` does
`session.call_tools(tool_calls).await?` — the `?` means an `Err` out of
`call_tools` ends the stage and the review. This actually happened:
`1646f761f25b`, where the model called `git_read_files` without
`revision` and the run died with `AI review for patch 1 failed with
exception: Missing revision`, after which `local_review` restarted the
entire multi-stage review. Today both the trait default in `session.rs`
and `StageSession::call_tools` convert an error into
`json!({"error": e.to_string()})`; the default was changed to do so in
`d0889e3b4b70`. **The doc comment that warned about this was replaced at
the same time.** So: if a diff adds a new `LlmSession` implementation or
overrides `call_tools`, verify it cannot return `Err` for a bad model
call. `test_call_tools_captures_errors_as_json` and
`test_session_runner_survives_tool_error` pin the defaults, not overrides.

**`history` vs `log_history`.** Two vectors are maintained in parallel.
`history` carries `initial_user_prompt()` and is what the provider sees;
`log_history` carries `log_user_prompt()` (a space-saving substitute) and
is what `SessionResult.history` returns for persistence. Every push must
go to both. A diff that appends to only one produces either a prompt the
logs cannot explain or a log that misstates what the model was asked.

**`handle_provider_error` matches on error text.** Both the trait default
and `StageSession`'s override test `err_str.contains("RECITATION") ||
err_str.contains("blocked")`. That works because
`gemini::translate_ai_response` formats blocked candidates as
`"Gemini candidate blocked (finish reason: {})"` — a plain `anyhow!`, so
`classify_ai_error` calls it `Fatal` and the string match is the only
thing that rescues it. This is a convention with nothing enforcing it. A
diff that rewords those messages, or adds a provider with a differently
worded safety error, silently turns recoverable recitation blocks into
stage failures. `RecitationPolicy::FallbackToFreeForm` additionally sets
`recitation_fallback_active`, which makes `response_format()` return
`None` for the rest of the session.

---

## 3. Token budgeting and truncation

Two separate mechanisms, often confused in review:

- **Tool-output truncation** (`src/ai/truncator.rs`) — cuts a single tool
  result down to a per-tool budget before it enters the conversation.
- **Review-wide budget** (`reviewer.rs`, `review.max_total_tokens`) —
  meters cumulative uncached-input + output across a review and aborts.

### The estimator must be arithmetic, and must over-count

`TokenBudget::approximate_tokens` is `len().div_ceil(BYTES_PER_TOKEN)`
with `BYTES_PER_TOKEN = 3`. Three, not four, deliberately: source, diffs
and lock files sit near three bytes per token; prose near four, so three
over-counts prose and truncates a little early. **Erring high is the
required direction** — an under-count lets a tool result overrun the
context it was given. `test_approximate_tokens_does_not_undercount_real_text`
pins this.

**Do not accept a diff that reintroduces a real tokenizer on the request
path.** This is the most expensive recurring defect in the module:

- `e180c53b8b6e` — `truncate_sequential` binary-searched for the line
  count that fit, re-joining and re-encoding the candidate with BPE on
  every step, then walked the answer down one line at a time doing the
  same. "On a repository-wide git result that is minutes of
  uninterruptible CPU on a runtime worker, which starves the accept loop
  and leaves the process alive but unable to answer." It was replaced by
  a single append-until-full pass.
- `0722ff8118b9` removed `AiProvider::estimate_tokens`, which had no
  production callers but whose implementations each ran a `cl100k_base`
  encode over a whole request.
- `73676f76c9e4` found three survivors — `kiro_cli` and `goose_cli`
  synthesising usage, and `vllm::fit_messages_to_budget` — and removed
  the `tiktoken-rs` dependency outright so nothing can reintroduce it
  without adding the dependency back. **If a diff adds `tiktoken` or any
  encoder to `Cargo.toml`, that is the finding.**
  `test_approximate_tokens_is_cheap_on_large_input` is the guard (200k
  lines must estimate in under 100 ms).

### `Truncator` output must fit its budget, in bytes, on a char boundary

The budget is `max_tokens * BYTES_PER_TOKEN` **bytes**. `288547a80e2c`
fixed `truncate_diff` spending that byte budget with `chars().take()`:

> "A diff of predominantly multi-byte text came back three times the size
> it was allowed, and four for a wider code point. Nothing downstream
> rechecks, so the overrun reaches the context window the budget exists
> to protect."

All three mid-line cases now route through `Truncator::head_of`, which
walks byte offsets (`char_indices().map(|(o, c)| o + c.len_utf8())`),
stops on a character boundary, and reserves room for its own notice
before deciding what fits. The same rule governs
`truncate_sequential`, which reserves `dropped_lines_warning` up front.
Elsewhere in the tree the shared helper is `crate::utils::utf8_prefix`
(from `2f2f3cf53a42`), used by `vllm::fit_messages_to_budget` and
`claude_cli`.

Consequences a reviewer should expect and not "fix":
- When the budget cannot hold both the notice and any content, the
  **notice alone** comes back. `test_truncate_diff_long_line` asserts
  exactly that, with `test_truncate_diff_long_line_keeps_content_at_a_realistic_budget`
  as its companion.
- `truncate_diff`'s `allowed_lines = budget_bytes / 50` is a heuristic
  standing in for "average line length"; when `total_lines <= allowed_lines`
  the input has long lines and there is no line boundary to cut on, so it
  falls to `head_of`.
- The head+notice+tail assembly is re-measured at the end and falls back
  to `head_of` if it still overshoots.

**When a diff changes any truncation path, the check is:** does
`result.len() <= max_tokens * TokenBudget::BYTES_PER_TOKEN` hold for
multi-byte input, and is `result.is_char_boundary(result.len())` true?
Both are asserted in `test_truncate_diff_multibyte_stays_within_the_byte_budget`.

---

## 4. The decorator stack

There are three pass-through decorators, plus the response cache. The
order is not cosmetic.

```
local_review::decorate_provider          reviewer::run_review_tool_with_cmd
  BackoffProvider          (outermost)     BackoffProvider (+ DeadlineBudget)
  ConcurrencyLimitedProvider              ConcurrencyLimitedProvider
  LoggingProvider (if [ai] log_turns)     (turn logging lives in the worker)
  CachingAiProvider (if response_cache)   CachingAiProvider
  concrete provider                       concrete provider
```

The cache is applied by `create_provider_cached(ai, database)`, taking an
`&AiSettings` and an `Option<&str>` location hint, which wraps the raw
provider *before* anything else sees it; the limiters are added later, by
the front end. It consults `ai.response_cache` itself and hands back the
bare provider when that is off or the provider is `stdio-*`, so
`create_provider` and `create_provider_from_ai` never cache at all. What
`database` does with the location is section 5(b).

**Invariant: backoff outside the concurrency limit.** Stated in
`decorate_provider`'s comment: "so a call that is waiting out a rate limit
holds no concurrency permit while it sleeps." `e15f9f84fdce` made the
same point about the daemon, which previously held the semaphore across
its backoff sleep. Inverting these two is a correctness bug that presents
as a review that stalls with idle capacity.

**Invariant: the cache stays innermost.** `BackoffProvider`,
`ConcurrencyLimitedProvider` and `LoggingProvider` each forward
`get_capabilities` and `cache_stats` but **none of them override
`cache_identity`** — they inherit the trait default, which returns
`get_capabilities().model_name`. Today that is harmless because
`CachingAiProvider` wraps the concrete provider directly. A diff that
reorders the stack so the cache sits outside any decorator silently drops
every knob from the cache key — a raised `max_tokens` or `effort` would
then replay the answer recorded under the old one, which is precisely
what `f53ca41c8c98`/`17725fcef7b9` existed to prevent. **If a diff adds a
fourth decorator, it should forward `cache_identity` too, even though
nothing needs it yet.**

**Invariant: one semaphore per run, not per provider.**
`ConcurrencyLimitedProvider::new` takes an `Arc<Semaphore>` the caller
owns. `Reviewer::new` builds `Semaphore::new(llm_permits(concurrency))`
once; `local_review` builds one per run. `llm_permits` returns
`1` for `concurrency < 2` and `concurrency * 3` otherwise — "a
configuration asking for no parallelism stays fully serial rather than
being widened", pinned by
`test_llm_permits_keeps_a_serial_configuration_serial`. The `*3` is
empirical (planning fan-out plus sequential consolidation), not a bound
the workflow guarantees.

**Invariant: `stdio-*` providers skip the limiters.**
`decorate_provider` returns early when `ai.provider.starts_with("stdio-")`,
because that worker reaches the model through the daemon, which throttles
it. A diff that adds a new IPC-shaped provider without the `stdio-`
prefix gets double-throttled or, worse, counted twice.

**What breaks if a provider is constructed outside the stack.** Several
call sites do exactly this on purpose — `api.rs` builds a bare provider
with `create_provider(&state.settings)` for a one-shot request. That path
has no backoff, no concurrency ceiling, no turn logging and no cache. If
a diff adds a new entry point that builds its own provider, ask which of
the four it needs; the common mistake is to get rate-limited in a path
nobody thought was hot.

### `QuotaManager` and `RetryBudget`

`QuotaManager` (`src/ai/quota.rs`) is the *account-wide* gate: a 429 from
any call blocks every concurrent call until the window expires.
- `report_quota_error` caps `retry_after` at `MAX_RETRY_AFTER` (5 min,
  `d51623027d93` — "the quota system blindly trusts whatever duration
  comes from the remote") and only *extends* an existing block, never
  shortens it.
- `report_success` clears `blocked_until` (`909e03e1962f` — without it
  the whole pool stayed blocked after the provider recovered). A diff
  that adds a success path bypassing `report_success` reintroduces that.

`BackoffProvider` treats the two retryable classes differently, and this
distinction is the point of the file:
- `RateLimit` → `quota.report_quota_error(retry_after)`, and the *next
  iteration's* `wait_for_access()` performs the sleep. Global.
- `Transient` → local exponential backoff, `2^(streak-1)` capped at 60×
  `base_delay`, floored by the server's `retry_after` (`c24e3e1aec3a`),
  plus up to 25% jitter. The jitter is load-bearing: a 503 usually
  carries no `Retry-After`, so without it concurrent callers re-collide.
- `Fatal` → returned immediately, no sleep.

`DeadlineBudget` credits quota wait time back to the caller's deadline so
a rate-limit window does not eat a review's activity budget. Its
`last_credit_end` de-duplication matters because several concurrent tasks
wake from the *same* window and each reports the full sleep;
`deadline_budget_credit_wait_deduplicates_overlapping_sleeps` pins it.
With a budget, `max_attempts` is `None` and the loop runs until the
deadline; without one it falls back to `MAX_ATTEMPTS = 6`.

---

## 5. Prompt caching

Two unrelated things share the word "cache". Keep them apart when reading
a diff.

**(a) Provider-side prompt caching** — `claude::apply_cache_control`
(reused by `vertex`), Bedrock's rolling `cachePoints`, Gemini's implicit
caching. `apply_cache_control` marks three breakpoints: the last system
block, the last tool, and the last content block of the last message. The
assumption is that everything *before* a breakpoint is a stable prefix.
**Any diff that reorders `AiRequest::messages`, moves content between the
system prompt and the first user message, or appends to the tool list
invalidates that prefix on every subsequent turn** — the cost shows up as
`cache_creation_input_tokens` where `cache_read_input_tokens` used to be,
not as an error. `6bf6c8725d30`, `8018d8e4f4c1` and `2c30808d2e3b` are
the history of getting this right.

**(b) Sashiko's own response cache** — `CachingAiProvider`
(`src/ai/cache.rs`), a local libsql table keyed on
`SHA256(cache_identity || "\0" || canonical_request_json)`, where the
canonical form has `context_tag` removed and `scrub_thought_signatures`
applied. TTL-swept on construction.

**Where the file lives** is `response_cache_path`: beside the database
when `database.url` names a local file, and `$XDG_DATA_HOME/sashiko/`
otherwise — a remote URL, or no database at all, which is the case for
`sashiko review`. A remote URL must never be treated as a path, since its
credentials would become a directory name, and a bare filename must not
resolve to an empty parent.

Assumptions a diff can break:
- **`context_tag` must stay out of the key.** It carries `[ps:123 p:1 s:4]`,
  which differs per patch and would make every entry a miss. Any new
  nondeterministic field added to `AiRequest` must be stripped here too.
- **A hit sets `cached_tokens = prompt_tokens`** and ignores the stored
  count, because the whole prompt was served locally (`f399f9a3b7b4`).
- **A write failure is swallowed** (`let _ = self.conn.execute(...)`) —
  intentional; a cache write must not fail a completed call.
- The insert binds `caps.model_name` to **both** the `provider` and
  `model` columns. Those columns are descriptive only (the key does not
  use them), but a diff that starts reading `provider` from this table is
  reading the model name.

---

## 6. What must be logged, and why partial logging is worse than none

There are three logging layers, and they answer different questions.

1. **`LoggingProvider`** (`src/ai/logging_provider.rs`) — per-turn INFO
   logs, gated on `[ai] log_turns`, wired in `local_review::decorate_provider`.
   It exists because turn logging previously lived only in the daemon's
   IPC multiplexer, so local reviews running the same worker produced
   nothing (`e75c257e5618`). It logs the last outgoing message (role,
   message count, tool-call names or a 300-char preview), then the
   response text (500-char preview), each tool call with a 200-char
   argument preview, and `in/out/cached` token counts.
2. **`LOG_CONTEXT` / `get_log_prefix`** (`src/ai/mod.rs`) — a
   `tokio::task_local` string the daemon scopes around each
   `generate_content` call so concurrently-running stages stay
   attributable. Warnings inside the providers (`gemini`'s MAX_TOKENS
   warning, `BackoffProvider`'s backoff warning, `QuotaManager`'s wait
   message) all prefix with it. **A provider that logs without
   `get_log_prefix()` produces lines that cannot be assigned to a patch
   when several reviews run at once**, which in practice means they get
   ignored.
3. **`Database::create_ai_interaction`** — the durable record:
   `provider`, `model`, `input_context`, `output_raw`, `tokens_in`,
   `tokens_out`, `tokens_cached`. Written from `reviewer.rs` alongside
   `complete_review`, whose `logs` column gets the session history
   *after* `scrub_thought_signatures`.

Why partial logging defeats the purpose: these records are the only
reconstruction of a review after the fact — the worktree is deleted, the
worker process is gone, and the model is nondeterministic. A turn that is
logged without its usage cannot be costed; a usage row without its input
cannot be explained; a history with thought signatures in it cannot be
stored (`ec08a998d1ab`). And the specific asymmetry to watch for:
**`LoggingProvider` logs nothing on the error path** — it uses `?` on
`self.inner.generate_content(request).await`, so a failed turn leaves a
request line with no response line. If a diff adds a new failure mode,
check whether the failed turn is recoverable from the logs at all.

Redaction is a standing requirement: `53a8caa96d2f` ("Redact secrets in
transport and API errors across AI clients") and `f9c953623496`. A new
provider that formats an error containing its base URL with embedded
credentials, or echoes a request header, is a finding. Note also
`e222e324f943`: transport errors must be formatted `{:#}` so the reqwest
source chain survives, not `e.to_string()`.

---

## 7. Error classification: retryable vs fatal

```rust
pub enum AiErrorClass {
    Fatal,
    RateLimit  { retry_after: Duration },   // account-wide
    Transient  { retry_after: Duration },   // this call only
}
```

`classify_ai_error(&anyhow::Error)` is a **hand-maintained list of
downcasts** in `src/ai/mod.rs`:

```rust
RemoteAiError, openai::OpenAiCompatError, claude::ClaudeError,
claude_cli::ClaudeCliError, gemini::GeminiError,
worker::prompts::ReviewError, ollama::OllamaError, vllm::VllmError
=> otherwise AiErrorClass::Fatal
```

**This is the single highest-value check for a diff that adds a
provider.** A new error type not added to that list classifies as `Fatal`
for everything, including its own 429s — the provider loses all backoff
and every rate limit becomes a failed review. Note who is *not* on the
list: `bedrock`, `vertex`, `codex_cli`, `copilot_cli`, `devin_cli`,
`kiro_cli`, `goose_cli`. Those providers either reuse a listed error type
or genuinely have no retryable failures; verify which before assuming
either.

Rules the existing implementations follow:
- HTTP status mapping goes through `classify_status_code`: 429 →
  `RateLimit`; 500/502/503/504 and Anthropic's 529 → `Transient`;
  anything else → `None`, and the caller falls back to `Fatal`. Reuse it
  rather than re-deriving the set.
- `DEFAULT_RETRY_AFTER` is 60s and is what `classify_status_code` attaches
  when the server suggested nothing.
- Auth, permission and malformed-request errors are `Fatal` in every
  provider (`ClaudeError::AuthenticationError`,
  `GeminiError::PermissionDenied`, `ClaudeCliError::Parse`). Retrying
  them burns the attempt budget and delays the real failure.
- A spawn failure is `Fatal` (`ClaudeCliError::Spawn`) but a timeout or
  wait error is `Transient` with a 30s floor.

**What `Fatal` actually means at each consumer**, because it is not
uniform:
- `BackoffProvider` returns it immediately.
- `SessionRunner` hands it to `session.handle_provider_error`, which may
  still convert it into `RetryWithFeedback` — this is how recitation
  blocks recover. So `Fatal` means "do not retry the same request
  unchanged", not "give up".
- The daemon writes it into the stdio protocol as
  `RemoteAiErrorPayload { message, class }`, which the worker decodes
  into a `RemoteAiError` whose `ai_error_class()` returns the class
  verbatim. **The class survives the process boundary; the concrete error
  type does not.** A diff that adds a class must update both
  `AiErrorClass`'s serde representation (`#[serde(tag = "class")]`, with
  `retry_after_secs` via the `duration_secs` module) and every consumer's
  `match`, which is exhaustive and will at least fail to compile.

`ReviewError` (`src/worker/prompts.rs`) is on the downcast list so that
budget and format failures are `Fatal` and fail fast rather than
retrying; `test_*_downcast*` in that file pins that plain `anyhow` errors
do *not* match, so they stay retryable.

---

## 8. The stdio/IPC path

`stdio-gemini` / `stdio-claude` do not talk to a model; they multiplex
`AiRequest`s over the worker's stdin/stdout to the daemon. Three statics
in `src/ai/mod.rs` — `IPC_REGISTRY`, `IPC_WRITER`, `IPC_READER` — are
**process-wide on purpose**:

> `a5271c3faf67`: each client carried its own `IpcRegistry` and spawned
> its own reader on the process's single stdin, so providers numbered
> transactions from 1 in parallel and competed for response lines —
> `CRITICAL PROTOCOL ERROR: Unsolicited response received for tx_id: 7!`
> — and two readers on one stdin can split a line between their buffers.

Things a diff must not undo:
- `ensure_stdin_reader` holds the `JoinHandle`, not a bool, so a reader
  that stopped (EOF, read error, dropped runtime) can be told apart from
  a running one and replaced. An earlier attempt at this used a `Weak`
  registry (`b873d5e9474e`) and was superseded.
- `IpcRegistry::register` checks `closed` **under the pending lock**, and
  `abort_all` sets it under the same lock, so a registration racing
  shutdown either lands before the drain or fails. `is_closed()` is
  explicitly advisory.
- Protocol violations call `std::process::exit(1)` rather than returning.
  That is deliberate (a mismatched tx_id means responses are being
  delivered to the wrong caller), and loud. Do not soften it into a
  logged warning.

---

## Checklist for a diff that touches `src/ai/`

- [ ] New provider: is its error type in `classify_ai_error`'s downcast
      list? If not, every failure is `Fatal`.
- [ ] New provider: does it set `AiResponse::truncated` from a real stop
      reason, or hardcode `false`? If hardcoded, is that stated?
- [ ] Usage parsing: `cached_tokens <= prompt_tokens`, and
      `prompt_tokens` includes the cached prefix. Check against the
      provider's API docs, not the neighbouring provider.
- [ ] Any new setting that shapes the reply but not the request body: is
      it in `cache_identity`, with a test?
- [ ] Any tokenizer added to the request path, or `tiktoken` reappearing
      in `Cargo.toml`? Reject.
- [ ] Truncation change: result bounded by `max_tokens * BYTES_PER_TOKEN`
      **bytes**, cut on a char boundary, notice reserved up front.
- [ ] New retry path in `SessionRunner`: does it decrement `turns`? Is it
      bounded by a counter?
- [ ] New `LlmSession`: can `call_tools` return `Err`? It must not.
- [ ] Decorator order unchanged (backoff outermost, cache innermost), and
      any new decorator forwards `cache_identity` and `cache_stats`.
- [ ] New logging: prefixed with `get_log_prefix()`, no secrets, errors
      formatted `{:#}`.
- [ ] Subprocess providers: `.kill_on_drop(true)` (`c3e31b737178`), prompt
      via stdin not argv (`10a28b521c89`, MAX_ARG_STRLEN), and any agent
      CLI pinned to completion-only mode *after* user-supplied env is
      applied (`6fe1ff0345ac`), with its state directories redirected out
      of `$HOME` (`c12a64a05f34`).
