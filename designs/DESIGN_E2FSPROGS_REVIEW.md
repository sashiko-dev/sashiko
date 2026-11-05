# Design: Sashiko Code Review Support for e2fsprogs

## 1. Overview & Motivation

`e2fsprogs` provides the canonical userspace utilities and shared libraries for
the `ext2`, `ext3`, and `ext4` filesystems (`e2fsck`, `mke2fs`, `resize2fs`,
`tune2fs`, `debugfs`, `dumpe2fs`, `e2image`, `e2undo`, `e4defrag`, `e2scrub`,
`fuse2fs`, and the `libext2fs`, `libe2p`, `libcom_err`, and `libss` libraries).

Although `e2fsprogs` is a C userspace codebase rather than kernel code, it
follows Linux-kernel-style C coding conventions (`tabs`, 80-column formatting,
`goto errout`/`cleanup` resource lifetimes, `Signed-off-by:` DCO trailers,
`Fixes:` commit trailers, and LKML-style inline plain-text patch review) and
shares the exact on-disk filesystem format invariants of Linux `fs/ext4/` and
`fs/jbd2/`. At the same time, its threat model, memory/I/O abstractions
(`errcode_t`, `ext2fs_get_mem`, `io_channel`, `blk64_t`), shared-library ABI
stability guarantees (`libext2fs`, `libe2p`, `libcom_err`, `libss`), and
repair/fuzzing safety requirements (`e2fsck` and `fuse2fs` operating on
corrupted or untrusted filesystem images) differ substantially from kernel
space.

This document specifies first-class `--project e2fsprogs` support in Sashiko:
1. Repository provisioning and branch-aware baseline resolution (`next`,
   `master`, `maint`).
2. Simultaneous ingestion of `linux-ext4@vger.kernel.org` via Lore NNTP (with
   declarative ingestion filtering to exclude Linux kernel and `xfstests`
   traffic on the shared list) and GitHub pull requests on `tytso/e2fsprogs`.
3. Active mailing-list email delivery and GitHub PR review comment delivery.
4. A dedicated `e2fsprogs` multi-stage review workflow, three-layer prompt
   hierarchy, and code-notes profile/agenda under `projects/e2fsprogs/`.
5. Standalone Kubernetes Kustomize manifests under
   `deployment/e2fsprogs.sashiko.dev/`.

---

## 2. Repository Provisioning & Baseline Resolution

### 2.1 `ProjectId::E2fsprogs`
`ProjectId` gains `E2fsprogs`:
- Selector (`as_str`, CLI `--project`, `Settings.project.kind`): `"e2fsprogs"`
- Display name: `"e2fsprogs"`
- Bundled prompt namespace (`prompt_dir`): `"e2fsprogs"`
  (`projects/e2fsprogs/prompts/` -> `<cache>/prompts/e2fsprogs/`)
- Mailing-list policy path: `"projects/e2fsprogs/mailing_lists.toml"`
- `uses_maintainers`: `false` (`e2fsprogs` has no Linux `MAINTAINERS` file)
- Upstream repository (`ProjectId::upstream`):
  - `origin`: `https://git.kernel.org/pub/scm/fs/ext2/e2fsprogs.git`
  - `seed`: `Seed::Mirror("https://github.com/tytso/e2fsprogs.git")` so initial
    `sashiko --project e2fsprogs repo sync` clones rapidly from GitHub and
    switches `origin` to `git.kernel.org`.

### 2.2 Baseline Resolution (`src/baseline.rs` & `src/workflows/linux_bug.rs`)
1. **Linux remote isolation:** `is_non_linux_project_url` recognizes
   `e2fsprogs` repository URLs (`e2fsprogs`) so `BaselineRegistry` never
   injects `linux-next` or Linux `stable` remotes into an `e2fsprogs` tree.
2. **Candidate branches:** `e2fsprogs` development uses `next` (integration
   branch), `master` (mainline release branch), and `maint` (maintenance fixes).
   When resolving patches without an explicit `base-commit:` or when a
   `base-commit:` is missing locally:
   - Explicit subject branch hints such as `[PATCH maint]` or `[PATCH next]`
     prioritize the matching branch (`{remote}/maint` or `{remote}/next`).
   - Otherwise `BaselineRegistry` tries `{remote}/next`, `{remote}/master`,
     `{remote}/maint`, and `HEAD` in order so patches based on integration,
     mainline, or maintenance branches apply cleanly.
