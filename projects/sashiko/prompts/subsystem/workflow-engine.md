# Sashiko Workflow Engine: Contracts a Stage Must Honour

Covers `src/workflow/` (`engine.rs`, `graph.rs`, `stage.rs`, `prompt.rs`,
`policy.rs`, `output.rs`, `events.rs`) and the parts of `src/ai/session.rs`
(`SessionRunner`, `LlmSession`) that a stage inherits. The only consumer of
the declarative engine today is
`src/workflows/linux_patch_review.rs`; `src/workflows/linux_bug.rs` drives
`LlmSession` by hand and only borrows
`crate::workflow::output::parse_json_from_text`. If a patch changes engine
behaviour, `linux_patch_review.rs` is the whole blast radius — check it.

What the engine guarantees, and nothing more:

- A stage sees an immutable `&S` while it runs, and hands back a closure that
  mutates `&mut S` afterwards. Concurrency safety follows from that split.
- Within one `WorkflowStep`, state mutations are applied in *declaration
  order*, never completion order.
- Steps in `Workflow::steps` run strictly in order, so a reducer has
  committed before the next step's condition or prompt reads state.
- An included file's bytes are never scanned for template directives or
  variable placeholders.

Everything else — that a reducer runs at all, that a tool name in a
`ToolScope::Selected` exists, that a schema field is consumed downstream — is
convention, and most of it is unenforced. Sections below say which is which.

## 1. `Stage` vs `ExecutableStage`, and why the mutation is deferred

`Stage<S, T>` (`stage.rs`) is the declarative record: `name`, optional
`system_prompt`, `user_prompt`, `output_format: OutputFormat<S, T>`,
`policy: StagePolicy`, `reducer: StageReducer<S, T>`, `skip_if`.
`ExecutableStage<S>` is the type-erased trait the engine actually drives; the
blanket impl `impl<S, T: DeserializeOwned> ExecutableStage<S> for Stage<S, T>`
is what erases `T`. `WorkflowStep` stores `Box<dyn ExecutableStage<S>>`, so
by the time the engine sees a stage, `T` is gone and the only way output can
reach state is through the reducer captured inside the returned closure.

```rust
pub type StateMutation<S> = Box<dyn FnOnce(&mut S) + Send>;

async fn execute_isolated(&self, env, state: &S, event_cb)
    -> Result<(StageOutcome, StateMutation<S>)>;
```

`ExecutableStage::execute` is a provided method: it calls `execute_isolated`
and immediately applies the mutation. Sequential steps use it; parallel
batches do not.

**Why `&S` and not `&mut S`.** `execute_parallel_batch` in `engine.rs` builds
`stages.iter().map(|stage| stage.execute_isolated(env, state, event_cb))` and
joins them. Every future reborrows the same `&mut S` as `&*state`. Shared
reborrows coexist; a `&mut S` reborrow would not. So the moment someone
changes `execute_isolated` to take `&mut S`, or adds a `&mut S` parameter to a
stage hook that parallel code calls, `execute_parallel_batch` stops compiling
— the borrow checker is the enforcement, not review discipline.

**What actually breaks if a stage mutates shared state directly** (e.g. by
capturing an `Arc<Mutex<...>>` in a reducer or a `with_var` extractor, which
*does* compile and sidesteps the borrow checker):

1. *Prompt rendering becomes racy.* `execute_isolated` renders both prompts
   from `state` at stage start (`sys.render_for_model(state, env.base_dir)`,
   `self.user_prompt.render_for_model(...)`). Seven analysis stages fan out
   concurrently in `build_linux_patch_review_workflow_with_options`. A stage
   that mutates shared state while a sibling is rendering changes what that
   sibling's prompt says, depending on scheduling.
2. *Results stop being reproducible.* `all_concerns` order is currently
   fixed by the `stages` slice order, because mutations are applied in a `for`
   loop after the join. The `verification` stage serialises `all_concerns` into
   its prompt verbatim (`serde_json::to_string_pretty(&s.all_concerns)`), so
   any reordering changes the verification prompt and therefore the review output.
   Two runs of the same patch would diverge for no reason visible in a log.
