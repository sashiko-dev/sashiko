# Design: Upstream Maintainer Grants for Bug and Patchset Access

## Status

Accepted. Addresses GitHub issue #787 ("Upstream maintainers patterns for bug
access").

## 1. Problem

A Linux bug is visible to the maintainers of the MAINTAINERS sections it is
attributed to, and to nobody else apart from operators, the security list and
the THE REST maintainer (`DESIGN_BUG_ACCESS_CONTROL.md`). Embargoed patch
review results and review transcripts follow the same rule through the
patchset's attributed sections (`DESIGN_PATCHSET_SUBSYSTEM_ACCESS_CONTROL.md`).
Both are attributed to every section whose `F:`/`N:` patterns match one of the
touched files (`MaintainersIndex::match_files`), not just the most specific
one.

That model has no notion of an upstream maintainer: a person who does not own
a subtree in MAINTAINERS but receives pull requests for it. The SoC group is
the motivating case. Platform maintainers (Qualcomm, Samsung, and so on) send
pull requests for `arch/*/boot/dts/<vendor>/`, `drivers/soc/<vendor>/` and
`drivers/firmware/<vendor>/` to the SoC group, not to Linus. The SoC group's
own section is narrow on purpose:

```
ARM AND ARM64 SoC SUB-ARCHITECTURES (COMMON PARTS)
M:	Arnd Bergmann <arnd@arndb.de>
M:	Krzysztof Kozlowski <krzk@kernel.org>
M:	Alexandre Belloni <alexandre.belloni@bootlin.com>
M:	Linus Walleij <linusw@kernel.org>
R:	Drew Fustini <fustini@kernel.org>
F:	Documentation/process/maintainer-soc*.rst
F:	arch/arm/boot/dts/Makefile
F:	arch/arm64/boot/dts/Makefile
```

No section claims `drivers/soc/` or `drivers/firmware/` as a whole, so a bug in
`drivers/soc/qcom/` is attributed only to `ARM/QUALCOMM SUPPORT`. As a result:

1. The SoC group cannot see bugs, or embargoed reviews, in the trees it merges.
2. When a platform maintainer is inactive, those bugs are visible to nobody who
   acts on them.

Adding `F:` lines to the SoC section upstream is not a fix: `F:` also drives
`scripts/get_maintainer.pl`, so every dts and SoC driver patch would start
copying the SoC group and its lists, which is exactly what the narrow section
avoids.

## 2. Goals

1. Let Sashiko grant the maintainers of an existing MAINTAINERS section the
   access they would have if the section listed extra paths: `Manage` on bugs
   and visibility of embargoed reviews and transcripts on patchsets in those
   paths, without changing MAINTAINERS.
2. Keep grants path-precise. The SoC group must see bugs in
   `drivers/soc/qcom/`, not bugs in `drivers/watchdog/gunyah_wdt.c` merely
   because `ARM/QUALCOMM SUPPORT` also lists that file.
3. Apply grants to existing bugs and patchsets, not only to new ones.
4. Make removing a grant revoke the access it gave.
5. Keep grants reviewable: they are policy that widens access to pre-disclosure
   material, so they live in a checked-in file changed through a pull request.

## 3. Non-Goals

* Mailing list routing, email recipients, contributor authority, the
  maintainer hint shown to the model, and the subsystem names displayed for a
  bug or patchset stay driven by MAINTAINERS alone. A grant changes who may see
  something, not who owns it.
* An email alias map. `krzk+dt@kernel.org` and `krzk@kernel.org` remain two
  principals, as `DESIGN_BUG_ACCESS_CONTROL.md` decided.
* New access levels. A grant gives exactly what maintaining the section gives.

## 4. Design

### 4.1 Grant file

`projects/linux/upstream_maintainers.toml`, found the same way as
`mailing_lists.toml` (`ProjectId::upstream_maintainers_path`). Only Linux has
one; a missing file means no grants.

```toml
# Maintainers of `section` receive pull requests for `paths` and get the same
# access to bugs and embargoed reviews there as if the section listed them.
# `section` must be an existing MAINTAINERS title; its M: and R: addresses are
# resolved from MAINTAINERS, so the file never copies addresses that could go
# stale.
[[grant]]
section = "ARM AND ARM64 SoC SUB-ARCHITECTURES (COMMON PARTS)"
paths = ["arch/*/boot/dts/", "drivers/soc/", "drivers/firmware/"]
```

`paths` use the MAINTAINERS `F:` syntax and are compiled with the existing
`CompiledPattern`, which treats a trailing slash as the whole subtree, also
when the directory part has a wildcard (`arch/*/boot/dts/`). The structs use
`#[serde(deny_unknown_fields)]`.

Validation, per grant, when the grants are attached to the loaded index:

* `section` names a section that exists in the index and is not a catch-all
  section. Otherwise the grant is dropped and an error is logged. Dropping
  rather than failing startup keeps the daemon up when upstream renames a
  section; the error makes the stale grant visible.
* `paths` is non-empty and every path is non-empty and not a catch-all
  spelling (`*`, `*/`, `**`). Otherwise the grant is dropped the same way.

A parse error in the file itself is logged and treated as no grants, which
fails closed.

A unit test parses the shipped file and checks every path, so a malformed
grant fails CI rather than being dropped in production.

### 4.2 Index and attribution

`MaintainersIndex` gains the validated grants and one query:

```rust
/// Grant sections covering any of `files`.
pub fn upstream_grant_sections<I, S>(&self, files: I) -> Vec<String>
```

`match_files` is unchanged, so every caller that is not about access keeps its
current behaviour by construction.