3. **Bug pipeline mainline ref:** `resolve_linus_sha` in
   `src/workflows/linux_bug.rs` recognizes `e2fsprogs` remote URLs and resolves
   `{remote}/master` and `{remote}/next` before falling back to `HEAD`.

---

## 3. Dual Ingestion & Shared Mailing-List Filtering

### 3.1 Simultaneous NNTP and GitHub Forge Operation
`e2fsprogs` accepts contributions through two active channels:
1. Patch emails sent to `linux-ext4@vger.kernel.org` (archived on
   `nntp.lore.kernel.org` as `org.kernel.vger.linux-ext4`).
2. Pull requests opened against `https://github.com/tytso/e2fsprogs`.

`e2fsprogs.sashiko.dev` enables both channels concurrently:
- `nntp.server = "nntp.lore.kernel.org"`
- `forge.enabled = true`
- `forge.disable_nntp = false`
- `forge.provider = "github"`
- `forge.post_mode = "live"`
- `forge.repos = ["tytso/e2fsprogs"]`

### 3.2 Outbound Routing Separation
In `projects/e2fsprogs/mailing_lists.toml`:
- `[defaults]` keeps `reply_to = []`, `positive_review = "none"`, and `cc = []`.
- `["linux-ext4@vger.kernel.org"]` configures:
  - `reply_to = ["author", "list"]`
  - `positive_review = "all"`
  - `cc = ["tytso@mit.edu"]`

Because GitHub PR commits ingested via `git-fetch` have empty `To:`/`Cc:`
headers and carry `mr_url`, `EmailRouter::resolve_recipients` falls back to
`[defaults]` (`Action::Mute`) for GitHub PRs while `Reviewer` enqueues the PR
review to `forge_outbox`. Conversely, mailing-list patches on
`linux-ext4@vger.kernel.org` have `mr_url = None` (skipping `forge_outbox`) and
match `["linux-ext4@vger.kernel.org"]`, delivering plain-text inline email
replies to the patch author, `linux-ext4@vger.kernel.org`, and
`tytso@mit.edu`.

### 3.3 Shared Mailing-List Ingestion Filter (`ingest_filter`)
`linux-ext4@vger.kernel.org` is shared across three distinct trees:
- Linux kernel (`fs/ext4/`, `fs/jbd2/`, `fs/mbcache.c`, `include/linux/`,
  `Documentation/filesystems/ext4/`, etc.)
- `xfstests` / `fstests` (`tests/ext4/`, `tests/generic/`, `tests/shared/`,
  `common/`, `ltp/`, `src/`, usually tagged `[PATCH fstests]`)
- `e2fsprogs` (`lib/ext2fs/`, `lib/e2p/`, `lib/et/`, `lib/ss/`, `lib/support/`,
  `lib/blkid/`, `lib/uuid/`, `e2fsck/`, `misc/`, `resize/`, `debugfs/`,
  `scrub/`, `fuse2fs/`, `ext2ed/`, `tests/[a-zA-Z]_*`, `po/`, `doc/`, `util/`,
  `config/`, `MCONFIG.in`, `configure.ac`, etc.)

Without ingestion filtering, every Linux kernel and `xfstests` patch on
`linux-ext4@vger.kernel.org` would enter `e2fsprogs.sashiko.dev` and fail patch
application as `FailedToApply`, or leave cover-letter-only series stuck in
`Incomplete`.

To solve this cleanly without project-hardcoded hacks in `src/main.rs`,
`MailingListPolicy` in `src/email_policy.rs` gains an optional sub-table
`["<list>".ingest_filter]` (`IngestFilterPolicy`):

