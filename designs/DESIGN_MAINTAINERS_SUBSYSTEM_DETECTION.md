# Design Document: 100% MAINTAINERS-Based Subsystem Detection

## 1. Overview & Objectives

Currently, subsystem detection in Sashiko relies partly on heuristic email regexes (configured in `Settings.toml` and email header matching) and rudimentary prefix checks.

This design transitions subsystem detection across the entire Sashiko architecture to be **100% based on the Linux kernel's `MAINTAINERS` file**, drawing direct architectural inspiration from the kernel's canonical `scripts/get_maintainer.pl` tool.

### Key Requirements
1. **Pure `MAINTAINERS` File Authority**:
   - Subsystem names, file ownership, mailing lists, and tree mappings are parsed directly from `MAINTAINERS`.
   - File patterns (`F:`), exclusions (`X:`), and regex patterns (`N:`) are fully supported with wildcard and directory matching.
2. **Multi-Subsystem Bug Support**:
   - A single bug or patch can touch multiple files across different subsystems, or a single file can belong to multiple subsystem tiers (e.g., driver-specific and general subsystem).
   - Pre-existing bugs record `subsystems: Vec<String>` (stored in `preexisting_bugs.subsystems` as a JSON array).
3. **Multi-Subsystem Vector Search & Deduplication**:
   - Localized vector embeddings incorporate all matched subsystems when indexing and computing candidate similarity.
4. **End-to-End Ingestion & UI Integration**:
   - Patchsets and messages associate with all matched subsystems in the database.
   - UI renders multiple subsystem tags for patchsets and pre-existing bugs.

---

## 2. Architecture & Pattern Matching Engine

### 2.1 Pattern Matching Rules (based on `scripts/get_maintainer.pl`)

In `MAINTAINERS`, sections are separated by blank lines and have the following format:
```text
SECTION TITLE
M: Maintainer Name <email@example.com>
R: Reviewer Name <email@example.com>
L: mailing-list@vger.kernel.org
S: Supported
T: git git://git.kernel.org/... branch
F: drivers/net/ethernet/intel/e1000/
F: drivers/net/ethernet/intel/e1000e/
X: drivers/net/ethernet/intel/e1000/e1000_osdep.h
N: [^a-z]e1000
```

#### Matching Logic:
1. **Section Exclusions (`X:`)**:
   - If any `X:` pattern matches the file path, the section is excluded for that file.
2. **Section Inclusions (`F:` & `N:`)**:
   - `dir/`: Trailing slash matches all files in and below `dir/`.
   - `dir/*`: Matches all files directly in `dir/`, but not subdirectories below.
   - `path/file.c`: Exact file path match.
   - `*glob*`: Wildcard glob matches (e.g., `*/net/*`).
   - `N:` regular expression matches against the file path.
3. **Specificity Ranking**:
   - Sections with more specific (deeper) path matches are ranked first.
4. **Union Matching for Multi-File Inputs**:
   - For a set of touched files $\{f_1, f_2, \dots, f_k\}$, the subsystem list is the union of all matched sections:
     $$\text{Subsystems}(P) = \bigcup_{f \in P} \text{MatchSection}(f)$$

---

## 3. Data Models and Schema Updates

### 3.1 Database Schema (`src/schema.sql`)

Update `preexisting_bugs` table to store multiple subsystems:
```sql
CREATE TABLE IF NOT EXISTS preexisting_bugs (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    problem TEXT NOT NULL,
    severity INTEGER NOT NULL,
    severity_explanation TEXT,
    locations TEXT,              -- JSON array of location objects
    subsystems TEXT,             -- JSON array of subsystem names: ["INTEL E1000 NETWORK DRIVER", "NETWORKING DRIVERS"]
    source_files TEXT,           -- JSON array of source file paths
    inline_review TEXT NOT NULL,
    logs TEXT,
    vector_json TEXT,
    discovered_in_patchset_id INTEGER,
    discovered_in_patch_id INTEGER,
    discovered_in_commit TEXT,
    introduced_in_commit TEXT,
    is_fixed INTEGER NOT NULL DEFAULT 0,
    fixed_in_commit TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(discovered_in_patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(discovered_in_patch_id) REFERENCES patches(id)
);
```

