# toolbox

`toolbox` provides the LLM tool execution framework (`LlmTool`, `ToolRegistry`, `ToolBox`) and the read-only Git and prompt inspection tools (`git_blame`, `git_diff`, `git_find_files`, `git_grep`, `git_log`, `git_ls`, `git_read_files`, `git_show`, `read_prompt`) used during automated patch review. It manages shared review context such as virtual `HEAD` resolution, active patch file proximity ranking, argument normalization, in-memory result caching, path confinement validation, and bounded, timed-out `git` subprocess execution.

## Architecture & Data Flow

- `ToolBox::new` must share the same `Arc<RwLock<HashMap<String, Value>>>` between `ToolBox::cache` and `SashikoToolContext::cache`. `ToolBox::call` stores final tool responses under `"{name}:{normalized_args_json}"` while `GitShowTool::call` stores raw git stdout `String`s under `"git_show_raw:{object}:{suppress_diff}:{paths:?}"`. Broken by: initializing separate `Arc` allocations, inserting unprefixed keys into `context.cache` that collide with `"{tool_name}:{json}"`, or storing non-`String` values under `"git_show_raw:*"`. [F1, F20]
- Every git subprocess in `toolbox` must run through `capped_output` or `for_each_line` in `src/toolbox/command.rs` rather than `tokio::process::Command::output` or `cmd.spawn()`, enforcing `kill_on_drop(true)`, `TOOL_COMMAND_TIMEOUT` (120s), and bounded stdout/stderr capture. [F46]
- When `SashikoToolContext::virtual_head` is `Some(sha)`, `SashikoToolContext::virtualize_ref` replaces only `"HEAD"` tokens preceded by start-of-string or a non-`'/'` character and followed by end-of-string or one of `[~^:.@]`, preserving remote refs like `"origin/HEAD"` and hyphenated names like `"HEAD-fixes"`. Broken by: plain substring replacement or regexes matching after `'/'` or before `'-'`. [F5]
- When `Truncator::truncate_sequential` truncates output in `GitBlameTool`, `GitReadFilesTool`, or ranged `GitShowTool` calls, `metadata.returned_items` and `end_index` must be computed from `lines_kept` rather than `truncated.lines().count()`, which includes the appended truncation warning line. [F10]
- In `GitReadFilesTool::read_single_file` and `GitShowTool::call`, 0-based `start` must be clamped via `start.min(total_lines)` before `end.clamp(start, total_lines)` and checked with `if start >= total_lines` before slicing `&lines[start..end]`, so out-of-range `start_line` values cannot panic `clamp` or slice indexing. [F56]
- Every entry returned by `ToolRegistry::declarations` must be a JSON object with string `"name"` and `"description"` fields, because `ToolBox::get_declarations_generic` calls `decl["name"].as_str().unwrap()` and `decl["description"].as_str().unwrap()`. [F63]

## Synchronization

- `ToolBox::call`, `GitShowTool::call`, and `GitGrepTool::call` must drop any `std::sync::RwLock` guard on `SashikoToolContext::cache`, `active_patch_files`, or `virtual_head` before every `.await` point, rather than holding a lock guard across `self.registry.call(...).await` or `capped_output(&mut cmd).await`. [F57]

## Object Lifetimes

- In `capped_output_with_limit` and `for_each_line_within`, the child's `stderr` pipe must be spawned into `drain_capped` before awaiting `stdout` reads, and `drain_capped` must read and discard `stderr` until EOF even after `MAX_CAPTURED_STDERR` bytes are kept so a child flooding `stderr` never blocks on a full pipe. [F17]
- When `capped_output_with_limit` stops reading `stdout` at `max_stdout + 1` bytes (`stdout_capped` is true), it must call `child.start_kill()` before awaiting `child.wait()` and `stderr_drain`, because `out_reader` stops consuming `stdout` and the child would otherwise block on its `stdout` pipe and never exit. [F18]
- `capped_output_within`, `for_each_line_within`, and any caller wrapping `capped_output_with_limit` in a timeout or cancellable future must set `cmd.kill_on_drop(true)` before spawning, because `capped_output_with_limit` does not set it and dropping the future on timeout drops `Child` without awaiting `child.wait()`. [F19]

## API Contracts