```toml
["linux-ext4@vger.kernel.org".ingest_filter]
allowed_subject_prefixes = ["e2fsprogs"]
ignored_subject_prefixes = ["fstests", "xfstests", "kernel", "linux", "syzbot"]
allowed_paths = [
    "lib/ext2fs/",
    "lib/e2p/",
    "lib/et/",
    "lib/ss/",
    "lib/support/",
    "lib/blkid/",
    "lib/uuid/",
    "lib/Makefile",
    "lib/config.h.in",
    "lib/dirpaths.h.in",
    "lib/fpopen.c",
    "e2fsck/",
    "misc/",
    "debugfs/",
    "resize/",
    "scrub/",
    "fuse2fs/",
    "ext2ed/",
    "po/",
    "doc/",
    "util/",
    "intl/",
    "config/",
    "contrib/",
    "debian/",
    "tests/",
    "MCONFIG.in",
    "Makefile.in",
    "configure",
    "configure.ac",
    "version.h",
    "wordwrap.pl",
    "depfix.sed",
    "INSTALL",
    "README",
    "RELEASE-NOTES",
    "SHLIBS",
    "SUBMITTING-PATCHES",
    ".github/",
]
ignored_paths = [
    "fs/",
    "include/",
    "Documentation/",
    "arch/",
    "block/",
    "drivers/",
    "kernel/",
    "mm/",
    "net/",
    "rust/",
    "scripts/",
    "security/",
    "tools/",
    "MAINTAINERS",
    "Kbuild",
    "Kconfig",
    "tests/ext4/",
    "tests/generic/",
    "tests/shared/",
    "tests/xfs/",
    "tests/btrfs/",
    "tests/f2fs/",
    "tests/overlay/",
    "tests/Makefile",
    "common/",
    "ltp/",
    "src/",
]
```

#### Filter Evaluation Rules (NNTP Ingestion Only)
When `process_parsed_article` processes an NNTP message (`source ==
MessageSource::Nntp`) whose `group` matches a configured list with a non-empty
`ingest_filter`:
1. **Ignored subject prefix:** Extract bracketed subject prefix tokens via
   `patch::get_subject_prefixes(&metadata.subject)`. If any token matches
   `ignored_subject_prefixes` (case-insensitive), reject as a patch/cover.
2. **Allowed subject prefix:** If any subject prefix token matches
   `allowed_subject_prefixes` (case-insensitive, e.g., `[PATCH e2fsprogs 0/4]`
   or `[e2fsprogs PATCH]`), accept unless diff/diffstat paths hit
   `ignored_paths`.
3. **Path evaluation (diff or cover-letter diffstat):**
   - For patch messages (`patch_opt.is_some()`), extract touched paths from the
     unified diff (`baseline::extract_files_from_diff`).
   - For cover letters (`metadata.index == 0 && patch_opt.is_none()`), extract
     touched paths from any `git format-patch --cover-letter` diffstat in
     `metadata.body`.
   - If extracted paths are non-empty:
     - Reject if **any** path matches `ignored_paths` (directory prefix ending
       in `/` or exact file path).
     - When `allowed_paths` is non-empty, accept only if **at least one** path
       matches `allowed_paths`.
   - If extracted paths are empty (e.g., a cover letter `[PATCH 0/N]` with
     neither an `allowed_subject_prefixes` tag nor a diffstat) and
     `allowed_paths` is non-empty: do not create a standalone cover-letter
     patchset (`metadata.is_patch_or_cover = false`). Because the raw cover
     letter is still stored in `messages`, if subsequent `[PATCH 1/N]` parts do
     match `e2fsprogs` paths, `Database::message_is_cover_letter_for` still
     links the series to its parent cover letter.
4. **Rejection behavior:** Setting `metadata.is_patch_or_cover = false` and
   `patch_opt = None` mirrors `ignored_emails`: the NNTP article receipt still
   settles (advancing the group high-water mark) and the message is recorded in
   `messages` for thread continuity, while zero `patchsets` or `patches` rows
   are created. Manual `sashiko-cli submit` (`api-submit`) and GitHub PR
   fetches (`git-fetch`) are unaffected.

---

## 4. Defect Taxonomy & Severity Calibration

`e2fsprogs` tools operate directly on block devices and untrusted or corrupted
filesystem images. Severity uses Sashiko's strict 4-tier scale (`Critical`,
`High`, `Medium`, `Low`):

