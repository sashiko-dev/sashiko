# Design: Per-Subsystem Baseline Configuration

## 1. Problem Statement

Sashiko currently resolves git baselines for mailing-list patchsets using a
combination of `MAINTAINERS` `T:` entries, global `[[git.custom_remotes]]`
entries in `Settings.toml`, and hardcoded per-subsystem special cases in
`src/baseline.rs`. Over time, several recurring failure modes have required
one-off Rust workarounds:

1. **Stale or non-development `HEAD` branches**: `MAINTAINERS` `T:` entries
   frequently omit a branch name, causing Sashiko to target `remote/HEAD`.
   On repositories such as `tytso/ext4.git` (`HEAD` at a 2020 merge; active
   branch is `dev`), `gregkh/staging.git` (`HEAD` at `main`; active branches
   are `staging-testing`, `staging-next`), `akpm/mm.git` (`mm-new`,
   `mm-unstable`, `mm-stable`), `tnguy/next-queue.git` (`dev-queue`), and
   `cel/linux.git` (`nfsd-next`, `nfsd-testing`), patches fail to apply or
   apply against years-old code.
2. **`-next` vs `-fixes` tree selection**: Subsystems such as `netdev`
   (`net-next` vs `net`), `bpf` (`bpf-next` vs `bpf`), `intel-wired-lan`
   (`next-queue` vs `net-queue`), and `mptcp` (`export` vs `export-net`)
   maintain separate feature and fixes trees/branches. Untagged fix patches
   that carry a `Fixes:` trailer or untagged feature patches need a clear,
   declarative ordering rather than fragile substring heuristics.
3. **Global `[[git.custom_remotes]]` leakage**: Adding a subsystem topic
   branch to `[[git.custom_remotes]]` causes Sashiko to try those branches
   for every patchset across all subsystems, wasting worktree/apply attempts
   and risking accidental matches on unrelated trees.
4. **Cross-posted series**: A series sent to multiple lists (e.g.,
   `netdev@vger.kernel.org` and `intel-wired-lan@lists.osuosl.org`, or
   `netdev@vger.kernel.org` and `bpf@vger.kernel.org`) must prioritize the
   tree whose subject prefix or touched paths specifically match the series.

## 2. Goals

1. Configure per-subsystem baseline trees and branch orders directly inside
   `projects/<project>/mailing_lists.toml`, alongside existing email,
   embargo, Patchwork, and ingestion-filter policies.
2. Support single-tree multi-branch fallbacks, multi-tree (`-next` vs
   `-fixes`) setups, subject-prefix matching, file-path scoping, and
   `Fixes:`-trailer preference using a small, declarative schema.
3. Preserve dynamic hot-reloading (changes to `mailing_lists.toml` take
   effect without restarting the daemon) and strict validation
   (`deny_unknown_fields`, URL/branch sanitization).
4. Keep existing `base-commit:`, version/stable tag, `MAINTAINERS`,
   `[[git.custom_remotes]]`, `linux-next`, and mainline fallbacks intact so
   unconfigured lists and edge cases continue to apply cleanly.

## 3. Schema Design (`projects/<project>/mailing_lists.toml`)

Each mailing list block accepts an optional array of baseline rules via
`[["<list>".baselines]]`:

```toml
["bpf@vger.kernel.org"]
reply_to = ["author", "list"]
subject_prefixes = ["bpf", "bpf-next"]

[["bpf@vger.kernel.org".baselines]]
url = "https://git.kernel.org/pub/scm/linux/kernel/git/bpf/bpf-next.git"
subject_prefixes = ["bpf-next"]

[["bpf@vger.kernel.org".baselines]]
url = "https://git.kernel.org/pub/scm/linux/kernel/git/bpf/bpf.git"
subject_prefixes = ["bpf"]
```

Multi-branch single-tree example (`linux-mm`, `linux-ext4`):

```toml
[["linux-mm@kvack.org".baselines]]
url = "https://git.kernel.org/pub/scm/linux/kernel/git/akpm/mm.git"
branches = ["mm-new", "mm-unstable", "mm-stable"]

[["linux-ext4@vger.kernel.org".baselines]]
url = "https://git.kernel.org/pub/scm/linux/kernel/git/tytso/ext4.git"
branches = ["dev"]
```

