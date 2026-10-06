# MCP Client Tools Design

## 1. Overview

Some facts a reviewer needs are not in the tree under review: hardware
specifications, errata, vendor programming guides. Many of these are
licensed documents that cannot be vendored into `third_party/prompts/`,
but an operator who holds a copy can serve it through a
[Model Context Protocol](https://modelcontextprotocol.io) (MCP) server.

This document describes how Sashiko's review stages call tools exposed by
operator-configured remote MCP servers, next to the built-in `git_*`
tools. For example, a server that looks up register and capability
definitions in a hardware specification lets the `hardware` stage check
a driver against the documented behavior instead of guessing.

## 2. Requirements

- **Off by default.** With no `[mcp]` section, nothing changes: no
  network traffic, no extra tools, identical requests and cache keys.
- **One tool loop.** Every provider, including `claude-cli`, `copilot-cli`,
  `kiro-cli` and ACP, is a text backend driven by Sashiko's own
  `ToolBox`. MCP tools are added there, so every provider gets them and
  the CLI providers keep their tools, MCP servers and hooks disabled.
- **Explicit exposure.** The operator names each server, the tools that
  may be called on it, and the stages that see them.
- **Non-fatal.** An unreachable server, a failing call or a malformed
  response never fails a review: discovery failures drop that server's
  tools with a warning, and call failures become `{"error": ...}` tool
  results, like every other tool.

## 3. Configuration

```toml
[[mcp.servers]]
# Lowercase letters, digits and '_'. Tools appear to the model as
# mcp_<name>_<tool>.
name = "docs"
url = "https://mcp.example.com/mcp"
# The bearer token is read from this environment variable, never from
# the settings file.
bearer_token_env = "DOCS_MCP_TOKEN"
# Required and not empty: only these server tools are exposed.
allowed_tools = ["search", "read"]
# Stages that see this server's tools (default: ["hardware"]).
stages = ["hardware", "verification", "post-verification"]
# One line added to the system prompt of those stages.
prompt_hint = "Look up register definitions in the hardware documentation."
timeout_secs = 30          # per request (default 30)
max_output_bytes = 32768   # per tool result (default 32768)
```

Settings are validated at load time: unique valid names, `http(s)` URLs
(plain `http` only for loopback hosts), and a non-empty `allowed_tools`.

## 4. Protocol

A small client in `src/toolbox/mcp.rs` speaks the Streamable HTTP
transport over the existing `reqwest` dependency. It needs only:

1. `initialize`, then the `notifications/initialized` notification,
   keeping the `Mcp-Session-Id` header if the server issues one, and
   sending `MCP-Protocol-Version` on later requests.
2. `tools/list`, following `nextCursor` up to a fixed page limit.
3. `tools/call`.

Each POST accepts `application/json` and `text/event-stream`. An SSE
response is read until the event carrying the JSON-RPC response with the
request's id. Redirects are not followed, so the token is only ever sent
to the configured origin. Responses larger than a fixed cap are
rejected.

A hand-written client keeps the dependency tree unchanged and builds
under every feature profile (`--no-default-features` included). Stdio
servers, resources, prompts, sampling and server-initiated requests are
out of scope.

## 5. Lifecycle and Wiring

- **Discovery** happens once per review worker process, in
  `run_worker_in_worktree`, before any patch is reviewed. Servers are
  contacted concurrently; one that fails is logged and skipped. The
  daemon runs reviews through the same worker, so both paths are
  covered.
- **Registration.** Each allowed tool becomes an `McpTool`, which
  implements `LlmTool<SashikoToolContext>` and is registered into every
  patch's `ToolBox` through `ToolBox::add_mcp_tools`. `LlmTool::name` and
  `description` return strings borrowed from the tool rather than
  `&'static str`, so tools discovered at run time need no leaked
  strings.
- **Stage exposure.** The `ToolBox` records which stages may see each
  MCP tool. `StageSession::tools()` drops MCP tools the current stage may
  not see, and `call_tool` refuses calls to them, so a model that guesses
  a name gets an error instead of a call.
- **Prompt hint.** For a stage that sees MCP tools, the system prompt
  gains a short section listing each server's tools and its
  `prompt_hint`. The hint comes from the settings file only.
- **Bug workflows** (`linux_bug`, `bug_worker`) are not wired in this
  first version. Their stages have different names, and their
  toolboxes are built in the daemon process.

## 6. Tool Results

A successful `tools/call` returns:

```json
{
  "source": "mcp:docs",
  "content": "...text items joined by blank lines...",
  "truncated": false
}
```

Non-text content items are replaced with a one-line placeholder.
`isError: true` becomes `{"error": ...}`. Content is truncated to
`max_output_bytes` on a UTF-8 boundary with `"truncated": true` and a
hint to narrow the query. Results go through the per-review `ToolBox`
cache like any other tool.

## 7. Threat Model

Patch text is untrusted (`prompts/sashiko/prompt-injection.md`), so the
arguments of every tool call are attacker-steerable.

- **Fixed endpoints.** The model chooses only the tool name and its
  arguments. URLs, headers and tokens come from the settings and the
  environment.
- **Allowlist.** Tools not in `allowed_tools` are never declared or
  callable, even if the server lists them later.
- **Secrets.** The token is read from the environment at discovery time,
  held in memory only, never logged, and never part of a cache key.
  Error messages go through `redact_secret`, and no request headers are
  logged.
- **Outbound data.** Tool arguments can carry patch text to the server.
  An operator must point Sashiko only at servers they trust with the
  code under review. This is documented next to the setting.
- **Untrusted results.** MCP output is data, like `git show` output. It
  is never used as a path, an `@include`, a prompt name or a lookup key,
  and is labeled with its source so a reader can tell where a quote came
  from.
- **Server metadata.** Tool names from the server must match
  `[A-Za-z0-9_-]{1,64}`. Descriptions are capped at 2 KiB. A parameter
  schema that is not a JSON object is replaced with an empty object
  schema.
- **Resource limits.** Per-request timeout, response size cap and
  `tools/list` page limit.

## 8. Caching

The tool declarations are part of every `AiRequest`, so enabling MCP
changes response cache keys only for stages that see MCP tools.
Declarations are sorted by name, as before, so keys stay stable between
runs.

## 9. Testing

`src/toolbox/mcp.rs` carries unit tests against an in-process mock MCP
server (JSON and SSE responses, session ids, pagination, allowlist
filtering, `isError`, truncation, timeouts, and token redaction), plus
`ToolBox` and `StageSession` tests for stage exposure.
