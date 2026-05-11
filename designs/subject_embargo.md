# Design: Subject-Aware Embargo Policy Selection

## Problem Statement
Currently, when a patch is cross-posted to multiple mailing lists, the system uses the shortest explicitly configured embargo period (`min()`). However, closely related subsystems often cross-post. For example, a patch targeting `net-next` might be sent to `netdev@vger.kernel.org` (24h embargo) and CC'd to `bpf@vger.kernel.org` (0h embargo). Because of the `min()` rule, the patch incorrectly receives a 0-hour embargo, circumventing the intended 24-hour review period for the `net` subsystem.

## Proposed Solution
We will enhance `calculate_embargo_hours` to be "subject-aware". By inspecting the patch subject prefix (e.g., `[PATCH net-next ...]`), the system can determine the primary intended subsystem and prioritize its embargo policy over the fallback `min()` behavior.

### 1. Configuration Changes (`email_policy.toml` and `src/email_policy.rs`)
Add a new optional list `subject_prefixes` to the `SubsystemPolicy` struct. This allows administrators to explicitly map Git tree prefixes to their respective subsystem.

```toml
[subsystems.net]
lists = ["netdev@vger.kernel.org"]
embargo_hours = 24
subject_prefixes = ["net", "net-next", "netdev"]

[subsystems.bpf]
lists = ["bpf@vger.kernel.org"]
embargo_hours = 0
subject_prefixes = ["bpf", "bpf-next"]
```

### 2. Subject Parsing
Implement a lightweight regex or string parser in `src/patch.rs` (or directly in `main.rs`) to extract the text between `[` and `]` in the subject line, and isolate the tree name (ignoring `PATCH`, `RFC`, `v2`, `n/m`, etc.).
- `[PATCH net-next v3 07/13] ...` -> `net-next`
- `[RFC PATCH bpf-next] ...` -> `bpf-next`
- `[PATCH v2 bpf 0/6] ...` -> `bpf`

### 3. Updated Logic in `calculate_embargo_hours`
Update the function signature to accept the patch `subject: &str`.
The new evaluation priority will be:
1. Identify all subsystems matched by the `To`/`Cc` email addresses (same as today).
2. Extract the prefix from the `subject`.
3. Check if any of the **matched** subsystems contain this prefix in their `subject_prefixes` configuration.
    - If **YES**: Use the `embargo_hours` of that specific subsystem, ignoring the others.
    - If **NO**: Fall back to the existing behavior: return the minimum `embargo_hours` among all matched subsystems.

## Implementation Steps
1. Update `SubsystemPolicy` in `src/email_policy.rs` to include `subject_prefixes: Vec<String>` with `#[serde(default)]`.
2. Write a helper function `extract_subject_prefix(subject: &str) -> Option<String>` and add unit tests for it.
3. Update `calculate_embargo_hours` in `src/main.rs` to use this new logic.
4. Add unit tests for `calculate_embargo_hours` simulating the `net-next` vs `bpf` cross-posting scenario.

## Embargo Bypass Tokens (API Access)

### Problem Statement
The embargo prevents review findings, summaries, and per-patch review status
from being publicly visible until the policy window expires. However, a small
set of trusted parties (subsystem maintainers, security-coordinated reviewers,
internal automation) needs to read this content during the embargo window —
e.g. to triage findings before announcing them, or to feed reviews into a
private dashboard.

We do not want to build a full user/auth system for this. We want a low-cost,
out-of-band mechanism that grants pre-disclosure read access on a per-request
basis.

### Proposed Solution
Allow the operator to configure one or more **static bearer tokens** in
`ServerSettings.embargo_bypass_tokens`. A client that presents a matching
token is allowed to see embargoed fields on every API endpoint that returns
patchset or review data. A client that does not present a token (or presents
a non-matching one) sees the existing embargoed view — i.e. no token still
works for unembargoed content; the default is silent pass-through, **not**
`401 Unauthorized`.

This is a deliberately narrow capability: the token only suppresses the
embargo masking logic. It does not unlock write endpoints, does not bypass
the read-only flag, and does not change any other authorization.

### Configuration (`Settings.toml` / `ServerSettings`)
```toml
[server]
host = "0.0.0.0"
port = 8081
read_only = true
embargo_bypass_tokens = ["s3cret-rotated-monthly", "second-token-for-rotation"]
```

