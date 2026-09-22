# Design: Stateless AI Agent Bug Access and Scoped JWT Tokens

## Status

Proposed.

## Context and Motivation

Sashiko tracks verified pre-existing bugs across both `sashiko.dev` (Linux
kernel) and `sashiko.sashiko.dev` (Sashiko self-review). Following
`DESIGN_BUG_ACCESS_CONTROL.md`, authentication and authorization in Sashiko are
**100% stateless**:

- Both email magic links (`typ: "sign_in_link"`) and browser sessions
  (`typ: "session"`) in [`src/auth.rs`](../src/auth.rs) are HS256 JWTs signed
  with `SASHIKO__SERVER__JWT_SECRET` and verified in memory without database
  queries.
- Authority is resolved in memory by [`Principal::resolve`](../src/access.rs)
  from `[server.acl]` and `MAINTAINERS`.

While email magic links work well for human maintainers in a web browser, they
create friction for AI coding and triage agents (such as local developer agents
investigating bugs in a subsystem or automated bug-reproduction / patch-fix
agents):

1. An AI agent cannot click an email magic link without access to the user's
   inbox or direct cluster access to `SASHIKO__SERVER__JWT_SECRET`.
2. Copying a browser's 24-hour session JWT into an agent environment grants the
   agent the user's full mutation privileges (`Manage`) rather than allowing
   read-only bug inspection.
3. There is currently no first-class CLI surface in `sashiko-cli` for listing,
   inspecting, or triaging bugs from a terminal or agent tool invocation.

## Goals

1. **100% Stateless Architecture (matching email auth):**
   - Zero database tables, zero migrations, and zero database lookups during
     authentication.
   - Agent tokens are HS256 JWTs signed with `SASHIKO__SERVER__JWT_SECRET`
     carrying `typ: "api_token"`, `sub: <email>`, `iat`, `exp`, and an optional
     `max_bug_access` claim (`"read"` | `"comment"` | `"manage"`).
2. **Preserve all `DESIGN_BUG_ACCESS_CONTROL.md` invariants:**
   - Every bug route continues to resolve a `Principal` from a verified email
     identity (`[server.acl]` + `MAINTAINERS`).
   - Effective access is attenuated to
     `min(principal.access_to(sections), token.max_bug_access)` so an agent
     token can never amplify a user's privileges and defaults to read-only
     (`"read"`).
   - Unauthorized bug reads continue to return `404 Not Found`.
   - Blocklisted addresses (`[server.acl].blocklist`) are always refused.
3. **First-class `sashiko-cli` bug commands for AI agents:**
   - Add `sashiko-cli bugs list`, `sashiko-cli bugs show <id>`, and
     `sashiko-cli bugs action <id>` authenticated via `SASHIKO_API_TOKEN` or
     `--token`.

## Non-Goals

- No database token tables or server-side token state.
- No separate user/role provisioning table: authority is derived strictly from
  `Principal::resolve(email, acl, maintainers)`, attenuated by the JWT's signed
  `max_bug_access` claim.
- No anonymous read access on any bug endpoint.

---

## Architecture

### 1. Stateless JWT Claims

