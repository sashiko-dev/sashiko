# API and Authorization

Covers `src/api.rs`, `src/auth.rs`, `src/access.rs` and the ACL parts of
`src/settings.rs`.

This area decides who may do what over HTTP. It guarantees three things, and
nothing else:

1. A caller gets a capability only by presenting a credential. Never by where
   the request came from.
2. Every mutating endpoint names the capability that gates it, in its own body.
   There is no middleware that gates routes; `build_router` attaches only
   `redirect_www` and `DefaultBodyLimit`.
3. Authority over a Linux kernel bug is resolved per bug, from
   `[server.acl]` plus MAINTAINERS, on a code path that the local operator
   token cannot reach.

**The defect this guide exists to catch: a new mutating handler that omits the
capability check.** Nothing in the type system forces `submit_patch`-shaped
handlers to call `is_authorized`. If a diff adds a route and the handler body
does not contain both a `state.read_only` check and either an `is_authorized`
call or a `Principal`/`may_create`/`bug_access` check, say so and name the
capability it should have used.

## 1. The two credential types

Both arrive in `Authorization: Bearer …`, and they cannot be confused:
`is_token_shaped` in `src/auth.rs` accepts exactly 64 ASCII hex characters, and
a JWT never has that shape (`test_local_token_does_not_match_a_session_jwt`).

**Session JWT** — `crate::auth::AuthUser`, an axum `FromRequestParts` extractor
over `Arc<AppState>`.
- HS256 is pinned in `verify_token` via `Validation::new(Algorithm::HS256)`;
  `test_verify_token_rejects_unpinned_algorithm` covers algorithm confusion.
- The extractor rejects any token whose `typ` is not `"session"`. The
  `sign_in_link` token minted by `request_link` is therefore not a bearer
  credential for API routes — only `verify_link` accepts it.
- Secret resolution is `settings.server.jwt_secret` then the `JWT_SECRET`
  environment variable (`resolve_jwt_secret`, and the same fallback inline in
  the `AuthUser` extractor). A missing secret is 500, not 200.
- `OptionalAuthUser` swallows every rejection into `None`. A handler taking
  `OptionalAuthUser` is unauthenticated until it calls `is_authorized`.

**Local operator token** — `crate::auth::LocalToken`.
- Generated from `/dev/urandom` (`LocalToken::generate`; `fastrand` is
  deliberately not used), written `0600` with `create_new(true)` after
  unlinking any previous file (`LocalToken::write_to`), published by
  `publish_local_token` in `src/main.rs` at
  `Settings::local_token_path()` — `.sashiko-local-token` in the directory of
  `database.url`, or `.` for a remote database URL — and removed at shutdown.
- Comparison is over SHA-256 digests with `subtle::ConstantTimeEq`
  (`LocalToken::matches`), so neither content nor length steers timing.

**Why a file read is an authority claim and a source address is not.** The
deployed topology is a reverse proxy in front of a loopback bind, so a request
from the public internet reaches the server from `127.0.0.1` exactly as a local
one does. Reading a `0600` file in the server's state directory is a claim a
remote caller cannot make. `is_authorized` therefore takes no address
parameter, and `submit_patch` takes `ConnectInfo(addr)` only to log a refusal —
the comment there says so explicitly. `extract_client_ip` trusts
`x-forwarded-for`/`x-real-ip`/`forwarded` unconditionally, which is acceptable
**only** because its output feeds the sign-in rate limiter and the email body,
never an authorization decision. A diff that feeds `extract_client_ip` or
`ConnectInfo` into any grant is wrong.

**The token is an authority, not an identity.** It carries no email, so it can
never match an ACL entry, stand in for a maintainer, or reach a bug route.
`Permission::granted_by_local_token` in `src/settings.rs` is an exhaustive
match — a capability added later is unreachable by the token until someone
writes it down. `Principal::from_request_parts` never consults it.
`test_local_token_authorizes_ingest_but_grants_no_identity` in `src/api.rs`
pins both halves.

## 2. The capability model

`AclSettings` in `src/settings.rs` holds seven lists: `admins`, `security`,
`bug_reporters`, `ingest`, `cancel`, `review`, `blocklist`. All are
`#[serde(default)]` and `Vec<String>`, so **omitting the section yields empty
lists and grants nothing** (`test_acl_default_fails_closed`). Each accepts a
comma-separated string as well as an array (`deserialize_string_or_vec`).
Matching is `list_contains`: trimmed, `eq_ignore_ascii_case` on both sides.