3. *`BestEffort` stops being partial-failure-safe.* A stage that fails
   mid-run has already published half its mutations; the engine's error
   handling assumes a failed stage published none.

**Check in a diff:** any new `Arc<Mutex<_>>`, `RefCell`, `OnceCell`, atomic,
or captured channel sender inside a `reduce(...)`, `with_var(...)`,
`include_files_from_state(...)`, `skip_if(...)`, or a hand-written
`ExecutableStage` impl. The reducer closure is the only sanctioned write path.

**Check in a diff:** a hand-written `impl ExecutableStage<S>` that ignores
`skip_if`, never emits `StageStarted`/`StageFinished`, or returns a mutation
that captures `state` data read at a *different* time than the prompt was
rendered.

## 2. `ParallelPolicy::FailFast` vs `BestEffort`

Both arms of `execute_parallel_batch` collect `(StageOutcome, StateMutation)`
pairs and only then apply mutations. They differ in what a failure costs.

`FailFast` uses `futures::future::try_join_all(futures).await?`. On the first
`Err`:

- The `?` propagates out of `execute_parallel_batch` and out of
  `WorkflowEngine::execute`. The whole workflow ends.
- **No mutation from the batch is applied — not even from stages that
  succeeded.** Their `StateMutation` values are dropped with the `Vec`.
- Token counts from the successful stages are lost too; `outcome.tokens_in`
  and friends are only incremented inside the success loop.
- Still-running futures are dropped, i.e. cancelled at their next await
  point mid-LLM-call.

`BestEffort` uses `join_all`, then `stages.iter().zip(results)`. A failed
stage produces `warn!("Parallel stage '{}' failed under BestEffort policy: {}",
stage.name(), err)` and nothing else — no event, no state marker, no field on
`WorkflowOutcome`. Downstream stages cannot tell that a stage was dropped;
they see fewer concerns and cannot distinguish that from a clean stage.

The review pipeline uses `BestEffort` for both its analysis fan-out and its
per-candidate `post-verification` fan-out
(`build_linux_patch_review_workflow_with_options` and
`build_sashiko_patch_review_workflow_with_options`), deliberately: one
misbehaving analyst or one `post-verification-N` stage exhausting its turn or
validation budget on a single candidate batch must not abort the entire review
and discard the six other analysts' work or the already-validated
`state.findings` from `verification` and sibling `post-verification` stages
(which `FailFast` would throw away).

**Check in a diff:**
- A switch from `BestEffort` to `FailFast` on a batch whose stages have
  independent value (such as the analysis or `post-verification` fan-outs). You
  are trading sibling stages' partial results (and any prior `verification`
  findings) for a hard abort on one.
- A switch from `FailFast` to `BestEffort` on a batch where a later step
  *requires* a specific stage's reducer to have run. There is no per-stage
  success signal, so the later step must be written to tolerate the default
  value, and an `early_exit_if` guard may be needed.
- Any code that tries to consume partial results from a `FailFast` batch. The
  mutations do not exist on the error path.
- A rewrite to `FuturesUnordered` / apply-on-completion. That silently makes
  reducer order nondeterministic (see §1.2).

## 3. Reducer / next-reader ordering, and `early_exit_if`

`WorkflowEngine::execute` iterates `workflow.steps` sequentially. For
`WorkflowStep::Stage`, `stage.execute(env, state, event_cb)` applies the
mutation before the loop advances. So the ordering rule is simple and
absolute: **step N's reducer has fully run before step N+1 renders a prompt
or evaluates a condition.** There is no lazy or deferred write.

The corollary is the dangerous part: a later reducer can clobber an earlier
one's work, and nothing detects it. That is exactly commit `c419f184205b`
("workflow: preserve pre-existing bug concerns across review and benchmark").
Both `verification_stage` and `post_verification_stage_for_batch` record
validated findings into `state.findings` (patch defects) and `state.concerns`
(pre-existing defects) via `record_verified_findings`:

```rust
.reduce_with_outcome(|state, mut out: VerificationOutput, outcome| {
    enrich_verification_output(state, &mut out, outcome, analysis_stage_by_name);
    record_verified_findings(state, out.findings);
    state.hard_cases = out.hard_cases;
    state
        .deduplicated_dismissed_concerns
        .extend(out.dismissed_concerns);
})
```

