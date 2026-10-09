# Component Guide Index

Load guides from the prompt directory based on what the change touches. Each
guide holds the invariants, contracts and known bug patterns for one area of
Sashiko.

A change can match several rows. Load **every** matching guide, not only the
most specific one. A patch touching `src/worker/prompts.rs` matches both the
workflow engine and the LLM stage design rows, and both apply.

The triggers column lists paths, type names and function names. Matching any
trigger is enough.

## Component Guides

| Component | Triggers | File |
|-----------|----------|------|
| Workflow Engine | `src/workflow/`, `Stage`, `StageBuilder`, `ExecutableStage`, `StateMutation`, `WorkflowEngine`, `WorkflowEnv`, `PromptTemplate`, `OutputFormat`, `StagePolicy`, `ToolScope`, `ParallelPolicy`, `RecitationPolicy`, `early_exit_if`, `dynamic_parallel` | workflow-engine.md |
| LLM Stage Design | `src/workflows/`, any new or modified stage, stage instruction text, output JSON schemas, `concerns`, `dismissed_concerns`, `findings`, validators, feedback formatters, planner or pre-screen changes | llm-stages.md |
| AI Providers | `src/ai/`, `AiProvider`, `SessionRunner`, `LlmSession`, `AiMessage`, token budget, truncation, `max_input_tokens`, prompt caching, backoff, quota, `ClassifyAiError`, any provider under `src/ai/*.rs` | ai-providers.md |
| Toolbox | `src/toolbox/`, tool registration, tool declarations, `validate_path`, `git_grep`, `git_log`, `git_show`, `git_diff`, `git_blame`, `git_ls`, `git_read_files`, `git_find_files`, `read_prompt`, tool output truncation | toolbox.md |
| Database and Migrations | `src/db.rs`, `src/migrations/`, SQL, schema, `patchsets`, `patches`, `messages`, `threads`, `reviews`, `findings`, `bugs`, `ai_interactions`, outbox tables, upsert or merge logic, libsql | db-migrations.md |
| API and Authorization | `src/api.rs`, `src/auth.rs`, `src/access.rs`, axum routes and handlers, JWT, `LocalToken`, `[server.acl]`, capabilities, `read_only`, `enable_unsafe_all_submit` | api-auth.md |
| Forge and Webhooks | `src/forge.rs`, `src/fetcher.rs`, webhook handlers, HMAC or signature verification, `ForgeProvider`, `ForgeMetadata`, `is_safe_repo_url`, repo URLs, PR or MR handling, forge API tokens | forge.md |
| Git Operations | `src/git_ops.rs`, `GitWorktree`, worktrees, `worktree_dir`, clones, fetches, `git am`, repack, gc, commit-graph, any new `git` invocation | git-ops.md |
| Email and Delivery | `src/email_policy.rs`, `src/email_router.rs`, `src/worker/email.rs`, `src/patchwork.rs`, `src/worker/patchwork.rs`, `email_policy.toml`, outbox rows, recipients, `dry_run`, embargo, loop prevention, SMTP | email-policy.md |
| Settings | `src/settings.rs`, `Settings.toml`, `docs/examples/Settings.example.toml`, `deny_unknown_fields`, any added, renamed or removed configuration key, `SASHIKO__` environment overrides | settings.md |

## Cross-Cutting Patterns

| Pattern | Triggers | File |
|---------|----------|------|
| Async and Tokio | `async fn`, `.await`, `tokio::spawn`, `spawn_blocking`, `select!`, timeouts, child processes, `src/worker/` | rust-async.md |
| Error Handling | `Result`, `?`, `anyhow`, `unwrap`, `expect`, `panic!`, indexing, slicing, `as` casts, swallowed errors | error-handling.md |
| Concurrency | `Arc`, `Mutex`, `RwLock`, `Semaphore`, `OnceLock`, statics, shared state, races between the daemon and its workers | concurrency.md |
| Resource Limits | Unbounded collections or channels, token budget, `concurrency`, timeouts, temporary directories, child processes, row growth, `src/worker/compressor.rs`, `src/worker/repack.rs` | resource-limits.md |
