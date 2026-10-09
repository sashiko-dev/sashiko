# Design: Unified Per-Mailing-List Configuration & Project Layout

## 1. Overview & Motivation

Sashiko's mailing list tracking and outbound email routing grew organically around a Linux-only deployment and currently suffer from several usability and correctness pitfalls:

1. **Split source of truth for mailing lists:**
   Tracking a mailing list requires adding a short name to `SASHIKO__MAILING_LISTS__TRACK` in `deployment/sashiko.dev/base/app/sashiko-k8s.yaml`, while outbound email/patchwork/embargo policies live in `deployment/sashiko.dev/email_policy.toml` (and a separate muted stub lives at `/email_policy.toml`). Contributors frequently update `email_policy.toml` without updating `sashiko-k8s.yaml` (e.g. `linux-kbuild` in PR #657), or use mismatched names (`sashiko-k8s.yaml` tracks `target-devel`, while `email_policy.toml` configures `linux-target@vger.kernel.org`, which does not exist in `MAINTAINERS` and never matches).
2. **Redundant `[subsystems.<name>]` wrapper:**
   Every entry in `email_policy.toml` is written under `[subsystems.<name>]` with a single-element `lists = ["<email>"]`. Across the entire codebase, `policy.subsystems` is only ever iterated via `.values()` — the `<name>` key is unused.
3. **No direct way to say "reply to this mailing list":**
   `SubsystemPolicy` has `reply_to_author`, `cc_individuals`, and `reply_all`, but no `"list"` target. To reply to their own list without enabling `reply_all`, 46+ blocks duplicate their mailing list address inside `cc = ["<list>"]`.
4. **Cross-posting veto breaks `reply_all = true`:**
   In `EmailRouter::resolve_recipients`, if *any* matched policy has `reply_all = false`, `is_private` becomes `true` and strips all known mailing lists from `To:`/`Cc:`. Because 46+ subsystems leave `reply_all = false` and rely on the `cc = ["<list>"]` workaround, any patch cross-posted between a `reply_all = true` list (such as `live-patching`, `nova-gpu`, `mtd`, or `kbuild`) and a `reply_all = false` list (such as `dri-devel`, `rust-for-linux`, `devicetree`, or `linux-modules`) silently drops the `reply_all = true` mailing list unless it was also duplicated in `cc`.
5. **`cc_individuals` leaks to unconfigured mailing lists:**
   `EmailRouter` only treats addresses in `policy.subsystems[*].lists` as mailing lists. Any mailing list without an explicit policy block (such as `linux-kernel@vger.kernel.org`) is treated as an individual recipient when `cc_individuals = true`.
6. **Naive recipient deduplication:**
   `final_to` and `final_cc` deduplicate by raw `HashSet<String>`. When a patch header contains `"Nathan Chancellor" <nathan@kernel.org>` and the static `cc` list contains `"nathan@kernel.org"`, both strings are emitted in the outgoing email headers.
7. **No author-only positive review option:**
   Maintainers who want authors to receive positive ("no issues found") confirmation without adding noise to the mailing list cannot express this because `send_positive_review` is a boolean that uses the same recipient set as negative reviews.
8. **Silent TOML typos:**
   `EmailPolicyConfig` and `SubsystemPolicy` do not reject unknown fields, allowing typos like `[subsystem.kbuild]` or `cc_maintainers` to parse silently as no-ops.

---

## 2. Project Directory Layout (Option B)

To separate Kubernetes infrastructure (`deployment/`) from project configuration while keeping `third_party/prompts/` intact as a clean mirror of upstream `masoncl/review-prompts`:

```text
projects/
  linux/
    mailing_lists.toml      # Single source of truth for Linux lists (tracking + email/patchwork/embargo)
  sashiko/
    prompts/                # First-party Sashiko self-review prompts (moved from prompts/sashiko/)
third_party/
  prompts/                  # Unchanged upstream mirror (kernel/, systemd/, iproute/, REVISION)
```

* **Future projects** (e.g. `gcc`, `qemu`, `llvm`) place their mailing list configuration at `projects/<project>/mailing_lists.toml` and their first-party prompts at `projects/<project>/prompts/`.
* `ProjectId::mailing_lists_path(self)` returns `"projects/<project>/mailing_lists.toml"`.
* `build.rs` collects vendored prompts from `third_party/prompts/` and first-party prompts from `projects/<project>/prompts/` (mapping `projects/<project>/prompts/**` into `<project>/**` in the embedded prompt bundle).
* The root stub `email_policy.toml` and `deployment/sashiko.dev/email_policy.toml` are replaced by `projects/linux/mailing_lists.toml`. Local runs remain safe by default because NNTP tracking requires `--track` and SMTP delivery requires `[smtp]` with `dry_run = false`.
* `Dockerfile` replaces `COPY deployment/sashiko.dev/email_policy.toml /app/email_policy.toml` with `COPY projects /app/projects`.

---

## 3. Schema Design (`projects/<project>/mailing_lists.toml`)

Each mailing list is configured as a top-level table keyed directly by its email address alongside `[defaults]`:

```toml
[defaults]
# Whether lists in this file are tracked and ingested via NNTP by default.
track = true

# Who from the original patch receives the review email:
# - "author": the patch author (From: header -> To:)
# - "list": this block's mailing list address (-> Cc:)
# - "recipients": all individual (non-mailing-list) To:/Cc: recipients on the patch (-> Cc:)
reply_to = []

# When a review finds no issues, who receives the positive review email:
# - "none": do not send positive review emails (default)
# - "author": send positive review only to the patch author (To: author, no list/CCs)
# - "all": send positive review to the full resolved recipient set
positive_review = "none"

# Hours to hold reviews with findings before sending (0 = immediate).
embargo_hours = 0

# Author email addresses whose submissions are ignored and never reviewed.
ignored_emails = ["patchwork@emeril.freedesktop.org"]

# --- Tracked for Web UI only (no outbound emails) ---

["linux-kernel@vger.kernel.org"]
["linux-mm@kvack.org"]
["linux-xfs@vger.kernel.org"]

# --- Tracked with Email / Embargo / Patchwork Policies ---

["linux-usb@vger.kernel.org"]
reply_to = ["author", "list"]
positive_review = "all"

["devicetree@vger.kernel.org"]
reply_to = ["author", "list"]
cc = ["robh@kernel.org", "conor+dt@kernel.org"]

["linux-kbuild@vger.kernel.org"]
reply_to = ["author", "list", "recipients"]
cc = ["nathan@kernel.org", "nsc@kernel.org"]
embargo_hours = 24

["rust-for-linux@vger.kernel.org"]
reply_to = ["author"]
cc = ["ojeda@kernel.org", "gary@garyguo.net"]

["selinux@vger.kernel.org"]
reply_to = ["list"]
positive_review = "all"

["linux-nfs@vger.kernel.org"]
cc = ["Chuck Lever <cel@kernel.org>", "Jeff Layton <jlayton@kernel.org>", "Anna Schumaker <anna@kernel.org>"]

["netdev@vger.kernel.org"]
embargo_hours = 24
subject_prefixes = ["net", "net-next"]

["mptcp@lists.linux.dev"]
reply_to = ["author", "list"]
["mptcp@lists.linux.dev".patchwork]
enabled = true
api_url = "https://patchwork.kernel.org/api/1.3"
```

### 3.1 Rust Types & Validation

```rust
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ReplyTarget {
    Author,
    List,
    Recipients,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PositiveReviewPolicy {
    #[default]
    None,
    Author,
    All,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct DefaultListPolicy {
    #[serde(default = "default_true")]
    pub track: bool,
    #[serde(default)]
    pub reply_to: Vec<ReplyTarget>,
    #[serde(default)]
    pub positive_review: PositiveReviewPolicy,
    #[serde(default)]
    pub mute_all: bool,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub ignored_emails: Vec<String>,
    #[serde(default)]
    pub patchwork: PatchworkPolicy,
    #[serde(default)]
    pub embargo_hours: u32,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct MailingListPolicy {
    pub track: Option<bool>,
    pub nntp_group: Option<String>,
    pub reply_to: Option<Vec<ReplyTarget>>,
    pub positive_review: Option<PositiveReviewPolicy>,
    pub mute_all: Option<bool>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub ignored_emails: Vec<String>,
    #[serde(default)]
    pub subject_prefixes: Vec<String>,
    #[serde(default)]
    pub patchwork: PatchworkPolicy,
    pub embargo_hours: Option<u32>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct MailingListsConfig {
    #[serde(default)]
    pub defaults: DefaultListPolicy,
    #[serde(flatten)]
    pub lists: HashMap<String, MailingListPolicy>,
}
```

**Strict Validation on Load:**
1. `DefaultListPolicy`, `MailingListPolicy`, and `PatchworkPolicy` carry `#[serde(deny_unknown_fields)]`, so any unknown key inside a block fails immediately.
2. `MailingListsConfig::load` validates that every key in `lists` is a valid email address containing `@` (rejecting top-level section typos like `[subsystem.kbuild]`).
3. `MailingListsConfig::load` validates that no list repeats its own key address inside its static `cc` array.

---

## 4. Behavioural Semantics

### 4.1 Recipient Resolution (`EmailRouter::resolve_recipients`)

Given incoming `To:` (`incoming_to`), `Cc:` (`incoming_cc`), and `From:` (`patch_author`):

1. **Bot loop guard & match active mailing lists:**
   * Check `EmailRouter::is_ignored_author(policy, patch_author)` and verify that `patch_author` does not match `sashiko_address` (enforced in both `EmailRouter::resolve_recipients` and `Reviewer::queue_notifications`); if matched, return `Action::Mute` immediately.
   * Find all `(list_email, list_policy)` entries in `config.lists` where `list_email` matches any address in `incoming_to` or `incoming_cc` (case-insensitive email comparison).
   * If no configured list matches, evaluate against `config.defaults`.
   * If any matched policy has `mute_all = true`, return `Action::Mute`.

2. **Additive recipient construction (no cross-list vetoes):**
   * **Author (`To:`):** If any matched policy contains `ReplyTarget::Author` in `reply_to` (and `patch_author` is non-empty), add `patch_author` to `To:`.
   * **Subsystem Mailing Lists (`To:` / `Cc:`):** For each matched `(list_email, list_policy)`, if `list_policy.reply_to` contains `ReplyTarget::List`, keep `list_email` (in `To:` if it appeared in `incoming_to`, otherwise in `Cc:`).
     * *Crucial property:* Each mailing list controls only itself. Cross-posting to `rust-for-linux` (`reply_to = ["author"]`) and `linux-kbuild` (`reply_to = ["author", "list", "recipients"]`) includes `linux-kbuild@vger.kernel.org` and excludes `rust-for-linux@vger.kernel.org`.
   * **Individual Patch Recipients (`To:` / `Cc:`):** If any matched policy contains `ReplyTarget::Recipients` in `reply_to`, include all non-mailing-list addresses from `incoming_to` and `incoming_cc`.
     * An address is recognized as a mailing list (`is_mailing_list`) if it matches any key in `config.lists` OR belongs to a known mailing-list domain (`KNOWN_MAILING_LIST_DOMAINS`).
   * **Static `cc` (`Cc:`):** Add all entries from `list_policy.cc` for each matched policy, plus `config.defaults.cc`.

3. **Positive Review Routing (`EmailRouter::resolve_positive_recipients`):**
   * Effective positive review mode across matched policies:
     * If any matched policy resolves to `PositiveReviewPolicy::All`, `EmailRouter::resolve_positive_recipients` constructs the `To:` and `Cc:` sets using **only** the matched policies that opted into `PositiveReviewPolicy::All` (plus the author), ensuring cross-posted lists that set `positive_review = "none"` or `"author"` never receive positive review emails.
     * Else if any matched policy resolves to `PositiveReviewPolicy::Author`, positive reviews are sent **only** to `patch_author` (`To: [patch_author]`, `Cc: []`).
     * Else (`PositiveReviewPolicy::None`), no positive review email is queued.

4. **Email-Aware Deduplication & Self-Filtering:**
   * Extract the normalized lowercase email address (`user@domain`) for every candidate in `To:` and `Cc:`.
   * Strip `sashiko_address`.
   * If the same normalized email appears multiple times in `To:` or `Cc:`, keep a single entry, preferring the formatted `"Display Name" <user@domain>` representation over a bare `user@domain`.
   * Remove any address from `Cc:` whose normalized email is already present in `To:`.

### 4.2 NNTP Tracking & Group Resolution (`Ingestor`)

1. **Tracked list source:**
   * If `settings.mailing_lists.track` is explicitly populated via CLI/env override, use it.
   * Otherwise, read all `(list_email, policy)` from `projects/<project>/mailing_lists.toml` where `policy.track.unwrap_or(defaults.track)` is `true`.
2. **NNTP group resolution:**
   * If `policy.nntp_group` is explicitly set (for lists whose lore.kernel.org NNTP group name does not match their email address), use it directly:
     * `devicetree@vger.kernel.org` $\rightarrow$ `nntp_group = "org.kernel.vger.linux-devicetree"`
     * `b.a.t.m.a.n@lists.open-mesh.org` $\rightarrow$ `nntp_group = "org.open-mesh.lists.batman"`
   * Otherwise, convert `local@d1.d2...dn` to `dn...d2.d1.local`:
     * `linux-kbuild@vger.kernel.org` $\rightarrow$ `org.kernel.vger.linux-kbuild`
     * `linux-mm@kvack.org` $\rightarrow$ `org.kvack.linux-mm`
     * `dri-devel@lists.freedesktop.org` $\rightarrow$ `org.freedesktop.lists.dri-devel`
     * `mptcp@lists.linux.dev` $\rightarrow$ `dev.linux.lists.mptcp`
   * Still verify resolved group names against `nntp.lore.kernel.org` `LIST` output on startup so any typo or unlisted group logs a clear warning instead of failing `GROUP` commands silently.

---

## 5. Risks & Mitigations

1. **NNTP Group Name Mismatches & High-Water Mark Reset (`mailing_lists` table):**
   * **Risk:** In SQLite (`src/migrations/001_initial.sql`, `ensure_mailing_list` in `src/db.rs`), the `mailing_lists` table stores `last_article_num` keyed by `nntp_group TEXT NOT NULL UNIQUE`. If a list's resolved `nntp_group` string changes, Sashiko either fails `GROUP <name>` on `nntp.lore.kernel.org` or inserts a new row with `last_article_num = 0` and re-ingests up to `max_catchup` (4,000) old messages (`src/ingestor.rs`).
   * **Finding:** Querying `LIST` on `nntp.lore.kernel.org` across all 88 tracked lists showed that 86 lists match `<reversed-domain>.<local-part>` identically, while **2 lists do not**:
     * `devicetree@vger.kernel.org` is hosted at NNTP group `org.kernel.vger.linux-devicetree` (not `org.kernel.vger.devicetree`).
     * `b.a.t.m.a.n@lists.open-mesh.org` is hosted at NNTP group `org.open-mesh.lists.batman` (not `org.open-mesh.lists.b.a.t.m.a.n`).
   * **Mitigation:** Support an optional `nntp_group` field on `MailingListPolicy` and set it on those two entries (`nntp_group = "org.kernel.vger.linux-devicetree"` and `nntp_group = "org.open-mesh.lists.batman"`), plus keep `LIST` verification in `Ingestor::resolve_tracked_group`. This guarantees all 88 `nntp_group` keys in SQLite remain byte-for-byte identical after migration.

2. **`subsystems` Table `UNIQUE` Constraints & Web UI Tag Continuity:**
   * **Risk:** `identify_subsystems` in `src/main.rs` populates the `subsystems` table (`name TEXT NOT NULL UNIQUE, mailing_list_address TEXT NOT NULL UNIQUE`), which drives the "Mailing Lists" tags in the Web UI. Currently, `linux-kernel@vger.kernel.org` is special-cased to name `"LKML"`, while all other lists use the local-part before `@`.
   * **Mitigation:** Preserve the existing naming rule in `identify_subsystems` (`"LKML"` for `linux-kernel@vger.kernel.org`, local-part for others) so `ensure_subsystem` never hits a `UNIQUE` constraint conflict against existing production database rows.

3. **`reply_to = ["recipients"]` Behaviour with `linux-kernel@vger.kernel.org` (LKML):**
   * **Risk / Behaviour Change:** Previously, `known_mailing_lists` only contained lists present in `email_policy.toml`. Because `linux-kernel@vger.kernel.org` was not in `email_policy.toml`, subsystems with `cc_individuals = true` (`kbuild`, `kexec`, `live-patching`, `nova-gpu`, `mtd`, `linux-fscrypt`, `fsverity`) treated `linux-kernel@vger.kernel.org` as an individual recipient and CC'd LKML whenever the patch was also sent to LKML.
   * **Impact:** Once `["linux-kernel@vger.kernel.org"]` is in `projects/linux/mailing_lists.toml` with default `reply_to = []` (and `@vger.kernel.org` is recognized as a mailing-list domain), `"recipients"` will only include actual human/individual addresses, not `linux-kernel@vger.kernel.org` (unless `["linux-kernel@vger.kernel.org"]` itself sets `reply_to = ["list"]`).
   * **Decision Point:** Confirm whether dropping `linux-kernel@vger.kernel.org` from replies (while still replying to the subsystem list like `linux-kbuild@vger.kernel.org` and all individual CCs) is desired, or if `["linux-kernel@vger.kernel.org"]` should stay on `Cc:` when another active list replies to the thread.

4. **Kubernetes Manifest vs. Container Image Rollout Order:**
   * **Risk:** In production, `sashiko-k8s.yaml` currently passes `SASHIKO__REVIEW__EMAIL_POLICY_PATH=/app/email_policy.toml` and `SASHIKO__MAILING_LISTS__TRACK=...`. If a new container image is deployed before `sashiko-k8s.yaml` is re-applied (or vice versa), a missing `/app/email_policy.toml` or legacy env var could cause confusion.
   * **Mitigation:**
     * Keep backward-compatible parsing or a symlink `/app/email_policy.toml -> /app/projects/linux/mailing_lists.toml` in `Dockerfile` and support `SASHIKO__REVIEW__EMAIL_POLICY_PATH` as an alias for the mailing lists config path during the transition.
     * Allow `SASHIKO__MAILING_LISTS__TRACK` to match either short names (`linux-kbuild`), NNTP groups (`org.kernel.vger.linux-kbuild`), or email addresses (`linux-kbuild@vger.kernel.org`) if an old k8s deployment env var is still set.

---

## 6. Step-by-Step Implementation Plan

1. **Step 1 — Move `prompts/sashiko` to `projects/sashiko/prompts` and update `build.rs`:**
   * Move `prompts/sashiko/**` to `projects/sashiko/prompts/**`.
   * Update `build.rs` to scan `projects/*/prompts` for first-party project prompts while keeping `third_party/prompts` unchanged.
2. **Step 2 — Implement the new `MailingListsConfig` / `EmailRouter` / `PositiveReviewPolicy` and email-aware deduplication:**
   * Update `src/email_policy.rs` and `src/email_router.rs` with `ReplyTarget`, `PositiveReviewPolicy`, `#[serde(deny_unknown_fields)]`, key validation, and address-normalized deduplication.
   * Update `src/main.rs` (`calculate_embargo_hours`) and `src/reviewer.rs` to support `PositiveReviewPolicy::Author`.
3. **Step 3 — Create `projects/linux/mailing_lists.toml` and wire `ProjectId` & `Ingestor`:**
   * Migrate all 64 policies from `deployment/sashiko.dev/email_policy.toml` and all tracked lists from `deployment/sashiko.dev/base/app/sashiko-k8s.yaml` into `projects/linux/mailing_lists.toml` (fixing `target-devel@vger.kernel.org`, `nova-gpu`, `mtd`, and `live-patching` in the process).
   * Wire `Ingestor` to derive tracked NNTP groups from `projects/<project>/mailing_lists.toml` when `mailing_lists.track` is not overridden.
   * Remove `SASHIKO__MAILING_LISTS__TRACK` and `SASHIKO__REVIEW__EMAIL_POLICY_PATH` from `sashiko-k8s.yaml`, update `Dockerfile` and docs (`MAINTAINERS_GUIDE.md`, `docs/examples/email_policy.toml`), and remove the old `email_policy.toml` files.