Notice that both `verification_stage` and `post_verification_stage` /
`post_verification_stage_for_batch` append to `state.findings`, `state.concerns`
(via `record_verified_findings`), and `state.deduplicated_dismissed_concerns`
(via `.extend(out.dismissed_concerns)`) without clearing entries already
recorded in state. `test_verification_stage_preserves_preexisting_concerns`
and `test_post_verification_stage_preserves_existing_state` seed
`state.findings`, `state.concerns`, and `state.deduplicated_dismissed_concerns`
before calling `(stage.reducer)(&mut state, output)` and assert both the seeded
and the new entries survive.

**Check in a diff:** any reducer that assigns (`state.x = ...`) or clears a
state field rather than extending it. Ask *who else writes this field*. In
`LinuxPatchReviewState`, `findings`, `concerns`, and
`deduplicated_dismissed_concerns` are written by both `verification_stage` and
each parallel `post_verification_stage_for_batch`, while `all_concerns` /
`all_dismissed_concerns` have one writer per analysis stage via
`append_stage_items_with_prompts` /
`append_stage_dismissed_concerns_with_prompts`. A new writer of a multi-writer
field needs a test in the shape of the ones above — call
`(stage.reducer)(&mut state, out)` directly on a pre-populated state.

`early_exit_if` (`graph.rs`, `WorkflowStep::EarlyExitIf`) evaluates its
condition against the state as of that point in the list, logs, emits
`WorkflowEvent::EarlyExitTriggered`, sets `outcome.early_exit` /
`early_exit_reason`, and `break`s the step loop. `WorkflowFinished` is still
emitted afterwards. `Branch` propagates a child workflow's `early_exit` up and
breaks the parent loop; `Parallel` and `DynamicParallel` do **not** inspect
`early_exit`.

**Check in a diff:** an `early_exit_if` inserted between a stage and its
reader must test the field that stage's reducer writes, not a neighbouring
one. The Linux review pipeline pairs them exactly:
`all_concerns.is_empty() && all_dismissed_concerns.is_empty()` after the
analysis fan-out, and `findings.is_empty()` after the `verification` +
`post-verification` `DynamicParallel` step (while `sashiko_patch_review.rs`
uses `.skip_if(...)` on `verification_stage` and `report_stage` so its
trailing `summary_stage` still runs when there are zero findings). A patch
that makes a stage write a differently named field, or that reorders the
steps, must move the guard with it, or the guard tests a field that is still
at its `Default` value and exits every run.

**Check in a diff:** `build_linux_patch_review_workflow` has a structural
test, `test_build_workflow_graph_structure`, asserting
`workflow.steps.len() == 6`. A patch that adds or removes a step and updates
that number without a matching reason in the commit message deserves a second
look.

## 4. `skip_if` and the event contract

`skip_if` is evaluated at the top of `execute_isolated`, **before**
`WorkflowEvent::StageStarted` is emitted. A skipped stage emits no events at
all, returns an all-zero `StageOutcome`, and returns `Box::new(|_| {})` as its
mutation — its reducer never runs, so every field it would have written stays
at its `Default`.

`prescreen_stage()` and `planning_stage()` both carry
`.skip_if(|s| s.manual_stages.is_some())`. So with `--stages`,
`state.selected_guides` stays empty (no subsystem guides are inlined into any
system prompt) and `state.planned_stages` stays empty.
`resolve_analysis_stages_with_options` handles that by falling back to
`manual_stages`.

Commit `5840cfaa6c08` is the bug this shape causes. The progress bar's
denominator came from an event emitted when the planner *finished*; a review
with an explicit stage list skipped the planner, so the event never fired and
the bar had no plan. The fix moved the announcement into the engine as
`WorkflowEvent::ParallelResolved`, emitted by
`WorkflowStep::DynamicParallel` *after* the resolver runs and
unconditionally — including when the resolver returned nothing.