Path-scoped example (`linux-nfs` covering NFSD topic branches without
affecting NFS client patches):

```toml
[["linux-nfs@vger.kernel.org".baselines]]
url = "https://git.kernel.org/pub/scm/linux/kernel/git/cel/linux.git"
branches = ["nfsd-next", "nfsd-testing"]
paths = [
    "fs/nfsd/",
    "fs/lockd/",
    "fs/nfs_common/",
    "include/linux/lockd/",
    "include/linux/sunrpc/",
    "net/sunrpc/",
]
```

### 3.1 Rust Types & Validation (`src/email_policy.rs`)

```rust
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BaselineRule {
    pub url: String,
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub subject_prefixes: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}
```

Added to `MailingListPolicy`:

```rust
#[serde(default)]
pub baselines: Vec<BaselineRule>,
```

Validation in `EmailPolicyConfig::validate`:
- `url`: non-empty after trim, no whitespace or control characters, must not
  start with `-`, and must begin with an allowed git URL scheme
  (`https://`, `http://`, `git://`, `ssh://`, or `file://`).
- `branches`: each entry must be non-empty after trim, must not start with
  `-`, and must not contain whitespace, control characters, `..`, `*`, or
  `?`. When omitted or empty, the rule targets the remote's default `HEAD`.
- `subject_prefixes`: no empty/whitespace-only entries.
- `paths`: no empty/whitespace-only entries; uses the same path-matching
  semantics as `IngestFilterPolicy` (trailing `/` for directory prefix,
  trailing `*` for narrow prefix glob, otherwise exact path match).

## 4. Candidate Resolution & Ordering (`src/baseline.rs`)

### 4.1 Overall Pipeline Order

`BaselineRegistry::resolve_candidates_with_policy` produces a deduplicated
candidate list in the following order:

1. Explicit `base-commit:` trailer from cover letter / patch body.
2. Subject version tags and explicit stable backport branches
   (`[STABLE v6.6]`, `[PATCH 6.1]`, GCC release branch).
3. Cover-letter / changelog body version tags (`applies on v7.0-rc3`).
4. Unversioned Linux stable backport branches.
5. **Configured mailing-list baseline rules** (new).
6. `MAINTAINERS` subsystem heuristic (existing).
7. Global `[[git.custom_remotes]]`.
8. `linux-next` (Linux projects only).
9. Mainline remote (`origin/master` + configured mirrors, or `HEAD`).

`BaselineRegistry::resolve_candidates` remains as a convenience wrapper
passing `policy = None, recipients = &[]`.

### 4.2 Matching & Specificity Tiers for Configured Rules

Given matched lists (every `(list_email, list_policy)` in `policy.lists`
whose bare address matches any recipient in `recipients`, sorted
deterministically by bare email):

For each `(list_idx, rule_idx, rule)`:
- `path_match`: `!rule.paths.is_empty()` and any touched file matches a
  pattern in `rule.paths`.
- **Path exclusion**: if `!rule.paths.is_empty()` and `!path_match`, the rule
  is skipped for this patchset so unrelated paths on a shared list do not
  trigger wrong topic trees.
- `prefix_match`: `!rule.subject_prefixes.is_empty()` and any bracketed
  token from `crate::patch::subject_prefix_tokens(subject)` matches an entry
  in `rule.subject_prefixes` (ASCII case-insensitive).

Each eligible rule is assigned a specificity tier (higher runs first; ties
preserve sorted list order and TOML declaration order within a list):

| Tier | Condition | Example |
|------|-----------|---------|
| 3 | `prefix_match && path_match` | `[PATCH iwl-next]` touching `drivers/net/ethernet/intel/` |
| 2 | `prefix_match` | `[PATCH bpf-next]` on `bpf@vger.kernel.org` |
| 1 | `path_match` | Patch touching `fs/nfsd/` on `linux-nfs@vger.kernel.org` |
| 0 | Unconditional rule (`subject_prefixes.is_empty()`) | `linux-ext4` -> `ext4.git dev`, `linux-mm` -> `akpm/mm` |
| -1 | Sibling fallback (remaining eligible rules whose `subject_prefixes` did not trigger) | If `[PATCH bpf]` fails on `bpf.git`, fall back to `bpf-next.git` before `linux-next` |