`Permission` has exactly three variants: `Ingest`, `Cancel`, `Review`. There is
no bug permission; `AclSettings::has_permission` matches them exhaustively,
with `admins` granting all three.

Non-`Permission` capabilities, resolved in `Principal::resolve`:
- `admins` → `operator`: `BugAccess::Manage` on every bug, plus `may_create`.
- `security` → `BugAccess::Comment` on every bug and
  `has_global_bug_visibility()`, and *no* `Permission` at all.
- `bug_reporters` → `may_create` only. Ships empty, so `/api/bug/analyze` is
  operator-only out of the box.
- MAINTAINERS: `subsystems_for_address` → `Manage` within those sections;
  `is_global_maintainer` (a section claiming the whole tree, i.e. THE REST) →
  `Manage` everywhere plus global visibility.

**Blocklist overrides everything.** `AclSettings::has_permission` checks
`is_blocklisted` before `is_admin`; `is_authorized` checks it before
`testing_mode` and before `allow_all_submit`; `Principal::resolve` returns
`Self::anonymous()` for a blocklisted address, so every later question answers
no without the caller remembering to ask. `is_known_identity` and
`is_sign_in_eligible` both exclude blocklisted addresses, and `verify_link` and
`refresh_token` re-check it so that an already-issued link or session dies.
The one hole is documented and structural: the blocklist can only match an
address the caller *presented*, so a revoked person who omits their JWT and
presents the local token instead still holds `Ingest`/`Cancel`/`Review` — but
no bug authority, because bug routes do not consult the token.

### `is_authorized` — read the order, it is load-bearing

`pub fn is_authorized(state, headers, auth: Option<&AuthUser>, perm)` in
`src/api.rs`, in order:

1. blocklisted presented identity → `false`
2. `settings.server.testing_mode` → `true`
3. `state.allow_all_submit` → `true`
4. `presents_local_token(headers, state) && perm.granted_by_local_token()` → `true`
5. `acl.has_permission(user.email, perm)` for a presented identity
6. otherwise `false`

Any diff that reorders these, or adds a bypass above step 1, reintroduces
`0821dfe65763`. Any diff that adds a parameter derived from the peer address
reintroduces the bypass removed in `56773cd43da9`.

### `read_only`

`settings.server.read_only` is copied into `AppState::read_only` in
`build_router`. Every mutating handler must check it **first**, before any
other work: `submit_patch`, `rerun_patchset`, `release_patchset_embargo`,
`cancel_patchset`, `rerun_patch`, `analyze_bug`, `bug_action`, `forge_webhook`.
It is also folded into the advisory flags `get_config` reports
(`permissions.review/cancel/ingest`), `can_release_embargo` in
`get_patchset`/`get_patchset_summary`, and `can_comment`/`can_manage` in
`get_bug` — those are UI hints, not enforcement; the enforcement is the
handler's own early return.

### `--enable-unsafe-all-submit`

`Cli::enable_unsafe_all_submit` → `ServerOptions::allow_all_submit` →
`AppState::allow_all_submit`. It relaxes exactly two things:
- step 3 of `is_authorized`, i.e. every `Permission` for every caller,
  blocklisted addresses excepted;
- the secretless-webhook precondition in `forge_webhook`.

It does **not** relax bug routes: `Principal` never reads it, so
`/api/bug/*` still needs a session. `testing_mode` is the broader hammer — it
short-circuits `is_authorized` *and* makes `Principal` resolve
`testing_operator()` (operator, `may_create`, empty email).

## 3. Bug access control

`BugAccess` is an ordered enum: `None < Read < Comment < Manage`. Composition
is a maximum, so a security-list member who also maintains the affected
subsystem gets `Manage` (`test_strongest_grant_wins`).

- `Principal` is a **fallible** extractor: no valid session ⇒ 401, so a bug
  route cannot reach bug data without naming it in its signature. That is the
  only structural protection in this file; everything past it is hand-written.
  `get_bug` is the sole exception that extracts
  `Result<Principal, (StatusCode, &'static str)>` and passes it to
  `resolve_bug_read_principal(&query)`: when `query.by_bugid()` is true (the
  caller supplied the 122-bit random `bugid` / `slug` without a numeric `id`),
  missing or expired credentials degrade to `Principal::anonymous()` for a
  single-bug capability read; when `query.by_bugid()` is false (`?id=<i64>`),
  the `401` rejection is preserved.