**Check in a diff:** new UI, metrics, or database writes hung off
`StageStarted`/`StageFinished` for a stage that can be skipped. Derive from
`ParallelResolved` or from state instead. `src/worker/prompts.rs`'s
`is_counted_stage`, `planned_stages_from`, and
`refine_planned_stages_with_post_verification` are the reference: they read the
stage tables in `linux_patch_review.rs` rather than the event stream's shape,
and emitting `WorkerProgressEvent::ReviewStarted` a second time when
`post-verification` resolves only updates `planned_stages` (the progress bar
denominator in `main.rs` and `local_review.rs`) without resetting
`completed_stages` or `active_stages`.

**Check in a diff:** a new `skip_if` on a stage whose reducer initialises a
field that a later `early_exit_if` tests. Skipping now means exiting.

## 5. The turn / validation / retry loop (`SessionRunner::run`)

`Stage::execute_isolated` wraps itself in a `StageSession` and hands it to
`SessionRunner::new(provider).with_max_turns(policy.max_turns)
.with_max_validation_attempts(policy.max_validation_attempts)`. Note what is
*not* configurable from `StagePolicy`: `max_transient_retries` (5) and
`max_provider_error_retries` (3) keep their `SessionRunner::new` defaults.

Counters and their exact semantics:

- `turns += 1` at the top of each iteration; `turns > max_turns` bails with
  "Session exceeded max turns limit".
- `is_final_turn = turns == max_turns`. On the final turn (and only when
  `turns > 1`) a "TURN BUDGET EXHAUSTED" user message is appended and
  `tools` is forced to `None` for that request, **overriding `ToolScope`**.
  If the model emits tool calls anyway they are logged and ignored, and the
  response goes straight to `validate`.
- Transient / rate-limit errors sleep and then do
  `turns = turns.saturating_sub(1)` — they do not consume the turn budget.
- A recitation retry (`ErrorAction::RetryWithFeedback`) also does
  `turns = turns.saturating_sub(1)`.
- A validation failure does `turns = turns.saturating_sub(1)` too.

So `max_turns` bounds *productive* turns, and the true worst-case request
count for a stage is roughly `max_turns + max_validation_attempts +
max_transient_retries + max_provider_error_retries`. A patch that raises
`max_validation_attempts` to "give the model more chances" is buying extra
provider calls that no turn budget caps.

`max_validation_attempts` uses `>=`, not `>`:

```rust
validation_attempts += 1;
if validation_attempts >= self.max_validation_attempts { anyhow::bail!(...) }
```

With the default of 3, the model gets feedback after the 1st and 2nd
violations and the 3rd violation is fatal — **two retries, not three**. A
patch that sets `max_validation_attempts: 1` disables retry entirely: the
first violation bails and `format_validation_feedback` is never called.

`resp.truncated` is fatal immediately, before validation, with "LLM output was
truncated by provider". There is no retry path for truncation.

`SessionResult.history` is `log_history`, not `history`. The two differ only
in their first message: `history` starts with `session.initial_user_prompt()`
(files expanded) and `log_history` with `session.log_user_prompt()`
(`render_for_log`, directives left as `@path` tokens). Every subsequent
message — assistant, tool result, validation feedback, recitation reminder —
is pushed to both. That is what keeps expanded guide files out of the stored
interaction log while keeping the log faithful about everything else.

**Check in a diff:** a new message pushed to `history` but not `log_history`
(or vice versa). GEMINI.md's observability rule requires every input and
output be recorded; a one-sided push silently drops it from the record.

**Check in a diff:** a new `LlmSession` impl that does not override
`call_tools`. See §6.

## 6. Duplicate tool calls, and tool errors that must not be fatal

Two defects here, both real, both regressions from the hand-rolled loop the
declarative engine replaced.

`1646f761f25b` — "report a rejected tool call to the model instead of
aborting". `StageSession` implemented `call_tool` but not `call_tools`, so it
inherited the `LlmSession` default, which propagates a tool error with `?`.
Any call the toolbox rejected ended the stage and with it the entire review
("AI review for patch 1 failed with exception: Missing revision"), after
which `local_review` restarted a full multi-stage run. The fix converts every
tool error into a value the model can read:

```rust
let res = match tools.call(&call.function_name, call.arguments).await {
    Ok(v) => v,
    Err(e) => json!({ "error": e.to_string() }),
};
```