Multiple tokens may be active simultaneously to enable rotation without
downtime: deploy the new token, distribute it, then drop the old one in the
next deploy.

### Token Presentation
Clients may present a token in either of two ways:

1. **`Authorization: Bearer <token>`** — preferred, never leaks into logs or
   referrers. Use this for programmatic API consumers. The scheme name is
   matched case-insensitively per RFC 6750.
2. **`?token=<token>`** in the query string, URL-decoded — convenient for
   `curl` and one-off browser requests, at the cost of leaking the token
   into web-server access logs and any captured proxy traces. Use only over
   HTTPS, and rotate tokens that may have been exposed.

If both are presented, the `Authorization` header wins.

### Comparison
The presented token is hashed with SHA-256 and compared against pre-hashed
configured tokens (computed once at startup) using `subtle::ConstantTimeEq`
over the fixed 32-byte digests. All per-token results are OR-ed into a
`subtle::Choice` accumulator before being read out, so the comparison loop
does not short-circuit on the first match — that would otherwise leak
*which* configured token matched via timing. Hashing both sides also
removes any length-dependent behavior of `ct_eq` on raw byte slices.

### Logging
On a successful match, log the SHA-256 hash prefix of the matched token (not
the token itself):

```
embargo bypass granted (token_hash=ab12cd34…)
```

This lets operators correlate access in audit logs while keeping the secret
out of plain text. On a non-match, log nothing — failed presentations are
indistinguishable from anonymous requests by design.

### Server Plumbing
A new `BypassEmbargo(bool)` axum extractor inspects each request and produces
the bypass decision. Each handler that returns embargo-sensitive data accepts
the extractor and forwards the boolean to the corresponding `Database`
method:

- `list_patchsets` → `Database::get_patchsets(..., bypass_embargo)`
- `get_patchset`   → `Database::get_patchset_details{,_by_msgid}(..., bypass_embargo)`
- `get_patchset_summary` → `Database::get_patchset_summary{,_by_msgid}(..., bypass_embargo)`
- `get_review` / `get_review_log` → `Database::get_review_details(..., bypass_embargo)` and `Database::get_latest_review_for_patchset(..., bypass_embargo)`

Inside the DB layer, the existing `is_embargoed` computation becomes:

```rust
let is_embargoed = !bypass_embargo
    && embargo_until.map(|u| u > now).unwrap_or(false);
```

`get_review_details` additionally `LEFT JOIN`s `patchsets` to recover the
patchset's `embargo_until` (a review row has no embargo column of its own),
and returns a stub `{id, status: "Embargoed", embargo_until}` payload when
embargoed without bypass — never the actual review content.

### Cache Interaction
The `patchsets_homepage_cache` is shared across all callers. Caching a
bypass-view response would leak embargoed fields to subsequent anonymous
requests, so bypass requests **must** skip the cache and hit the DB
directly. The cached path is restricted to the default-no-token query
shape (no search, no mailing-list filter, page 1, default page size).

### Out of Scope (this branch)
The frontend is **not** modified on this branch. Bypass is a pure-API
capability: callers attach `Authorization: Bearer …` themselves, e.g.

```
curl -H "Authorization: Bearer $SASHIKO_BYPASS" \
     https://sashiko.example/api/patchset?id=42
```

A future change may add a UI affordance (storing the token in
`localStorage`, a corner badge, URL scrubbing); that work is intentionally
deferred so the API contract can stabilize first.

### Non-Goals
- **Per-token scopes / per-subsystem ACLs.** A token is all-or-nothing.
- **Token expiry / rotation automation.** Operators rotate manually by
  editing `Settings.toml` and restarting.
- **User identity / audit trail.** Logs identify the *token* (by hash prefix),
  not a person.

### Implementation Steps
1. Add `subtle = "2.6.1"` to `Cargo.toml`.
2. Add `embargo_bypass_tokens: Vec<String>` (with `#[serde(default)]`) to
   `ServerSettings` in `src/settings.rs`.
3. In `src/api.rs`: add the `BypassEmbargo` extractor, store the tokens
   on `AppState`, and thread the extractor through every embargo-sensitive
   handler.
4. In `src/db.rs`: add a `bypass_embargo: bool` parameter to the affected
   getters; replace the `is_embargoed` calculation to honor it; extend
   `get_review_details`'s SQL to join `patchsets.embargo_until` and emit
   the stub payload when embargoed.
5. Update in-tree callers and integration tests to pass `false`.