| Severity | Meaning in `e2fsprogs` | Concrete Examples |
| :--- | :--- | :--- |
| **Critical** | Silent or unrecoverable on-disk filesystem corruption on valid filesystems, destructive `e2fsck`/`resize2fs`/`tune2fs` mis-repair that loses user data, or arbitrary code execution / out-of-bounds memory write triggered by a crafted filesystem image or `fuse2fs` mount | Writing wrong block/extent mappings, corrupting group descriptors/bitmaps/superblocks, broken metadata checksum updates leaving an unmountable filesystem, `libext2fs` heap buffer overflow when parsing crafted extent/xattr/dir blocks |
| **High** | Shared-library ABI breakage in `libext2fs`/`libe2p`/`libcom_err`/`libss`, incorrect filesystem state or feature-flag incompatibility with Linux `fs/ext4`, lost `e2undo`/journal rollback safety, 32-bit truncation of 64-bit block numbers (`blk64_t` -> `blk_t`), big-endian byte-swap bugs (`EXT2FS_BE`), or crashes during normal operation on valid filesystems | Modifying public struct layout in `ext2fs.h` without reserved padding, failing to mark bitmaps/superblock dirty before `ext2fs_flush`, `1 << s_log_block_size` UB or 32-bit overflow above 16 TiB, `fuse2fs` race corrupting state |
| **Medium** | Crash, assertion/abort, infinite loop, or unbounded allocation (DoS) when `e2fsck`/`debugfs`/`dumpe2fs`/`fuse2fs` inspects a corrupted or fuzzed filesystem image (without memory corruption or data loss), resource/memory leaks on error paths, partial-I/O error handling bugs, or cross-platform build/portability breakage | Division by zero on corrupted superblock `s_blocks_per_group == 0`, infinite cycle in corrupted extent/htree traversal without visited-block guard, leaking `ext2fs_get_mem` buffer on `goto errout`, `#ifdef` unused variable breaking `-Wall` builds |
| **Low** | Minor edge-case diagnostic inaccuracy, missing bounds check with benign fallback on impossible configurations, or concrete `Fixes:` / `Signed-off-by:` trailer defects when coupled with substantive review | Wrong block number printed in `e2fsck` problem context without affecting repair action, minor man-page / option mismatch where a user-visible flag behaves differently than documented |

Pure style nitpicks, subjective refactoring suggestions, and requests to add
tests without a concrete bug are suppressed.

---

## 5. Multi-Stage Review Workflow (`src/workflows/e2fsprogs_patch_review.rs`)

`e2fsprogs` uses the modern Sashiko map-reduce review pipeline with dynamic
stage planning, parallel discovery, consolidation, sharded verification,
prior-version feedback attribution, and LKML-style inline report generation:

```
planning
  ├── goal             (mandatory; commit log + series context + Fixes: check)
  ├── implementation   (mandatory; commit log + series context + ABI/cross-tool symmetry)
  ├── execution-flow   (mandatory; errcode_t propagation, bounds, iteration)
  ├── resources        (optional; ext2fs_get_mem/free_mem, dirty flags, io_channel/undo lifecycles)
  ├── ondisk-types     (optional; untrusted image fuzzing, 64-bit blk64_t, endianness, checksums, bigalloc)
  ├── concurrency-io   (optional; fuse2fs locking/security, MMP, mounted checks, signals, portability)
  └── prior-feedback   (automatic when prior version feedback exists)
        │
  consolidation        (short-circuits when zero findings and zero prior feedback)
        │
  verify-1..N          (sharded adversarial verification against actual repo code)
        │
  prior-attribution    (automatic when prior version feedback exists)
        │
  report               (inline-template.md -> plain-text LKML reply & GitHub PR comment)
```

### Stage Responsibilities
1. **`planning`:** Selects optional stages based on the diff (`resources`,
   `ondisk-types`, `concurrency-io`) while always scheduling `goal`,
   `implementation`, and `execution-flow`.
2. **`goal`:** Evaluates architectural soundness, compatibility with the Linux
   kernel `ext4`/`jbd2` on-disk specification (`EXT2_FEATURE_COMPAT_*`,
   `INCOMPAT_*`, `RO_COMPAT_*`), safety of automatic `e2fsck -p` preen repairs
   vs interactive prompts, DCO `Signed-off-by:`, and `Fixes:` trailer accuracy
   (using pre-analyzed `{{fixes_block}}` from `fixes_check.rs`).
3. **`implementation`:** Checks code vs commit message claims, shared-library
   public API/ABI backward compatibility (`libext2fs`, `libe2p`, `libcom_err`,
   `libss`), and horizontal consistency across utilities (`mke2fs`, `e2fsck`,
   `tune2fs`, `resize2fs`, `debugfs`, `dumpe2fs`, `fuse2fs`) and 32-bit vs
   64-bit APIs.
4. **`execution-flow`:** Audits `errcode_t` propagation, silent error
   swallowing, null dereferences, uninitialized output parameters, off-by-one
   bounds, and extent/directory/bitmap/xattr iteration termination (including
   cycle/depth guards on corrupted trees).