`c9f5570588fd` — "run a batch of tool calls concurrently". The inherited
default also ran the batch sequentially. The fix clones the `ToolBox` handle
per call and uses `futures::future::join_all`. `join_all` preserves input
order, and results are additionally written back by index
(`results[idx] = Some(res)`), which matters on the Gemini path where the tool
call id never reaches the wire and a result is matched to its call by
position. `test_batched_tool_results_keep_their_call_order` asserts the ids
come back as `call_0, call_1, call_2`.

`bdcea46ef7cb` / `a6aa86e2e08b` — the duplicate guard. `StageSession` carries
`last_tool_call: Option<(String, Value)>`. Exact semantics, all covered by
tests in `stage.rs`:

- Only a *consecutive* repeat is blocked — same `function_name` **and**
  byte-equal `arguments` as the immediately preceding call that actually ran.
  `[a, b, a]` runs all three (`test_non_consecutive_duplicate_tool_call_runs`).
- The guard is per session, not per batch, so a repeat on the next turn is
  blocked too (`test_duplicate_tool_call_is_blocked_across_turns`).
- A blocked call returns
  `{"error": "Duplicate tool call blocked. Please change parameters or use a
  different tool."}` and **does not** update `last_tool_call`, so a third
  identical call is blocked as well.
- `call_tools` walks the whole batch first, recording blocked repeats in
  place, and only then runs the survivors concurrently. Blocking is decided
  against the batch's own running `last_tool_call`, so `[a, a]` blocks the
  second.
- Equality is `serde_json::Value` equality. `serde_json` is used without the
  `preserve_order` feature (`Cargo.toml`), so object maps are `BTreeMap` and
  key order in the model's output does not affect the comparison.
- Both `call_tool` and `call_tools` implement the guard; `a6aa86e2e08b` ported
  it to `call_tool` specifically so a future session type cannot inherit half
  of it.

**Check in a diff:** a new `LlmSession` implementation, or a refactor that
moves `StageSession`'s tool handling. It must (a) override `call_tools`,
(b) convert `ToolBox` errors to `{"error": ...}` rather than `?`, (c) preserve
result order relative to calls, (d) carry the duplicate guard in both
methods. Losing any one of those is a silent regression: the review still
runs, it just dies intermittently, or loops, or misattributes tool output.

**Check in a diff:** anything that makes the duplicate guard's key coarser
(e.g. comparing only `function_name`) — legitimate repeated calls to the same
tool with different arguments within one batch would be blocked, and
`ToolCallingProvider::probing` in the tests exists precisely because distinct
arguments must survive.

## 7. `ToolScope`

`StageSession::tools()` maps `ToolScope::None` → `None`,
`ToolScope::All` → `self.tools.get_declarations_generic()`,
`ToolScope::Selected(names)` → that list filtered by `names.contains(&t.name)`.

Nothing validates that a name in `Selected` corresponds to a registered tool.
A typo silently yields a shorter tool list and the stage then fails in a way
that looks like model incompetence. `ToolScope::Selected` has no user in the
current pipeline — every stage is `All` except `prescreen_stage` and
`planning_stage`, which are `ToolScope::None` with `max_turns: 1`.

**Check in a diff:** a first use of `ToolScope::Selected`. Each name must
match a `LlmTool::name()` registered on the `ToolBox`, and there is no test
that will tell you otherwise — ask for one.

**Check in a diff:** granting tools (`ToolScope::All`) to a stage that also
has `max_turns: 1`. `is_final_turn` is true on turn 1 when `max_turns == 1`,
but the "TURN BUDGET EXHAUSTED" message is only injected when `turns > 1`, so
the model gets no tools and no explanation of why.

## 8. `PromptTemplate`: the Template/Included boundary is a security boundary

`render_for_model` builds `Vec<Segment>` where
`enum Segment { Template(String), Included(String) }`, and:

- `place(segments, directive, block)` scans **only** `Segment::Template`
  pieces for the directive, splits the matching one, and inserts the file's
  content as a `Segment::Included`.
- `assemble()` runs `substitute_vars` **only** on `Segment::Template` pieces;
  `Segment::Included` blocks are pushed verbatim.

Two properties fall out, and both are load-bearing:

1. **A variable's value can never place a file.** Sashiko reviews patches
   from public mailing lists. A diff containing the literal text
   `@include("subsystem/locking.md")` or `@includes` must stay text. This is
   why placement happens before substitution and why `place` never looks at
   an `Included` segment. `test_a_variable_value_cannot_place_a_file` asserts
   exactly this: a diff carrying both directives renders them literally, and
   the real guide is still placed exactly once.
2. **An included file's content is never scanned for `{{var}}`.** A vendored
   guide under `third_party/prompts/kernel/` that happens to contain
   `{{target_commit_diff}}` does not get the diff spliced into it.

This came from `2baf858f4e03`, which also fixed the functional half: before
it, every included file was appended to the end of the prompt and the
directive text was left in place. The system prompt's
`<global_review_guidelines>` block closed with nothing inside it and the
guides arrived later with no framing, and the pre-screen's literal
`@include("subsystem/subsystem.md")` reached the model as text.

Mechanics you must get exactly right:

- The directive is `format!("@include(\"{}\")", path.display())`. The string
  in the template must **byte-match** the path passed to `include_file`.
  `@include('x.md')`, `@include( "x.md" )`, or a template saying
  `subsystem/locking.md` while the builder says `locking.md` all fail to
  match. The failure is quiet: the block is appended as `trailing` *and* the
  literal directive survives into the model's prompt.
- Dynamic inclusions (`include_files_from_state`) have no path in the
  template, so they render at the `@includes` marker
  (`DYNAMIC_INCLUDE_MARKER`). `linux_system_prompt` puts that marker inside
  `<global_review_guidelines>`.
- `place` replaces the **first** matching template segment only. A template
  naming the same directive twice leaves the second literal.
- A static file that does not exist yields an empty block, and the directive
  is still consumed — `test_a_directive_for_a_missing_file_is_still_consumed`.
  A missing directory does the same. A dynamic path that does not exist is
  skipped silently, with no marker consumed on its behalf beyond the shared
  `@includes`.
- A static file that exists but fails to read is a hard error
  (`with_context("Failed to read file: ...")`) and fails the stage.
- `base_dir.join(path)` performs **no normalisation**. `PathBuf::join` with an
  absolute component discards the base entirely, and `..` traverses. The
  prompt engine does not defend against this; callers must.

That caller-side defence is `f81f8a2edd9f`, "workflows: reject prescreen
guides that name a path". The pre-screen stage asks the model for guide file
names which become paths under the prompt tree and are inlined into every
later stage's system prompt and into the stored interaction log. The model
picks them while reasoning about a patch from a public list, so the choice is
steerable by anyone who can send mail. The reducer now drops anything that is
not a plain file name:

```rust
let plain = !name.is_empty()
    && !name.contains('/')
    && !name.contains('\\')
    && !name.contains("..");
if !plain {
    tracing::warn!("Ignoring prescreen guide with a path in its name: {name}");
}
```

Note it warns rather than failing the review — a bad name is the model
getting the format wrong, not a reason to abandon the work.

**Check in a diff:**
- Any new path that reaches `include_file` / `include_files_from_state` /
  `InclusionDirective` from **model output**, from a patch, or from any other
  untrusted source. It needs the plain-name filter above, or an equivalent
  allow-list. There is no central chokepoint that will catch you.
- Any refactor that substitutes variables before placing files, matches
  directives against the assembled string, or makes `place` consider
  `Segment::Included`. Each one re-opens the injection
  `test_a_variable_value_cannot_place_a_file` guards. That test is the only
  thing standing between a patch and the prompt tree.