### 3.2 Rust Struct Updates

In `src/maintainers.rs`:
```rust
#[derive(Debug, Clone)]
pub struct MaintainerSection {
    pub name: String,
    pub files: Vec<String>,
    pub excludes: Vec<String>,
    pub regexes: Vec<regex::Regex>,
    pub mailing_lists: Vec<String>,
    pub trees: Vec<(String, Option<String>)>,
}

#[derive(Debug, Clone)]
pub struct MaintainersIndex {
    sections: Vec<MaintainerSection>,
}

impl MaintainersIndex {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self>;
    pub fn match_file(&self, file_path: &str) -> Vec<String>;
    pub fn match_files<I, S>(&self, file_paths: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>;
}
```

In `src/pipelines/preexisting.rs`:
```rust
pub struct PreexistingBugInput {
    pub problem: String,
    pub reasoning: String,
    pub locations: Option<Value>,
    pub subsystems: Vec<String>,
    pub source_files: Vec<String>,
    pub commit_sha: Option<String>,
    pub patchset_id: Option<i64>,
    pub patch_id: Option<i64>,
    pub baseline_sha: Option<String>,
}
```

In `src/db.rs`:
```rust
pub struct PreexistingBug {
    pub id: i64,
    pub slug: String,
    pub problem: String,
    pub severity: Severity,
    pub severity_explanation: Option<String>,
    pub locations: Option<Value>,
    pub subsystems: Vec<String>,
    pub source_files: Option<Vec<String>>,
    pub inline_review: String,
    pub logs: Option<String>,
    pub vector_json: Option<String>,
    pub discovered_in_patchset_id: Option<i64>,
    pub discovered_in_patch_id: Option<i64>,
    pub discovered_in_commit: Option<String>,
    pub introduced_in_commit: Option<String>,
    pub is_fixed: bool,
    pub fixed_in_commit: Option<String>,
    pub created_at: i64,
}
```

---

## 4. Vector Space Search with Multiple Subsystems

In `src/ai/vector_search.rs`:
- Update `extract_bug_vector` to accept `subsystems: &[String]`.
- For each subsystem $S \in \text{subsystems}$:
  - Normalize and add terms with localized subsystem weight ($w = 2.5$).
  - Tokenize individual words from subsystem names (e.g. "intel", "e1000", "network", "driver") to allow soft-matching between closely related sub-areas.

---

## 5. Implementation Steps

1. **Implement `src/maintainers.rs`**:
   - Complete parser for `MAINTAINERS` handling `F:`, `X:`, `N:`, `L:`, `T:`, and section headers.
   - Robust pattern matcher supporting directories, globs (`*`, `?`), exact files, and exclusions.
   - Comprehensive unit tests covering kernel path edge cases (e.g., `e1000`, `net/`, `fs/btrfs/`, `arch/x86/`).
2. **Update Database Layer (`src/db.rs` & `src/schema.sql`)**:
   - Update `preexisting_bugs` table definition and migration to support `subsystems TEXT`.
   - Update `create_preexisting_bug`, `get_preexisting_bug`, and parsing helpers to handle `subsystems: Vec<String>`.
3. **Update Vector Search (`src/ai/vector_search.rs`)**:
   - Update `extract_bug_vector` and candidate search to index multiple subsystems.
4. **Update Ingestion & Pipelines**:
   - In `src/main.rs`: Replace regex-based subsystem identification with `MaintainersIndex::match_files`.
   - In `src/reviewer.rs`: Extract touched files and match against `MaintainersIndex` to populate `subsystems` on candidate pre-existing bugs.
   - In `src/pipelines/preexisting.rs`: Pass `subsystems: &[String]` to dedup and vector search.
5. **Update Web UI (`static/index.html`)**:
   - Render multiple subsystem tags in bug detail views and patchset pre-existing bug cards.
6. **Testing & Verification**:
   - Run `make check-pr` to verify that all unit and integration tests pass.
