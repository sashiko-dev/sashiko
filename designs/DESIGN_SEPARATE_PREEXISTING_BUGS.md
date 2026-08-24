# Design: Standalone Pre-existing Bug Pipeline, Verification, Vector Deduplication, and Reporting

## 1. Objective and Problem Statement

### 1.1 Context
Sashiko is an agentic code review system for the Linux kernel that analyzes patch submissions across multiple specialized review stages. During analysis, reviewers frequently uncover two distinct categories of issues:
1. **New Bugs / Regressions**: Defects directly introduced or triggered by the changes in the proposed patchset.
2. **Pre-existing Bugs**: Latent bugs or architectural vulnerabilities already present in the existing codebase prior to the patch being applied, which were observed in the surrounding context.

### 1.2 The Problem
Previously, pre-existing issues were bundled together with new issues in the same verification pass and mixed into a single inline LKML review comment block. This caused several problems:
- **Patch Author Friction**: Patch authors receiving review emails had to parse through comments about bugs they did not introduce, adding confusion and noise to the patch review discussion.
- **Lack of Tracking & Deduplication for Pre-existing Defects**: Because pre-existing bugs were tied ephemerally to individual patch reviews, the same pre-existing bug could be rediscovered across multiple independent patch submissions, repeatedly wasting review and verification effort.
- **Lack of Independent Processing**: Pre-existing issues require individual, localized investigation, separate verification criteria, and standalone inline reviews. Furthermore, pre-existing bug processing should not be locked into patch reviews alone—it needs to be a reusable, standalone pipeline usable via API calls or future standalone scans.

### 1.3 Key Architectural Goals
1. **Clean Separation in Patch Reviews**: Ensure patch inline reviews and main review bodies report *only* new regressions introduced by the patch under review.
2. **Standalone Pre-existing Issue Pipeline**: Build an independent pipeline (`PreexistingBugPipeline`) that processes candidate pre-existing bugs one by one. This pipeline can be invoked either during patch review or independently via API/CLI calls.
3. **Per-Issue Independent Verification**: Every pre-existing issue is verified and processed individually with High/Critical severity calibration.
4. **Subsystem & File-Aware Vector Deduplication**: Localize candidate search by file paths, directory hierarchy, and subsystem, then use fast vector comparison to fetch top $N$ candidates (e.g. $N=20$), followed by an LLM confirmation step.
5. **Dedicated Standalone Inline Reviews**: Generate an individual, complete inline review for each newly discovered pre-existing bug.
6. **Clear Reporting with Links**: Expose newly discovered pre-existing bugs on email reports and web pages as clickable links to their dedicated bug views (`/bug/<slug>`).
7. **Zero Regressions**: Maintain full compatibility with existing benchmark evaluation, CLI workflows, and database integrity.

---

## 2. Architecture Overview

```mermaid
flowchart TD
    subgraph PatchReview [Patch Review Workflow]
        S1_7[Stages 1-7: Analysis Stages] --> S8[Stage 8: Deduplication]
        S8 --> S9[Stage 9: Conflict Resolution]
        S9 --> Split{Split Concerns}
        Split -->|preexisting = false| S10[Stage 10: New Regressions Verification]
        S10 --> S11[Stage 11: Patch Inline Review]
    end

    Split -->|preexisting = true| ForEachBug[Iterate Candidate Pre-existing Concerns]
    ExternalAPI[External API / Standalone Scan Input] --> ForEachBug

    subgraph PreexistingPipeline [Standalone Pre-existing Bug Pipeline (Per Issue)]
        ForEachBug --> Verify[1. Issue Verification & Severity Calibration]
        Verify -->|Discard false positive / low sev| Drop[Drop / Ignore]
        Verify -->|Valid High/Critical| Localize[2. Subsystem & File-Aware Candidate Filter]
        Localize --> VecSearch[3. Vector Similarity Search: Top N (N=20)]
        VecSearch --> LLMDedup[4. LLM Deduplication Confirmation]
        LLMDedup -->|Duplicate of Bug #K| LinkExisting[Link to Existing Known Bug #K]
        LLMDedup -->|Newly Discovered| GenInline[5. Generate Standalone Bug Inline Review]
        GenInline --> StoreNewBug[6. Store in preexisting_bugs Database]
    end

    S11 --> Assemble[Assemble Patch Review Result]
    LinkExisting --> Assemble
    StoreNewBug --> Assemble

    Assemble --> Email[Email Outbox: Patch Review + Bug Links]
    Assemble --> WebUI[Web Page: Patchset View + Bug Cards & Links]
```