Extend [`Claims`](../src/auth.rs) with an optional `max_bug_access` field:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    #[serde(default)]
    pub iat: Option<usize>,
    #[serde(default)]
    pub sid: Option<String>,
    #[serde(default)]
    pub typ: Option<String>,
    /// Optional ceiling on BugAccess ("none", "read", "comment", or "manage").
    #[serde(default)]
    pub max_bug_access: Option<String>,
}
```

- Browser sessions continue to use `typ: Some("session")` with
  `max_bug_access: None` (unattenuated).
- Agent tokens use `typ: Some("api_token")` with a bounded lifetime (e.g. up to
  90 days) and an explicit `max_bug_access` ceiling (`"read"` by default).
- Note: `POST /api/auth/refresh` only refreshes browser sessions
  (`typ: Some("session")`); agent tokens have a fixed `exp` set at creation
  time and cannot be refreshed indefinitely.

### 2. Principal Resolution and Privilege Attenuation

When an incoming request carries `Authorization: Bearer <jwt>`:

1. Verify the HS256 signature and expiration against
   `SASHIKO__SERVER__JWT_SECRET`.
2. Accept `typ == Some("session") | Some("api_token") | None` on bug routes
   (rejecting `sign_in_link` tokens). On global patch mutation routes guarded by
   `is_authorized` (`/api/submit`, `/api/patchset/rerun`,
   `/api/patchset/cancel`, `/api/patch/rerun`), reject `typ == Some("api_token")`
   so scoped bug tokens never inherit global patch ingestion, rerun, or
   cancellation capabilities.
3. Verify both `!state.settings.server.acl.is_blocklisted(&claims.sub)` and
   `!claims.sid.as_deref().is_some_and(|sid| state.settings.server.acl.is_blocklisted(sid))`.
4. Resolve the base principal via
   `Principal::resolve(&claims.sub, &state.settings.server.acl, maintainers)`
   and apply the optional `max_bug_access` ceiling from `claims.max_bug_access`:
   - For any specific bug, `principal.access_to(sections)` returns
     `min(base_principal.access_to(sections), max_bug_access)`.
   - For bug creation (`POST /api/bug/analyze`), `may_create()` is allowed only
     when `base_principal.may_create()` is true **and** `max_bug_access` permits
     `Manage`.
   - Raw transcript access (`/api/bug/logs`, `/api/bug/raw`, `/api/bug/input`,
     and patchset transcripts via `TranscriptPrincipal::access_to_patchset`)
     requires a token ceiling that permits reading
     (`max_access.is_none_or(|m| m.can_read())`). When `max_bug_access` is
     `"none"`, `has_global_bug_visibility()`, `visibility()`, and
     `TranscriptPrincipal::access_to_patchset` all deny access (`Sections(&[])`
     / `TranscriptAccess::Denied`).
   - Individual or emergency revocation of compromised stateless tokens is
     evaluated in memory on every request with zero database queries:
     - **Per-token revocation:** Every minted API token carries a unique
       128-bit hex token identifier in its `sid` claim (returned as `token_id`
       by `POST /api/auth/token`). Adding that `token_id` to
       `[server.acl].blocklist` (or `SASHIKO__SERVER__ACL__BLOCKLIST`)
       immediately revokes only that specific token across `Principal`,
       `is_authorized`, and `refresh_token` while leaving the maintainer's
       interactive sessions and other tokens unaffected.
     - **Per-identity or global revocation:** Adding an email address to
       `[server.acl].blocklist` revokes all tokens for that address, and
       rotating `SASHIKO__SERVER__JWT_SECRET` revokes all issued tokens
       globally.

### 3. Stateless Token Minting Endpoint (`POST /api/auth/token`)

Only an interactive browser session (`typ == Some("session")`) or the local
operator token (`LocalToken` via `OptionalPrincipal`) may mint a stateless agent
JWT via `POST /api/auth/token`. Requests authenticated with `typ == Some("api_token")`
are rejected with `403 Forbidden` so an attenuated agent token can never mint a
higher-privilege token or extend its own lifetime:

- `POST /api/auth/token`
  - Request body:
    ```json
    {
      "max_bug_access": "read",
      "expires_in_days": 30,
      "email": null
    }
    ```
  - `max_bug_access`: `"none"`, `"read"` (default), `"comment"`, or `"manage"`.
  - `expires_in_days`: clamped to `1..=90` days (default `30`).
  - `email`: optional target email; only an operator (`principal.is_operator()`
    or local operator token) may mint a token for a different email address
    (e.g. a dedicated bot identity configured in `[server.acl]`).
  - Response (`200 OK`):
    ```json
    {
      "token": "<signed-jwt>",
      "token_id": "<32-char-hex-sid>",
      "email": "user@example.com",
      "max_bug_access": "read",
      "expires_in_days": 30
    }
    ```

### 4. `sashiko-cli` Bug and Token Subcommands

Extend `src/bin/sashiko-cli.rs` with:

- Authentication resolution order:
  1. `--token <jwt>` CLI flag
  2. `SASHIKO_API_TOKEN` environment variable (isolated from daemon settings
     because `config-rs` uses double-underscore `separator("__")` between the
     prefix and first key)
- Commands:
  - `sashiko-cli bugs list [--server <url>] [--status <status>] [--subsystem <name>] [--q <search>] [--page <n>]`
  - `sashiko-cli bugs show <bug-id> [--server <url>]`
  - `sashiko-cli bugs action <bug-id> --action <action> [--comment <text>] [--assignee <email>] [--duplicate-of <id>]`
  - `sashiko-cli token create [--server <url>] [--max-access <none|read|comment|manage>] [--expires-days <days>]`

---

## Step-by-Step Implementation Plan

1. **Step 1 (Design doc):** Commit `designs/DESIGN_AGENT_BUG_ACCESS.md`.
2. **Step 2 (Stateless JWT & Principal attenuation):** Extend `Claims` and
   `AuthUser` in `src/auth.rs` and `Principal` in `src/access.rs` to support
   `typ: "api_token"` and `max_bug_access` attenuation, with unit tests
   verifying subsystem scoping, read-only attenuation, refresh rejection for
   `api_token`, and blocklist enforcement.
3. **Step 3 (Stateless minting endpoint):** Add `POST /api/auth/token` in
   `src/api.rs` with unit tests.
4. **Step 4 (CLI subcommands):** Add `sashiko-cli bugs` (`list`, `show`,
   `action`) and `sashiko-cli token create` subcommands in
   `src/bin/sashiko-cli.rs`.