- `ToolBox::call` must run `normalize_json_numbers` on `args` (so `.as_u64()` works on float-encoded integers like `3.0`) before calling `normalize_tool_args` (`LlmTool::normalize_args`) and `registry.call`, and caches `Ok` results (except `"todowrite"`) under `"{name_normalized}:{serde_json::to_string(&normalized_args)}"`. Because `ToolBox::call` uses `normalize_args` only for the cache key and passes the un-normalized (number-coerced) `args` to `LlmTool::call`, every tool's `normalize_args` defaults must match the fallback defaults inside its `call` method. [F3, F72]
- Every git tool in `src/toolbox/` rejects model-supplied revisions, ranges, objects, and paths starting with `'-'` (after `virtualize_ref`), validates paths via `validate_path`, and passes `"--"` before path arguments or constructs validated `rev:path` specs to prevent git option injection. [F67]
- `validate_path` must allow non-existent trailing path components while walking up to the nearest existing ancestor to verify `canonical_base` containment and reject dangling symlinks, so git tools can validate file paths that only exist in historical revisions. Broken by: replacing the ancestor walk with direct `full_path.canonicalize()`. [F8]
- `ToolBox::new` must register `ReadPromptTool` only when `prompts_path.is_some()`. When `Some`, `SashikoToolContext::prompts_path` must be an existing directory path rather than a file path (`validate_path` rejects `!canonical_base.is_dir()`), and `ReadPromptTool` must resolve prompt names via `resolve_prompt_path`, which strips outer bundle prefixes via `normalize_prompt_relpath` and falls back to `subsystem/<normalized>` and `patterns/<normalized>` when a slashless name is not directly at `prompts_path`. [F6, F33, F45]
- `for_each_line` streams stdout lines exclusively to its `on_line` callback (capping each line at `MAX_LINE_BYTES` and stripping trailing `\r`) and always returns a `CappedOutput` with an empty `stdout` buffer and `stdout_capped: false`. Tools that filter or aggregate full tree listings in Rust (`GitFindFilesTool` and `GitLsTool`) must use `for_each_line` rather than `capped_output` and cap retained items at 1,000 in the callback so matches past `MAX_CAPTURED_STDOUT` are not dropped while memory stays bounded. [F41, F47]
- `GitShowTool::call` caches raw `git show` stdout in `SashikoToolContext::cache` under `git_show_raw:{object}:{suppress_diff}:{paths:?}` before slicing lines, and when `start_line` is provided without `end_line`, defaults `end_line` to `start_line + 100` before applying `Truncator` (20,000 tokens for blobs, 10,000 for commits). [F51]
- `GitReadFilesTool::call` rejects requests with `files.len() > 10` before calling `read_single_file`, and truncates each file's selected line slice with `Truncator::truncate_sequential(&result, 20_000)`. [F50]
- `GitLogTool::call` clamps `limit` to `args["limit"].as_u64().unwrap_or(10).min(100)` and passes `-n <limit>` to `git log` before applying `Truncator::truncate_sequential(&raw_stdout, 10_000)`. [F52]
- `format_git_grep_output` in `src/toolbox/utils.rs` sorts file match blocks by `get_priority_score(path, active_files)` before formatting and caps the header file list at `MAX_SUMMARY_FILES` (10), so `GitGrepTool`'s `Truncator::truncate_sequential(&formatted, 10_000)` retains matches closest to `active_patch_files` first. [F53]
- `glob_to_regex` escapes `. + ( ) | ^ $ [ ] { } \`, translates only `*` and `?` into wildcards, and anchors the pattern with `^` and `$` so `GitFindFilesTool` matches complete file paths without interpreting model-supplied globs as arbitrary regexes. [F70]

## Error Handling

- When `capped_output_with_limit` stops a child process because stdout exceeded `max_stdout`, `CappedOutput::status` reflects the kill signal while `stdout_capped` is true and `stdout` is truncated to the last newline, so callers that accept capped output must check `CappedOutput::is_usable()` instead of `status.success()`. [F7]
- When `!output.is_usable()` in `GitGrepTool::call`, an empty trimmed `stderr` must be treated as a normal zero-match result (`Ok` with `"content": ""`, `"matches": []`, and `"message": "No matches found."`), whereas a non-empty `stderr` surfaces as `Ok(json!({ "error": ... }))`. Broken by: returning an error whenever `!output.is_usable()` without checking `stderr.is_empty()`. [F24]
- `ToolBox::call` only inserts into `self.cache` when `self.registry.call(...)` returns `Ok(res)`, and `GitShowTool::call` only inserts `git_show_raw:...` into `context.cache` after `capped_output` succeeds and `output.is_usable()` is true, so failures never populate either cache. [F26]

## External Contracts

- `ToolRegistry::declarations` must sort tools by `LlmTool::name` before returning them because `HashMap` iteration order is randomized per registry instance and the declaration list is embedded into every LLM request and prompt-prefix cache key. [F4]

## Known Pitfalls

- `ToolBox::call` keys `self.cache` solely by tool name and pre-virtualized `normalized_args` without incorporating `virtual_head` or `active_patch_files` and without invalidating the cache when `set_virtual_head` or `set_active_patch_files` is called, so those setters must be called before any cached tool invocations on that `ToolBox`. [F2]
- In `GitReadFilesTool::read_single_file`, 1-based `start_line` and `end_line` must be pre-clamped against `total_lines.max(1)` before computing 0-based slice bounds clamped to `total_lines`, because `total_lines` is 0 for an empty file and `usize::clamp(1, 0)` panics. [F9]
- `GitGrepTool::call` passes `"-e"` before `pattern` so patterns starting with `'-'` are not parsed as flags, and splits `path` on whitespace so every non-`"."` pathspec token is checked for a leading `'-'` and validated with `validate_path` before being passed after `"--"`. [F68]