5. **`resources`:** Audits `ext2fs_get_mem` / `ext2fs_resize_mem` /
   `ext2fs_free_mem` lifetimes, `goto cleanup`/`errout` before pointer
   initialization, `ext2_inode_scan` / `ext2_extent_handle_t` / `io_channel`
   release, dirty-bit propagation (`ext2fs_mark_super_dirty`,
   `ext2fs_mark_bb_dirty`, `ext2fs_mark_ib_dirty`), and `e2undo`/journal
   cleanup.
6. **`ondisk-types`:** Audits arithmetic and type safety on untrusted on-disk
   structures: 32-bit vs 64-bit block/offset truncation (`blk_t` vs `blk64_t`,
   `e2_blkcnt_t`, `ext2_off64_t`), shift UB on `s_log_block_size` /
   `s_log_cluster_size`, division by zero on corrupted superblock fields,
   big-endian byte swapping (`ext2fs_le*_to_cpu` / `ext2fs_cpu_to_le*`),
   `metadata_csum` CRC32C updates on metadata mutation, and `bigalloc` block vs
   cluster unit confusion (`EXT2FS_B2C` / `EXT2FS_C2B`).
7. **`concurrency-io`:** Audits `fuse2fs` multi-threaded locking and mount
   privilege boundaries (`uid`/`gid`/permissions/suid stripping), `io_channel`
   cache coherency and partial I/O, MMP (`EXT4_FEATURE_INCOMPAT_MMP`) and
   mounted-filesystem checks, temporary/undo file safety (`O_EXCL`, `fsync`),
   and POSIX/BSD/macOS/Windows portability (`#ifdef` unused-variable hazards).

### Pre-existing Bug & `Fixes:` Integration
- `src/worker/fixes_check.rs`: Enables `analyze_fixes_for_commit` for
  `ProjectId::E2fsprogs`.
- `src/worker/bug_worker.rs` & `src/workflows/linux_bug.rs`: Wires
  `sashiko:e2fsprogs_bug` and `sashiko:e2fsprogs_bug:fix_check` with
  `e2fsprogs`-specific subsystem extraction (`lib/ext2fs`, `lib/e2p`, `e2fsck`,
  `misc`, `resize`, `debugfs`, `fuse2fs`, etc.) and mainline SHA resolution.

---

## 6. Three-Layer Prompt & Code Notes Architecture

Under `projects/e2fsprogs/prompts/`:

1. **Layer 1 — Core Review Procedures (`*.md`):**
   - `README.md`: Prompt bundle layout and maintenance rules.
   - `review-core.md`: Core `e2fsprogs` review methodology, evidence standards,
     on-disk untrusted-input discipline, and codebase exploration instructions.
   - `severity.md`: 4-tier severity calibration (`Critical`, `High`, `Medium`,
     `Low`) for `e2fsprogs`.
   - `false-positive-guide.md`: Concrete patterns that are *not* bugs in
     `e2fsprogs` (e.g., `ext2fs_free_mem(&ptr)` zeroing the pointer and safely
     accepting `NULL` in `ext2fs_free_mem`, `e2fsck` exit-status bitmask flags,
     intentional legacy 32-bit `libext2fs` entrypoints kept for ABI
     compatibility alongside `64` variants, process-exit cleanup in short-lived
     CLI utilities when not inside `libext2fs`).
   - `inline-template.md`: Plain-text LKML-style inline review format used for
     both `linux-ext4@vger.kernel.org` replies and `tytso/e2fsprogs` PR
     comments.
2. **Layer 2 — Scoped Maintainer Notes (`maintainers/*.md`):**
   - `general.md` (`paths = ["*"]`): Project-wide ABI stability policy, DCO and
     commit conventions, `-Wall` `#ifdef` cleanliness, and suppression rules.
   - `libext2fs.md` (`paths = ["lib/ext2fs/"]`): Shared-library symbol/struct
     ABI compatibility, untrusted filesystem image safety, endianness, and
     `metadata_csum` / `bigalloc` invariants.
   - `e2fsck.md` (`paths = ["e2fsck/"]`): Pass 1-5 invariants, `preen` (`-p`)
     non-destructive repair policy, corrupt-image loop/crash resilience, and
     exit-code bitmask semantics.
   - `tools.md` (`paths = ["misc/", "resize/", "debugfs/", "scrub/"]`): Mounted
     filesystem / MMP checks, `e2undo` / `io_channel` flush guarantees, and
     cross-tool feature symmetry (`mke2fs`, `tune2fs`, `resize2fs`, `debugfs`).
   - `fuse2fs.md` (`paths = ["fuse2fs/", "misc/fuse2fs.c"]`): Multi-threaded
     FUSE locking, unprivileged mount security boundaries, and error mapping
     (`errcode_t` -> `-errno`).
   - `support-libs.md` (`paths = ["lib/e2p/", "lib/et/", "lib/ss/", "lib/support/", "lib/blkid/", "lib/uuid/"]`):
     Legacy library ABI preservation (`libcom_err`, `libss`, `libe2p`) and
     internal quota/dict support invariants.