---

## 3. Detailed Component Design

### 3.1 Separation of Concerns in Patch Review

During patch review, after Stage 9 (Conflict Resolution), the retained concerns are divided:
1. `new_concerns`: `[c for c in concerns if not c.preexisting]`
2. `preexisting_concerns`: `[c for c in concerns if c.preexisting]`

- **Stage 10 (New Regressions Verification)** receives `new_concerns` only.
- **Stage 11 (Patch Inline Review Generation)** receives `findings` (new bugs only), commenting *strictly* on lines modified by the patch.
- Each item in `preexisting_concerns` is dispatched independently to the `PreexistingBugPipeline`.

### 3.2 Standalone Pre-existing Bug Pipeline (`PreexistingBugPipeline`)

The pipeline operates on a single candidate concern at a time:

```rust
pub struct PreexistingBugInput {
    pub problem: String,
    pub reasoning: String,
    pub locations: Vec<Location>,
    pub subsystem: Option<String>,
    pub source_files: Vec<String>,
    pub commit_sha: Option<String>,
    pub patchset_id: Option<i64>,
    pub patch_id: Option<i64>,
}
```

The pipeline executes discrete steps for each candidate, prioritizing early deduplication to eliminate redundant verification work:

#### Step 1: Subsystem & File-Aware Fast Vector Space Similarity Search ($N=20$)
Linux kernel bugs are highly localized to specific subsystems and files.
- Feature vectors are built from file path components, directory hierarchies, subsystem names, function symbols, and error keywords.
- Computes cosine similarity against all stored vector embeddings in the `preexisting_bugs` table and selects the **Top 20** candidate matches above the similarity threshold.

#### Step 2: LLM Deduplication Confirmation (Early Short-Circuit)
- If Top $N$ candidates are found:
  - Invokes an LLM prompt comparing candidate details against the Top $N$ known verified bugs.
  - If `is_duplicate == true`: The issue is immediately recognized as an existing bug, recording the review association and returning the existing bug details without running expensive tool-based verification.
  - If `is_duplicate == false` (or 0 candidates found): Proceeds to verification.

#### Step 3: Verification & Severity Calibration (For Novel Candidates)
- For new/unmatched candidate issues:
  - Invokes verification against the codebase using tools (`git_read_files`, `git_grep`, etc.) to confirm if the defect is genuine.
  - Filters out low/medium severity or non-reproducible concerns, accepting only **High** and **Critical** pre-existing issues.

#### Step 4: Standalone Inline Review Generation
- For confirmed High/Critical newly discovered bugs:
  - A dedicated prompt generates a self-contained LKML-style report quoting the problematic codebase lines (`> ...`) from the baseline tree, explaining the failure mechanism, and suggesting a fix.
  - The bug is saved in `preexisting_bugs` with a unique slug (e.g. `pb-a1b2c3d4`), locations JSON, standalone inline review, vector embedding, and discovery metadata.

---

## 4. Database Schema and Data Models

### 4.1 Schema Definition (`src/schema.sql`)