Trace of common scenarios:
- `[PATCH bpf-next]` on `bpf@vger.kernel.org`: `bpf-next` (Tier 2) > `bpf`
  (Tier -1) -> tries `bpf-next`, then `bpf`.
- `[PATCH bpf]` on `bpf@vger.kernel.org`: `bpf` (Tier 2) > `bpf-next`
  (Tier -1) -> tries `bpf`, then `bpf-next`.
- Untagged `[PATCH]` on `bpf@vger.kernel.org`: both Tier -1 -> declaration
  order (`bpf-next`, then `bpf`).
- `[PATCH iwl-next]` cross-posted to `netdev` + `intel-wired-lan`:
  `iwl-next` (`next-queue/dev-queue`, Tier 2) > `net-next` / `net`
  (Tier -1).

### 4.3 Remote Equivalence & Recipients

- In `BaselineRegistry::resolve_url`, if exact lookup in `remote_map`
  misses, fall back to `fetch_remotes` matched with
  `crate::git_ops::are_git_urls_equivalent`. This prevents `https://` in
  `mailing_lists.toml` from colliding with an existing `git://` remote in
  the local repository.
- Recipients passed to baseline resolution are gathered from the patchset's
  `to_recipients` and `cc_recipients` (via existing
  `get_patchset_routing_info`), falling back to the first patch's message
  headers when the patchset columns are empty.

## 5. Initial Linux Configuration (`projects/linux/mailing_lists.toml`)

Configure the well-known multi-tree and non-default-branch subsystems:
- `bpf@vger.kernel.org`: `bpf-next.git` (`bpf-next`), `bpf.git` (`bpf`)
- `netdev@vger.kernel.org`: `net-next.git` (`net-next`), `net.git` (`net`)
- `intel-wired-lan@lists.osuosl.org`: `tnguy/next-queue.git` (`dev-queue`,
  `iwl-next`), `tnguy/net-queue.git` (`dev-queue`, `iwl-net`)
- `mptcp@lists.linux.dev`: `mptcp_net-next.git` (`export` for `mptcp-next`;
  `export-net` for `mptcp`, `mptcp-net`)
- `linux-mm@kvack.org`: `akpm/mm.git` (`mm-new`, `mm-unstable`,
  `mm-stable`)
- `damon@lists.linux.dev`: `sj/linux.git` (`damon/next`), `akpm/mm.git`
  (`mm-new`, `mm-unstable`, `mm-stable`)
- `linux-ext4@vger.kernel.org`: `tytso/ext4.git` (`dev`)
- `linux-nfs@vger.kernel.org`: `cel/linux.git` (`nfsd-next`,
  `nfsd-testing`, scoped to NFSD/SUNRPC paths)

Existing `MAINTAINERS` heuristics remain intact as fallbacks for series
without matching list recipients (e.g., direct CLI injection or unconfigured
lists).

## 6. Validation Plan

- Unit tests in `src/email_policy.rs` covering valid/invalid `baselines`
  rules (scheme validation, branch validation, empty fields, unknown keys).
- Unit tests in `src/baseline.rs` covering:
  - `-next` vs `-fixes` selection via `subject_prefixes` and untagged
    fallback ordering.
  - Multi-branch ordering (`mm-new` -> `mm-unstable` -> `mm-stable`).
  - Path-scoped inclusion and exclusion (e.g., NFSD vs NFS client).
  - Cross-posted series priority (`iwl-next` on `netdev` + `intel-wired-lan`,
    `bpf-next` on `netdev` + `bpf`).
  - Precedence relative to `base-commit:`, stable tags, `MAINTAINERS`,
    `linux-next`, and mainline.
  - `git://` vs `https://` remote deduplication in `resolve_url`.
- Verify `projects/linux/mailing_lists.toml` parses and validates cleanly.