- `OptionalPrincipal` degrades to `Principal::anonymous()`. Used by
  `get_patchset`, `get_patchset_summary`, `get_review`, `get_review_log`, which
  stay publicly readable and instead call `redact_embedded_bugs` to drop the
  `bugs` array members the caller may not read (and compute
  `can_release_embargo` on embargoed patchsets).
- Per-bug authority comes from `Database::authorizing_sections_for_bug(s)`,
  which selects only rows with `source = 'maintainers_section'`. A
  `path_prefix` or `caller_supplied` subsystem names nobody and confers
  nothing (`SubsystemSource::confers_authority`). **Never authorize from
  `Bug::subsystems` or `get_subsystems_for_bug`** — the doc comment on
  `authorizing_sections_for_bug` says `parse_bug_row_core` leaves
  `Bug::subsystems` empty anyway, so such a check would silently deny, or
  worse, be "fixed" by widening it.
- `SectionTitle` is a newtype whose `Eq`/`Hash`/`Ord` run on a normalized form
  (trim, collapse whitespace, lowercase) while `original()` preserves the
  MAINTAINERS spelling for SQL. It exists so that comparing a section title to
  a directory prefix is a type error. Do not accept a diff that compares raw
  `String` subsystem names in an authorization path.
- Helpers, and what each guarantees:
  - `readable_bug` → 404 for both "absent" and "not yours", byte for byte
    (evaluates `bug_access` with `by_bugid = false`).
  - `readable_bug_for_view` → used only by `get_bug`; evaluates
    `bug_access_for_query(..., query.by_bugid())` so knowing a bug's `bugid` /
    `slug` grants `BugAccess::Read` on that single bug (unless attenuated by a
    token's `max_bug_access = "none"` ceiling), while counterpart bugs in
    `attach_duplicate_relations` and `bug_family` are still filtered by the
    caller's `Principal` authority (`by_bugid = false`).
  - `bug_access` → the level, for a handler that must compare against a
    required level.
  - `readable_bug_ids` → batch filter, one query.
  - `transcript_bug` → `readable_bug` **plus**
    `principal.has_global_bug_visibility()`, refusing 403. This gates
    `/api/bug/raw`, `/api/bug/input`, `/api/bug/logs`, because a transcript
    embeds the problem statements of unrelated bugs from the deduplication
    stage.
- List scoping happens in SQL, not in Rust: `principal.visibility(&scope)` →
  `BugVisibility::Unrestricted | Sections(&[…])`, and both `list_bugs` and
  `get_subsystems_bug_counts` return empty for an empty scope. Filtering after
  the query would corrupt the pagination total, which is why the scope
  predicate is pushed in before the caller's own filters.
- `list_bug_subsystems` keys its cache on `lifecycle_status` **and** the scope
  (`"*"` for unrestricted, else the sections joined by `\n`). A diff that
  simplifies this cache key leaks one principal's counts to another.

## 4. Route reference

Gate column = what the handler body actually does. "—" means no authorization
at all, which is correct for public read routes and is the thing to re-check
when the payload changes.

| Route | Handler | Gate |
|---|---|---|
| `GET /health` | `health_check` | — |
| `GET /api/config` | `get_config` | — (reports `is_authorized` results per `Permission`) |
| `GET /api/lists` | `list_mailing_lists` | — |
| `GET /api/patchsets` | `list_patchsets` | — |
| `GET /api/messages` | `list_messages` | — |
| `GET /api/message` | `get_message` | — |
| `GET /api/patchset` | `get_patchset_summary` | `OptionalPrincipal` (computes `can_release_embargo`; payload carries no `bugs` key) |
| `GET /api/stats`, `/api/stats/timeline`, `/api/stats/reviews`, `/api/stats/tools` | `get_stats`, `stats_*` | — |
| `GET /api/patch` | `get_patchset` | `OptionalPrincipal` + `redact_embedded_bugs` (and computes `can_release_embargo`) |
| `GET /api/review` | `get_review` | `OptionalPrincipal` + `redact_embedded_bugs` |
| `GET /api/review_log` | `get_review_log` | `OptionalPrincipal` + `redact_embedded_bugs` |
| `POST /api/submit` | `submit_patch` | `read_only` + `Permission::Ingest` |
| `POST /api/patchset/rerun` | `rerun_patchset` | `read_only` + `Permission::Review` |
| `POST /api/patchset/release-embargo` | `release_patchset_embargo` | `read_only` + (`Permission::Review` or `principal.may_release_embargo(&authorizing_sections_for_patchset)`) |
| `POST /api/patch/rerun` | `rerun_patch` | `read_only` + `Permission::Review` |
| `POST /api/patchset/cancel` | `cancel_patchset` | `read_only` + `Permission::Cancel` |
| `POST /api/webhook/{provider}` | `forge_webhook` | `read_only` + webhook secret, else local token, else `allow_all_submit` (see forge.md) |
| `POST /api/auth/request-link` | `request_link` | rate limiter + `is_sign_in_eligible`; always answers 200 |
| `GET /api/auth/verify` | `verify_link` | `typ == "sign_in_link"` + blocklist |
| `POST /api/auth/refresh` | `refresh_token` | session + blocklist + 1-year `iat` cap |
| `GET /api/bug` | `get_bug` | `resolve_bug_read_principal` + `readable_bug_for_view` (capability Read by `bugid`/`slug`; `Principal` required for `id`) |
| `GET /api/bug/enrichments` | `get_bug_enrichments` | `Principal` + `readable_bug` |
| `GET /api/bugs` | `list_bugs` | `Principal` + `visibility()` in SQL |
| `GET /api/bugs/subsystems`, `GET /api/subsystems` | `list_bug_subsystems` | `Principal` + `visibility()`, scope-keyed cache |
| `GET /api/bug/raw`, `/api/bug/input`, `/api/bug/logs` | `get_bug_raw`, `get_bug_input`, `get_bug_logs` | `Principal` + `transcript_bug` (global visibility) |
| `POST /api/bug/analyze` | `analyze_bug` | `read_only` + `Principal::may_create()` |
| `POST /api/bug/action` | `bug_action` | `read_only` + `readable_bug` + `required_access(action)` |
| `GET /bug/{bugid}` | `redirect_bug` | — (redirect only) |

## 5. Bug patterns that actually happened here

**Authority inferred from the peer address** (`56773cd43da9`, and for webhooks
`16c5beb7920b`; earlier attempts to patch it: `302da7419641`,
`b35e8cebb742`). Ingest/rerun/cancel were granted to any loopback peer. Behind
the deployed reverse proxy that is the whole internet, and the forwarded
markers the check vetoed on are present only when the proxy is configured to
send them. *In a diff:* any use of `ConnectInfo`, `addr.ip().is_loopback()`,
`x-forwarded-for` or `x-real-ip` in a branch that returns "authorized".

**Bypass evaluated before the revocation** (`0821dfe65763`). The blocklist was
consulted after `testing_mode` and after `allow_all_submit`, so either flag
re-admitted a revoked address. *In a diff:* a new early-return in
`is_authorized`, or a new capability helper that checks a grant list without
first checking `is_blocklisted`.

**A capability reachable by a bypass it should not be** (`2c90f96cc06c`, then
`09e5666f08d5`). Filing a bug — which spends money on an LLM run — was gated on
`Permission::Review`, the same capability that reruns a patch review and which
the bypass handed to any unauthenticated local caller. The fix made
bypassability a property of the capability (`granted_by_local_token`, an
exhaustive match) and moved bug creation to `may_create`. *In a diff:* a new
`Permission` variant — the match will force a decision, but check the decision
is `false` unless the capability really is safe for an identity-less caller.

**Bug data leaking through a non-bug endpoint** (`c0428dbba917`, and the
`redact_embedded_bugs` / `attach_duplicate_relations` machinery). Patchset and
review payloads embed bug summaries; duplicate relations point at other bugs;
`bug_family` evidence spans a duplicate chain. Each had to be filtered
separately. *In a diff:* any new payload that gains a `bugs` array, a
`duplicate_of`, or anything derived from `bug_family`, on a route that does not
already run `readable_bug_ids` over those ids.

**A read level that was too coarse** (`198e11c8ad26`). Raw transcripts embed
the problem statements of every deduplication candidate, so `Read` on the bug
was not enough; the endpoints now demand `has_global_bug_visibility()`. Note
the deliberate status asymmetry: `readable_bug` answers 404 (existence is
secret), `transcript_bug` answers 403 (the caller already passed the read
check). *In a diff:* a new transcript-shaped endpoint (logs, prompts, raw
records, candidate payloads) wired to `readable_bug` instead of
`transcript_bug`.

**Token lifecycle** (`53cd80709379`, `115c1aeb2229`, `ca2feb8481e3`). HS256 is
pinned; only `typ == "session"` is accepted as a bearer; `refresh_token`
carries `iat` and `sid` forward and refuses past 1 year, so refreshing cannot
extend a session indefinitely. A sign-in link is *not* single-use — the code
no longer claims it is. Sign-in links are withheld from the log unless
`server.log_sign_in_links` is set, because the link is a bearer credential.
*In a diff:* a new `create_token` call — check `typ`, check the lifetime, and
check the result is not logged.

**Self-referencing duplicate** (`480ea5ddc273`, extended later). `bug_action`'s
`MarkDuplicate` must reject a target equal to the bug, a target that is itself
a duplicate, and a target the caller cannot `Manage` — the last one answers
with the same "choose an existing canonical bug" 400 as a missing target, so it
does not disclose that the target exists.

**Backend error strings returned to the caller** (`09e5666f08d5`). The AI
provider error names the provider and its configuration; the workflow error
quotes paths, prompts and provider responses. Both are now `tracing::error!`
plus a fixed sentence. *In a diff:* `.map_err(|e| (StatusCode::…, e.to_string()))`
on anything that touches the AI stack, git, or the filesystem. Note
`bug_action` still does exactly this for its database calls; a diff that adds
a new arm should not copy it for non-database errors.

## 6. Things that are conventions, not guarantees

- Nothing forces a handler to check `read_only` or a capability. Only
  `Principal`'s fallibility is structural.
- `is_authorized` is `pub`, so a caller outside `src/api.rs` could use it with
  a hand-built `HeaderMap`. Nothing prevents that.
- `AclSettings` matching is ASCII-case-insensitive only
  (`eq_ignore_ascii_case`). Non-ASCII case variants of an address are distinct.
- Enforcement lives in unit tests inside `src/api.rs`
  (`test_blocklist_outranks_every_bypass`,
  `test_loopback_grants_nothing_without_the_token`,
  `test_local_token_authorizes_ingest_but_grants_no_identity`,
  `test_bug_reads_require_an_authorized_principal`,
  `test_acl_blocklist_authorization_and_auth_endpoints`) and in
  `src/access.rs`/`src/settings.rs`. `tests/integration/` contains **no**
  authorization coverage. A new gate with no unit test has nothing holding it.
- Patchset review embargo (`embargo_until`) is enforced inside the db queries
  `get_patchset_details`/`get_patchset_summary`, not at the handler. It is a
  separate axis from the ACL and does not travel with a new query.

## Checklist for a diff that touches this area

- [ ] Every new route: does the handler name the capability that gates it? If
      it mutates, does it return early on `state.read_only` *before* doing
      anything else?
- [ ] Does any new grant depend on the peer address, `ConnectInfo`, or a
      forwarded header?
- [ ] New `Permission` variant: is `granted_by_local_token` `false` unless the
      capability is genuinely safe for a caller with no identity?
- [ ] New ACL list or grant helper: does it check `is_blocklisted` first, and
      is it reachable from `is_known_identity` so those principals can sign in?
- [ ] Reordered `is_authorized`: is the blocklist still ahead of
      `testing_mode` and `allow_all_submit`?
- [ ] Bug route: `Principal` (not `OptionalPrincipal`), and the right
      helper — `readable_bug` for bug data, `transcript_bug` for anything that
      can embed other bugs, `bug_access` compared against `required_access`
      for mutations?
- [ ] Any authorization comparing subsystem names: does it go through
      `authorizing_sections_for_bug(s)` and `SectionTitle`, never raw strings
      or `Bug::subsystems`?
- [ ] New payload field: can it carry another bug's id, bugid, title or problem
      statement? If so, is it filtered by `readable_bug_ids`?
- [ ] New refusal: does it distinguish "absent" from "forbidden" where the
      existing code deliberately does not (404 from `readable_bug`, 400 from
      `MarkDuplicate`)?
- [ ] New error path: does the response body carry a backend error string?
- [ ] New cache: is the key scoped by the principal's visibility?
- [ ] New unit test pinning the gate, or a stated reason there is none?
