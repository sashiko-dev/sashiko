# DESIGN: TraeCLI Provider Support

## Context

Sashiko supports command-line AI providers through the `AiProvider` trait.
The provider receives a complete `AiRequest`, converts it to Sashiko's text
protocol, invokes a local CLI, and converts the CLI response back to an
`AiResponse`.

TraeCLI exposes a non-interactive JSONL interface:

```text
traecli exec --json [OPTIONS] -
```

The prompt is supplied on standard input. Standard output contains JSONL
events such as `item.completed` and `turn.completed`.

## Goals

1. Add a provider named `traecli`.
2. Reuse Sashiko's existing prompt and synthetic tool-call protocol.
3. Prevent TraeCLI native tools from executing commands or modifying the
   review checkout.
4. Keep each request ephemeral and read-only.
5. Preserve token usage reported by TraeCLI.
6. Route server-side review workers through Sashiko's stdio provider.

## Non-goals

- Do not expose the review checkout as TraeCLI's working directory.
- Do not persist or resume TraeCLI sessions.
- Do not read project or user TraeCLI configuration.
- Do not add provider-specific settings beyond the model name.

## Provider Invocation

`TraeCliProvider` builds the prompt with
`claude_cli::build_prompt()` and invokes:

```text
traecli exec
  --json
  --skip-git-repo-check
  --ignore-user-config
  --ephemeral
  --disallowed-tool unified_exec
  --disallowed-tool exec_command
  --disallowed-tool apply_patch
  --disallowed-tool web_search
  --disallowed-tool update_plan
  --sandbox read-only
  -m <model>
  -
```

The isolation flags have separate roles:

- `--ignore-user-config` prevents local instructions, MCP configuration, and
  other user settings from changing provider behavior.
- `--ephemeral` prevents session persistence.
- `--disallowed-tool` blocks native shell execution, patch application,
  search, and plan mutation. These are the native capabilities that could
  inspect or change the review workspace instead of using Sashiko's protocol.
- `--sandbox read-only` is a defense-in-depth filesystem restriction.

TraeCLI runs with a newly created empty temporary directory as its working
root. The source checkout is available only through Sashiko's bounded
application-level tools and tool results. This remains fail-safe if TraeCLI
adds a new read-only native tool name that is not in the explicit deny-list:
the tool sees the empty workspace, not the review repository.

If the request includes Sashiko tools, the prompt explicitly states that
`tool_calls` is an application-level text protocol. TraeCLI must return that
JSON as ordinary response text rather than attempting a native tool call.

## JSONL Processing

The provider reads standard output line by line:

- `item.completed`: append `item.text` to the completion.
- `turn.completed`: read input, output, and cached token counts.
- Unknown or malformed lines: ignore them.

Multiple completed text items are joined with newlines. The combined text is
then parsed by `claude_cli::parse_inner_response()` so plain content and
Sashiko synthetic tool calls behave like the existing CLI providers.

If no completed text event is present, the provider falls back to the raw
output. This preserves compatibility with older or simplified CLI output.

## Worker Routing

The server owns the configured provider and launches the `review` child
process. `traecli` therefore maps to `stdio-claude` for the child, matching
the other CLI providers. Model requests travel over Sashiko's typed stdio
protocol and are executed by the provider in the parent process.

This avoids invoking TraeCLI directly from the review child and preserves
centralized concurrency and error handling.

## Error Handling

The provider returns explicit errors for:

- missing `traecli` executable;
- a request exceeding the ten-minute timeout;
- process wait failures;
- non-zero exit status.

Standard error is logged at debug level and included in a failed process
error. Environment variables and configuration files are not printed.

## Tests

Normal tests do not require an authenticated TraeCLI installation. Unit tests
cover:

1. the complete isolated command argument list;
2. JSONL text concatenation;
3. token usage extraction;
4. malformed and unrelated event handling.

The normal Sashiko test suite covers provider factory construction and stdio
review behavior.