```sql
CREATE TABLE IF NOT EXISTS preexisting_bugs (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    problem TEXT NOT NULL,
    severity INTEGER NOT NULL, -- 1: Low, 2: Medium, 3: High, 4: Critical
    severity_explanation TEXT,
    locations TEXT,            -- JSON array of location objects
    subsystem TEXT,            -- Subsystem name (e.g. net, mm, fs)
    source_files TEXT,         -- JSON array of affected file paths
    inline_review TEXT,        -- Dedicated standalone inline review
    vector_json TEXT,          -- Serialized vector representation for matching
    discovered_in_patchset_id INTEGER,
    discovered_in_patch_id INTEGER,
    discovered_in_commit TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(discovered_in_patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(discovered_in_patch_id) REFERENCES patches(id)
);

CREATE INDEX IF NOT EXISTS idx_preexisting_bugs_slug ON preexisting_bugs(slug);
CREATE INDEX IF NOT EXISTS idx_preexisting_bugs_severity ON preexisting_bugs(severity);
CREATE INDEX IF NOT EXISTS idx_preexisting_bugs_subsystem ON preexisting_bugs(subsystem);

CREATE TABLE IF NOT EXISTS review_preexisting_bugs (
    review_id INTEGER NOT NULL,
    bug_id INTEGER NOT NULL,
    is_newly_discovered INTEGER NOT NULL DEFAULT 1, -- 1 = newly discovered, 0 = matched existing
    PRIMARY KEY(review_id, bug_id),
    FOREIGN KEY(review_id) REFERENCES reviews(id),
    FOREIGN KEY(bug_id) REFERENCES preexisting_bugs(id)
);

CREATE INDEX IF NOT EXISTS idx_review_preexisting_bugs_review ON review_preexisting_bugs(review_id);
```

### 4.2 Rust Data Structures (`src/db.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreexistingBug {
    pub id: i64,
    pub slug: String,
    pub problem: String,
    pub severity: Severity,
    pub severity_explanation: Option<String>,
    pub locations: Option<serde_json::Value>,
    pub subsystem: Option<String>,
    pub source_files: Option<serde_json::Value>,
    pub inline_review: String,
    pub vector_json: Option<String>,
    pub discovered_in_patchset_id: Option<i64>,
    pub discovered_in_patch_id: Option<i64>,
    pub discovered_in_commit: Option<String>,
    pub created_at: i64,
}
```

---

## 5. API Endpoints & Reporting

### 5.1 API Endpoints (`src/api.rs`)
- `GET /api/preexisting_bug?slug=<slug>`: Fetch full details, locations, and dedicated inline review.
- `POST /api/preexisting_bug/analyze`: Ingest and analyze a candidate pre-existing concern through `PreexistingBugPipeline`.
- `GET /api/patchset?id=<id>`: Includes linked `preexisting_bugs` (both newly discovered and matched existing).

### 5.2 Email Reporting (`src/reviewer.rs`)
- The main email body contains *only* new regressions inline comments.
- If newly discovered pre-existing bugs are associated with the review, a dedicated section is appended:
  ```text
  Newly Discovered Pre-existing Issues in Surrounding Codebase:
  - [Critical] Out-of-bounds read in eth_type_trans()
    View details & inline report: https://sashiko.dev/bug/pb-a1b2c3d4
  - [High] Missing mutex unlock on error path in proc_sys_call()
    View details & inline report: https://sashiko.dev/bug/pb-e5f6g7h8
  ```

### 5.3 Web UI (`static/index.html`)
- **Patchset Page**: New regression findings shown cleanly. A dedicated "Discovered Pre-existing Bugs" section lists badges and links to `/bug/<slug>`.
- **Pre-existing Bug View (`#/bug/:slug`)**: Displays metadata, affected files, subsystem, severity, and the standalone inline review.

---

## 6. Implementation Plan Step-by-Step

1. **Step 1: Database Migration & Models**: Add `preexisting_bugs` and `review_preexisting_bugs` tables, indexes, DB methods in `src/db.rs` and `src/schema.sql`.
2. **Step 2: Vector Space & Localization Engine**: Implement `src/ai/vector_search.rs` (term-frequency/sparse vector, path/subsystem weighting, cosine similarity, top-$N$ candidate retrieval).
3. **Step 3: Standalone Preexisting Bug Pipeline**: Implement `src/pipelines/preexisting.rs` with verification, vector search, LLM dedup, and standalone inline review generation.
4. **Step 4: Decouple Patch Review Workflow**: Update `src/worker/kernel_workflow.rs`, `src/worker/prompts.rs`, and `src/local_review.rs` to separate `new_concerns` and route pre-existing concerns through `PreexistingBugPipeline`.
5. **Step 5: Email & Web Reporting**: Update email builder in `src/reviewer.rs`, API endpoints in `src/api.rs`, and UI views in `static/index.html`.
6. **Step 6: Testing & Verification**: Write unit tests for vector search, pipeline integration, dedup logic, and run `make check-pr`.