3. **Layer 3 — Code Notes Configuration (`notes/`):**
   - `notes/profile.toml`: Maps `e2fsprogs` directories into cohesive analysis
     units (`lib/ext2fs`, `e2fsck`, `misc`, `resize`, `debugfs`, `fuse2fs`,
     `lib/e2p`, `lib/support`, etc.) while skipping `po/`, `tests/`, `doc/`,
     `intl/`, and `debian/`, and activates `universal`, `lang/c`, and
     `domain/e2fsprogs` agendas.
   - `notes/agenda/universal.md`, `notes/agenda/lang/c.md`, and
     `notes/agenda/domain/e2fsprogs.md`: Version-agnostic source-verification
     questions for `sashiko --project e2fsprogs notes build`.

---

## 7. Standalone Production Deployment (`deployment/e2fsprogs.sashiko.dev/`)

Modeled on `deployment/sashiko.dev/` with GitHub Forge wiring from
`deployment/sashiko.sashiko.dev/`:
- **Namespace & Identity:** Dedicated `sashiko-e2fsprogs` namespace and
  `sashiko-e2fsprogs-ksa` Kubernetes ServiceAccount for GKE Workload Identity
  (Vertex AI + GCS backups).
- **Storage:** `sashiko-e2fsprogs-data-pvc` (`100Gi` `premium-rwo`, sized for
  the compact `e2fsprogs` git repository, worktrees, and SQLite database).
- **Application (`base/app/`):**
  - `sashiko-e2fsprogs-deployment` running `--track` on port `8080` with:
    - `SASHIKO__PROJECT__KIND=e2fsprogs`
    - `SASHIKO__PROJECT__DOMAIN=e2fsprogs.sashiko.dev`
    - `SASHIKO__SERVER__PUBLIC_BASE_URL=https://e2fsprogs.sashiko.dev`
    - `SASHIKO__GIT__REPOSITORY_PATH=/app/third_party/e2fsprogs`
    - `SASHIKO__REVIEW__EMAIL_POLICY_PATH=/app/projects/e2fsprogs/mailing_lists.toml`
    - `SASHIKO__NNTP__SERVER=nntp.lore.kernel.org`
    - `SASHIKO__FORGE__ENABLED=true`
    - `SASHIKO__FORGE__DISABLE_NNTP=false`
    - `SASHIKO__FORGE__PROVIDER=github`
    - `SASHIKO__FORGE__POST_MODE=live`
    - `SASHIKO__FORGE__REPOS=tytso/e2fsprogs`
    - `SASHIKO__LINUX_BUG__ENABLED=true`
    - `SASHIKO__LINUX_BUG__FIX_CHECK_ENABLED=true`
    - `SASHIKO__SERVER__ACL__ADMINS=tytso@mit.edu,roman.gushchin@linux.dev`
    - Secret-backed `SASHIKO__FORGE__GITHUB_TOKEN`,
      `SASHIKO__FORGE__WEBHOOK_SECRET`, `SASHIKO__SMTP__PASSWORD`, and
      `SASHIKO__SERVER__SESSION_SECRET` (`sashiko-e2fsprogs-secrets`).
  - `data-manager` pod (`sashiko-e2fsprogs-ksa`) and `sashiko-db-backup`
    nightly CronJob backing up SQLite to GCS.
- **Routing (`base/routing/`), `maintenance/`, and `production/`:**
  - `sashiko-e2fsprogs-service`, `sashiko-e2fsprogs-backendconfig`,
    `sashiko-e2fsprogs-cert` (`e2fsprogs.sashiko.dev`), dual-stack GCE Ingress
    (`sashiko-e2fsprogs-ip` / `sashiko-e2fsprogs-ipv6`), `maintenance` 503
    overlay, and `production/kustomization.yaml.example`.
