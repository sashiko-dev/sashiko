# Design: Subsystem Selector and Maintainers Lifecycle

## Status
Proposed

## Context & Motivation
Sashiko tracks Linux kernel defects and normalizes them against Linux kernel subsystems identified via MAINTAINERS. Two key areas require systematic improvement:

1. Maintainers Lifecycle:
   Previously, MaintainersIndex was parsed on-demand from disk or temporary review worktrees across several worker workflows and API handlers. This resulted in redundant parsing of the ~30,000 line MAINTAINERS file, dependency on whatever branch a temporary review worktree happened to have checked out, and lack of an authoritative, immutable top-of-trunk source of truth.
   MAINTAINERS must be loaded once at startup directly from the top-of-trunk of Linus's tree (origin/master:MAINTAINERS), parsed correctly into memory, and kept immutable throughout normal execution.

2. Frontend Subsystem Selector & Scalability:
   The bug list view in the frontend previously exposed a free-form single-line text input for subsystem filtering. Users could not see which subsystems currently had open bugs, nor how many open bugs existed per subsystem.
   Furthermore, the database queried bugs by executing unindexed LIKE searches against JSON-serialized strings in bugs.subsystems. Filtering on multiple subsystems or scaling to tens of thousands of bugs resulted in full table scans.
   The frontend selector must display a checklist of actual subsystems with open bug counts (N), skipping (0) entries, selecting all non-zero entries by default, and querying bugs through a normalized, indexed relational structure that scales to massive datasets.

---

## Architectural Specification

### 1. Authoritative Top-of-Trunk MAINTAINERS Lifecycle

#### Loading Strategy
At startup of the sashiko daemon and CLI commands, MaintainersIndex::from_top_of_trunk reads MAINTAINERS using Git object extraction:
1. `git -C <repository_path> show origin/master:MAINTAINERS` (Linus Torvalds' mainline top-of-trunk).
2. Fallback to `master:MAINTAINERS` or `HEAD:MAINTAINERS` if origin/master is absent.
3. Fallback to reading MAINTAINERS directly from disk in repository_path.

#### Immutability & Concurrency
- The parsed index is stored in a process-wide `Arc<MaintainersIndex>` registered in GLOBAL_MAINTAINERS (OnceLock).
- Once initialized, it provides read-only pattern matching (match_file, match_files, match_mailing_lists).
- All worker threads, API handlers, and normalization sessions consume this shared index without disk I/O, Git subprocesses, or mutation.

---

### 2. High-Performance Subsystem Relational Model

#### Schema: bugs_subsystems
A normalized junction table tracks the mapping between bugs and subsystems:

```sql
CREATE TABLE IF NOT EXISTS bugs_subsystems (
    bug_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    PRIMARY KEY (bug_id, subsystem),
    FOREIGN KEY(bug_id) REFERENCES bugs(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_bugs_subsystems_subsystem ON bugs_subsystems(subsystem, bug_id);
CREATE INDEX IF NOT EXISTS idx_bugs_subsystems_bug_id ON bugs_subsystems(bug_id);
CREATE INDEX IF NOT EXISTS idx_bugs_status ON bugs(status);
CREATE INDEX IF NOT EXISTS idx_bugs_status_severity ON bugs(status, severity);
```

#### Lifecycle Synchronization
- Bug Creation (`create_bug`): Inserts associated subsystems into bugs_subsystems.
- Bug Update (`update_bug_outcome`): When subsystems are updated, atomically replaces entries in bugs_subsystems.
- Migration (`migrate`): Backfills bugs_subsystems from existing bugs.subsystems JSON arrays.

---

### 3. Scalable Bug Selection & Aggregation APIs

#### Subsystem Aggregation: `GET /api/bugs/subsystems`
Returns all subsystems with open bug counts, omitting zeroes:

```sql
SELECT bs.subsystem, COUNT(DISTINCT b.id) AS count
FROM bugs_subsystems bs
JOIN bugs b ON bs.bug_id = b.id
WHERE b.status = 'open'
GROUP BY bs.subsystem
HAVING count > 0
ORDER BY count DESC, bs.subsystem ASC;
```

Responses are cached in an AsyncCache with short TTL to handle high-concurrency dashboard browsing without load on the database.

#### Filtered Bug Query: `GET /api/bugs`
- Zero Selected: Returns empty list immediately in O(1) without touching the database.
- All Subsystems Selected (Default): Omits subsystem predicate, querying bugs WHERE status = 'open' using idx_bugs_status.
- Subset of Subsystems Selected: Queries via indexed subquery:
  `WHERE id IN (SELECT bug_id FROM bugs_subsystems WHERE subsystem IN (?, ?, ...))`
  Executing an index-only scan over idx_bugs_subsystems_subsystem.
- Legacy Hierarchical Prefix: Single subsystem queries (e.g. subsystem=drivers) match `subsystem = ? OR subsystem LIKE ?` (drivers/%), utilizing the B-tree index prefix.

---

### 4. Frontend Subsystem Selector UI

The free-form text input is replaced with a multi-select dropdown component:
- Button Header: Displays summary state (e.g. `Subsystems (All) ▼`, `Subsystems (3/15) ▼`, `Subsystems (None) ▼`).
- Dropdown Menu:
  - Search filter input to quickly narrow down subsystems by name.
  - Quick action controls: Select All and Clear.
  - Scrollable checklist of items: `[x] <Subsystem Name> (<count>)`.
- Zero-Bug Suppression: Entries with 0 open bugs are skipped.
- Default State: All non-zero entries are checked on initial load.
- State Management: Selections trigger debounced/instant updates to table data via `/api/bugs?subsystems=...`.