- A new `@include(...)` in a template text without a matching
  `.include_file(...)` on the same `PromptTemplate` (the directive renders
  literally), or a `.include_file(...)` whose path does not appear in the
  template (the file is appended at the end, outside whatever block was
  meant to frame it — and `render_for_log`'s `@path` token lands there too).
  Nothing tests this for the real pipeline templates; the tests in
  `prompt.rs` all use synthetic ones.
- `render_for_log` must stay cheap. It substitutes variables (so the log has
  the full diff) but emits `@path` tokens instead of file contents. A patch
  that makes `render_for_log` expand files inflates every stored interaction
  log by the size of the guide tree.

### Single-pass variable substitution

`substitute_vars` scans each `Segment::Template` in a single pass from left to
right (`while let Some(pos) = rest.find("{{")`), replacing each bound `{{key}}`
placeholder and advancing `rest` past the placeholder without ever re-scanning
the inserted value (`test_a_variable_value_is_not_substituted_again`). As a
result, an untrusted variable value (such as a patch diff, series context, or
model-generated JSON in `aggregated_concerns` / `candidate_hard_cases`) that
contains another variable's `{{placeholder}}` keeps that text literally,
regardless of `with_var` registration order.

**Check in a diff:** any refactor of `substitute_vars` in `src/workflow/prompt.rs`
that replaces single-pass scanning with sequential `text.replace(...)` over
`self.vars`. Sequential replacement would re-scan earlier variable values for
later variable placeholders and break `test_a_variable_value_is_not_substituted_again`.

## 9. `OutputFormat` parsing traps

`parse_json_from_text` tries, in order: `clean_json_string` then `from_str`,
raw `from_str`, then `find_json_candidates(raw_text)` iterated **in reverse**
— so when the model emits several top-level objects, the **last** one that
deserialises wins.

Combine that with `#[serde(default)]`. `StageConcernsOutput` requires the
top-level `concerns` key (marking only `dismissed_concerns` with
`#[serde(default)]`), `VerificationOutput` requires all three top-level keys
(`findings`, `hard_cases`, and `dismissed_concerns`), and
`PostVerificationOutput` requires the top-level `findings` key (marking only
`dismissed_concerns` with `#[serde(default)]`), so inner `locations` objects in
malformed outer JSON cannot accidentally deserialize as a stage output
(`test_stage_outputs_do_not_match_inner_location_on_malformed_outer_json`), and
in `VerificationOutput` a typo in `hard_cases` or `dismissed_concerns` is
rejected by Serde rather than silently defaulting to an empty vector. Their
custom validators
(`validate_concerns_output`, `validate_verification_stage_output`, and
`validate_post_verification_output`) then enforce structural requirements on the
parsed elements and prevent raised concerns or dismissed concerns from being
silently dropped.

`PrescreenOutput` and `PlanningOutput` do not use `#[serde(default)]`, so
their required keys genuinely gate parsing. That asymmetry is worth knowing
before you conclude that "the schema validates it".

Other traps in `output.rs`:

- `with_validator` and `with_feedback_formatter` match only
  `Self::Json { .. }`. Called on an `OutputFormat::Text`, they compile
  (`String: DeserializeOwned`) and **silently do nothing**. Text validation
  must go through `text_with_validator`, as `report_stage` does.
- `OutputFormat::json()` has `schema: None`, so `StageSession::response_format`
  returns `None` and the provider is never told the shape. Only
  `json_with_schema` sets it — today that is `prescreen_stage` and
  `planning_stage` alone.
- `format_feedback` for a `Json` variant with no `feedback_formatter` falls
  through to the generic "Previous attempt was rejected: {}. Please correct
  your output format." Both `verification_stage` and
  `post_verification_stage_for_batch` attach explicit feedback formatters.
- `Self::Text`'s `validate` downcasts a `String` into `T`; this is sound only
  because `text()`/`text_with_validator` live in `impl<S> OutputFormat<S, String>`.
  A patch that widens that impl breaks the invariant into a runtime
  `"Failed to downcast text output"`.

**Check in a diff:** a new output struct with `#[serde(default)]` on all
fields and no `with_validator`. Add a validator that rejects the empty /
missing-key case, or the stage will report "nothing found" on malformed
output. That the model produced garbage will not appear anywhere.

## 10. `RecitationPolicy`

`StageSession::handle_provider_error` is only reached when
`classify_ai_error` returns `AiErrorClass::Fatal`, and it only consults the
policy when `error.to_string()` contains `"RECITATION"` or `"blocked"`. Every
other fatal error returns `ErrorAction::Fail` regardless of policy. It is a
substring match on the flattened error chain — coarse, and it will match an
unrelated error whose message happens to contain "blocked".

- `Fail` → `ErrorAction::Fail`, error propagates.
- `RetryWithReminder(String)` → `RetryWithFeedback(reminder)`. This is the
  `StagePolicy::default()` value, with a long reminder telling the model not
  to quote code verbatim and to re-emit its JSON.
- `FallbackToFreeForm { reminder }` → sets
  `self.recitation_fallback_active = true` and returns
  `RetryWithFeedback(reminder)`.

**`recitation_fallback_active` has exactly one effect:**
`response_format()` returns `None` early instead of
`Some(AiResponseFormat::Json { schema })`. It does not change validation, it
does not change the output format, and it is never reset for the rest of the
session.

Two consequences a reviewer should hold onto:

1. For a stage whose `OutputFormat` has no schema — `json()` or `Text` —
   `response_format()` was already `None`, so the flag changes nothing at all.
2. `report_stage` is the only user of `FallbackToFreeForm`, and it uses
   `OutputFormat::text_with_validator(validate_inline_format,
   format_inline_feedback)`. So the fallback drops nothing, and
   `validate_inline_format` still requires `>`-quoted lines, a `commit `
   header and an `Author:` header within the first 20 lines. The fallback
   reminder — "Do not quote code verbatim. Summarize your review directly."
   — pushes the model away from the `>` quoting that the validator then
   demands. This looks like a live conflict, not a deliberate design.

**Check in a diff:** a patch that sets `FallbackToFreeForm` on a stage,
expecting the structured-output constraint to relax. Verify the stage
actually has a schema (`json_with_schema`) and that its validator tolerates
free-form output. If the validator is strict, the fallback just burns
`max_provider_error_retries` (3) attempts and then fails.

**Check in a diff:** broadening the `"RECITATION" || "blocked"` match, or
routing a new error class through `handle_provider_error`. The substring test
is already loose.

## 11. Checklist for a diff that touches `src/workflow/`

- [ ] Does a stage write shared state outside its reducer (`Arc<Mutex>`,
      channel, atomic, global) in `reduce`, `with_var`, `skip_if`, an
      inclusion resolver, or a hand-written `ExecutableStage`?
- [ ] Did `execute_isolated`'s signature change in a way that lets parallel
      stages observe each other's writes?
- [ ] Is a `ParallelPolicy` change consistent with what downstream steps do
      when a stage silently produced nothing (`BestEffort`) or when the batch
      published nothing at all (`FailFast`)?
- [ ] Does a reducer assign or clear a field that another reducer also
      writes? Is there a direct `(stage.reducer)(&mut state, out)` test on a
      pre-populated state, in the shape of
      `test_verification_stage_preserves_preexisting_concerns`?
- [ ] Does each `early_exit_if` test the field written by the step
      immediately before it, after a reorder or rename?
- [ ] Does a newly skippable stage break an `early_exit_if`, a progress
      count, or a downstream prompt that assumed its reducer ran?
- [ ] Do new UI / metrics / DB hooks depend on `StageStarted` for a stage
      that can be skipped, instead of `ParallelResolved` or state?
- [ ] Does a new `LlmSession` impl override `call_tools`, convert tool errors
      to `{"error": ...}`, preserve result order, and carry the duplicate
      guard in **both** `call_tool` and `call_tools`?
- [ ] Do `max_turns` / `max_validation_attempts` changes account for `>=`
      (so N gives N-1 retries) and for the fact that retries decrement the
      turn counter, so total provider calls are unbounded by `max_turns`
      alone?
- [ ] Does every `@include("...")` in template text byte-match a path passed
      to `include_file`, and vice versa?
- [ ] Does any path reaching an inclusion API originate from model output or
      from the patch under review? If so, is it filtered to a plain file name
      as in `prescreen_stage`'s reducer?
- [ ] Does the change preserve: placement before substitution, single-pass
      substitution on `Segment::Template` only (never re-scanning inserted
      variable values), and `place` never scanning `Segment::Included`?
- [ ] Does a new output struct use `#[serde(default)]` on every field
      without a validator that rejects the empty parse?
- [ ] Was `with_validator` / `with_feedback_formatter` called on an
      `OutputFormat::Text` (silent no-op)?
- [ ] Are new messages pushed to both `history` and `log_history` in
      `SessionRunner::run`?
