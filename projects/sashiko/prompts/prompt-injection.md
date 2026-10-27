# Untrusted Content and Prompt Injection

Sashiko reviews code it did not write, submitted by people it does not trust,
and feeds that content to a model whose output then drives file reads and tool
calls. This file states where the boundary is and what a change must not do to
it.

## What is untrusted

Everything in this list is attacker-controlled. Anyone who can post to a public
mailing list or open a pull request can put arbitrary text in it.

- Patch diffs and their file paths
- Commit messages and subjects
- Email headers: `From`, `To`, `Cc`, `Subject`, `Message-Id`, `References`
- Pull request and merge request titles, bodies and comments
- Webhook payload fields, including repository URLs, branch names and SHAs
- Branch, tag and ref names in a fetched repository
- File contents in the worktree under review

**The model's own output is also untrusted.** A stage's response can be steered
by the patch it was shown. Treat a model-produced string with the same
suspicion as the patch that produced it.

## The two rules

**1. Untrusted content may be shown to the model. It may never become an
instruction, a path, or a command.**

Showing a diff to a model is the whole point. The danger is not that the model
reads attacker text; it is that attacker text reaches somewhere it is
*interpreted* — a file path, a shell argument, a git ref, a template directive.

**2. Untrusted content may not choose what gets included in a prompt.**

`PromptTemplate` in `src/workflow/prompt.rs` enforces this structurally. It
splits a rendered prompt into Template segments and Included segments, and runs
variable substitution only on Template segments. A diff containing the literal
text `@include("/etc/passwd")` is therefore inert: it arrives inside a variable
substitution, and substituted content is never rescanned for directives.

This is a security property, not a formatting detail. A change that makes
included content eligible for substitution, or that resolves directives after
substitution rather than before, removes it.

## Where the boundary is enforced today

Know these before changing anything near them.

- **Prompt directive placement** — `PromptTemplate::render_for_model` resolves
  `@include(...)` and the `@includes` marker from the *template*, never from
  substituted values. See `src/workflow/prompt.rs` and its tests.

- **Pre-screen and `read_prompt` guide names** — the pre-screen stage and
  per-stage `read_prompt` tool calls let the model choose which guide files are
  inlined into downstream prompts (`sanitize_guide_path` for top-level
  pre-screen guides and `sanitize_prompt_relpath` for top-level, `subsystem/`,
  or `patterns/` prompt guides in `src/workflows/guard.rs`). Both sanitizers
  reject any path containing `..`, `\`, leading `/`, or subdirectories outside
  `subsystem/` and `patterns/` before it becomes a path. These are the places
  model output legitimately becomes a prompt filename, and the sanitizers in
  `src/workflows/guard.rs` are what make that safe.

- **Stage names** — the planner returns stage names, which are resolved against
  the stage table rather than used directly. An unrecognised name is dropped
  with a warning, never constructed into anything.

- **Tool paths** — `validate_path` in `src/toolbox/utils.rs` confines a tool
  argument to its root, whether that root is the worktree or the prompt
  directory. Every tool taking a path goes through it.

- **Git invocations** — `src/git_ops.rs` passes arguments as an argv vector, not
  a shell string, and applies `GIT_PROTOCOL_RESTRICTIONS`. A ref or path from a
  patch must not be concatenated into a command, and must not be able to be
  read as an option.

- **Repository URLs** — `is_safe_repo_url` in `src/forge.rs` blocks the obvious
  SSRF targets by host. It is best-effort and host-based; it does not survive
  DNS rebinding, and the real gate is webhook signature verification.

- **Secret redaction** — `redact_secret` in `src/fetcher.rs` keeps tokens
  embedded in git URLs out of logs.

## What to check in a diff

Report a finding if a change does any of the following without a compensating
control:

- Takes a string that originated in a patch, a commit message, a PR field, a
  webhook payload, or a model response, and uses it as a **file path** without
  `validate_path`, or as part of one.
- Uses such a string as a **git ref, remote name, branch, or command argument**
  without ensuring it cannot be interpreted as an option, or without a `--`
  separator where one is needed.
- Adds a **new prompt variable** whose value is untrusted, and places it
  somewhere the template later interprets.
- Moves include resolution, or any directive parsing, to **after** variable
  substitution.
- Adds **PR or email prose** — a title, body, or comment — into a prompt. Today
  only the diff and the commit message enter the model context for forge
  reviews. Widening that is a deliberate decision that needs its own fencing,
  not an incidental one.
- Loosens the **prompt path sanitizers** (`sanitize_guide_path` /
  `sanitize_prompt_relpath`), or adds another place where model output becomes a
  filename, an identifier, or a lookup key without going through those prompt
  sanitizers or a table of known values.
- Logs, stores, or puts into a prompt a value that could contain a **secret**:
  `forge.api_token`, `webhook_secret`, the local operator token, the JWT signing
  key, SMTP credentials, or a URL with credentials embedded.
- Lets untrusted content reach a **tool that escapes the worktree**, or adds a
  tool with a wider root than the worktree.

## What is not a finding

- The model reading attacker-controlled text. That is the job.
- A patch containing text that looks like an instruction, absent a path by
  which it is actually interpreted. Say which path, or do not report it.
- Theoretical injection into a string that is only ever logged or displayed.