A new `SubsystemSource::UpstreamGrant` marks a section title that confers
access through a grant. It is `#[serde(skip_deserializing)]`, so no API payload
or stored JSON can claim it; only Rust code holding the index can produce it.
`MaintainersIndex::attribute_files(files)` returns the `MaintainersSection`
entries from `match_files` followed by `UpstreamGrant` entries from
`upstream_grant_sections`. The same title may appear with both sources.

Every place that attributes changed files for access uses it:

* bugs: the bug pipeline (`resolve_official_subsystems_for_project`), bugs
  filed through the API, and pre-existing bugs recorded during patch review;
* patchsets: `sections_for_diff`, which patch ingestion, the
  attribution backfill and the benchmark use.

### 4.3 Storage: separate derived tables

Grant rows are stored apart from MAINTAINERS rows, in two tables added by one
additive migration:

```sql
CREATE TABLE IF NOT EXISTS bug_upstream_sections (
    bug_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    PRIMARY KEY (bug_id, subsystem),
    FOREIGN KEY (bug_id) REFERENCES bugs(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS patchset_upstream_sections (
    patchset_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    PRIMARY KEY (patchset_id, subsystem),
    FOREIGN KEY (patchset_id) REFERENCES patchsets(id) ON DELETE CASCADE
);
```

with an index on `(subsystem, bug_id)` for the bug list filter.

The writers that store an attribution today (`create_bug`,
`replace_bug_subsystems`, `add_patchset_maintainer_sections`,
`replace_patchset_maintainer_sections`, and the patchset merge) route
`UpstreamGrant` entries to these tables and everything else to the existing
ones, in the same transaction. A writer that missed the routing would try to
store `upstream_grant` in `bug_subsystems` or `patchset_maintainer_sections`,
whose CHECK constraints reject it, so the mistake fails loudly instead of
granting or losing access silently.

Keeping grant rows separate:

* needs no change to the existing CHECK constraints, which would require
  rebuilding those tables and the bug audit triggers;
* lets a removed grant be revoked exactly, without touching MAINTAINERS rows;
* keeps the displayed subsystem names unchanged, since display reads only the
  existing tables.

### 4.4 Reconciliation of existing rows

Goal 3 and goal 4 need existing bugs and patchsets re-attributed when the
grants change. At daemon startup, after the patchset attribution backfill, a
reconciliation compares a fingerprint of the validated grants with the one
stored in the `meta` table:

* unchanged: nothing to do;
* no grants: delete every row in both tables, store the fingerprint;
* otherwise: walk bugs and patchsets in id order, one page per short
  transaction, recompute each item's grant sections, replace its rows in the
  upstream tables, then store the fingerprint.

The files come from what is stored with each item:

* bugs: `Bug::attributable_files()`, which returns the latest `verification`
  enrichment's `source_files` and falls back to the `candidate` enrichment's.
  There is no `source_files` column on `bugs`, and `Bug::source_files()` only
  reads verified files;
* patchsets: the stored diffs (`patch_diffs_only`), as the attribution
  backfill does.

Items without files get no grant rows; the reconciliation logs how many it
skipped. Like the backfill, it does nothing when the MAINTAINERS index is
empty, because every grant would then be invalid and the walk would revoke
everything until the next boot. It is idempotent: an interrupted walk leaves
the fingerprint unset and the next boot redoes it.

### 4.5 Authorization

The queries that read authorizing sections read
`... WHERE source = 'maintainers_section'` today. Each becomes a `UNION` with
the matching upstream table:

* `authorizing_sections_for_bugs` (per-bug and per-page access checks);
* the bug list visibility filter and the per-subsystem counts' visibility
  filter (the counts still group by the displayed MAINTAINERS sections);
* `authorizing_sections_for_patchsets` (embargo visibility and transcripts).

`Principal` does not change: the grant target is a real section title the
principal already holds through `subsystems_for_address`, so `access_to`,
`may_view_embargoed` and `BugVisibility::Sections` work unchanged and give
`Manage` and embargo visibility exactly as for the section's own paths.

## 5. Alternatives Considered

| Option | Why not |
| --- | --- |
| Add `F:` lines upstream | Changes `get_maintainer.pl` output for every dts and SoC patch. |
| Expand the principal's sections (SoC group also "maintains" every section with a pattern under the grant paths) | No schema change and retroactive, but section-granular: the SoC group would see every `ARM/QUALCOMM SUPPORT` bug, including watchdog and DT binding bugs, and every `OPEN FIRMWARE AND FLATTENED DEVICE TREE BINDINGS` bug. |
| Store grant rows in the existing tables as `maintainers_section` | No new tables, but grant rows are indistinguishable from MAINTAINERS rows, so removing a grant cannot revoke access without also risking MAINTAINERS attributions. |
| New `upstream_grant` value in the existing `source` columns | Needs table rebuilds to change the CHECK constraints and the audit triggers, for the same result as separate tables. |
| Grants in `Settings.toml` | Per-deployment and unreviewed; access to pre-disclosure material should be visible policy in the repository. |

## 6. Implementation Plan

1. Grant file parsing and validation, `UpstreamGrant` source,
   `MaintainersIndex::upstream_grant_sections` and `attribute_files`, tests.
2. Migration adding both tables, write routing, authorization unions, tests
   for listing, counts, per-bug access, embargo visibility and revocation.
3. Attribution call sites use `attribute_files`.
4. Startup reconciliation.
5. Load the grant file at startup, ship it with the SoC grant, and document it
   in `docs/configuration.md` and `projects/linux/MAINTAINERS_GUIDE.md`.
