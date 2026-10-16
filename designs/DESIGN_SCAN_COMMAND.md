# Design: `sashiko scan` — Active Linux Kernel Codebase Scanner

## Status
Proposed

## 1. Overview & Motivation

Sashiko's existing review pipeline (`sashiko review` and the daemon `Reviewer`) is diff-driven: it inspects proposed patches and only uncovers pre-existing defects when they happen to lie in the immediate context of a modified hunk. Meanwhile, the vast majority of the Linux kernel codebase—device drivers, filesystems, networking protocols, IPC, and memory management subsystems—contains latent **High** and **Critical** bugs (use-after-free, double free, out-of-bounds memory access, race conditions, deadlocks, sleeping in atomic context, integer overflows, and reference count leaks) in code that has not been touched by recent patches.

This document specifies `sashiko scan`, a command that actively scans specified files and directories in the Linux kernel repository for serious (`High` and **Critical**) defects using a **two-phase map-reduce chunk discovery workflow** (`src/workflows/linux_scan.rs`) tightly integrated with Sashiko's **Linux Bug Pipeline** (`src/workflows/linux_bug.rs`).

### 1.1 Goals
1. **Path-Scoped Active Scanning**: Accept one or more file or directory paths (e.g., `sashiko scan fs/smb/server/` or `sashiko scan drivers/nvme/host/core.c`) and resolve all target source files (`.c`, `.h`, `.rs`) at a pinned Git revision (`--ref`, defaulting to `HEAD` or mainline top-of-trunk).
2. **Structure-Aware Chunking**: Partition large kernel source files into coherent, bounded function clusters using `tree-sitter` so the LLM never suffers attention dilution on 5,000+ line files while keeping related intra-file callers/callees (e.g., `alloc`/`free`, `probe`/`remove`, `lock`/`unlock`) together.
3. **Two-Phase Map-Reduce Chunk Discovery (`src/workflows/linux_scan.rs`)**:
   - **Phase 1 (Pre-screen & Exploration/Triage)**: Select subsystem/pattern guides and run an exploration pass over each chunk's function inventory and source code to identify high-risk entry points, state invariants, and relevant specialist stages (short-circuiting benign boilerplate chunks early).
   - **Phase 2 (Parallel Specialist Map + Verification Reduce)**: Fan out to specialist stages (`execution-flow`, `resources`, `locking`, `security`, `hardware`) with full Git `ToolBox` access, Negative Data Tracking (`dismissed_concerns`), and Anti-Charity directives; consolidate and verify in `verification` and parallel `post-verification` to emit high-confidence candidate [`BugInput`](file:///usr/local/google/home/kfree/sashiko/src/api.rs) records.
4. **First-Class Integration with the `linux_bug` Pipeline (`src/workflows/linux_bug.rs`)**:
   - Instead of duplicating normalization, origin commit tracing, or patch-review LKML email formatting inside `linux_scan.rs`, every candidate [`BugInput`](file:///usr/local/google/home/kfree/sashiko/src/api.rs) discovered across chunks is processed through the canonical `linux_bug` pipeline:
     - **Standalone CLI Mode (default)**: Runs `run_standalone_bug_pipeline_for_project` (`NormalizeSession` -> `VerifySession` -> `TracingSession` + `deterministic_blame_fallback` -> `SeveritySession`), filtering out false positives and `< High` issues and returning structured `ScannedBug` findings (`canonical_title`, `canonical_description`, `severity`, `severity_explanation`, `affected_source_files`, `locations`, `introduced_in_commit`).
     - **Bug Repository Submission Mode (`--submit-bugs`)**: Atomically inserts and claims each candidate bug (`create_claimed_issue_for_project` / `BugWorker::submit_and_process_bug`) and executes `process_issue_worker_for_project` (`NormalizeSession` -> `VerifySession` -> `DedupSession` under `BUG_DEDUP_LOCK` + cross-process `bug_dedup_lease` -> `TracingSession` -> `SeveritySession` -> `ReportSession` -> `db.update_bug_outcome`).
5. **Serious Issues Only (`High` / `Critical`)**: Strictly target reachable bugs with severe impact (memory corruption, UAF, double free, kernel panic/oops, deadlocks, data races with real impact, privilege escalation, information leaks, severe resource leaks), discarding stylistic, theoretical, or `Low`/`Medium` concerns.

---

## 2. CLI Interface

`sashiko scan` is added as a subcommand in [`Commands`](file:///usr/local/google/home/kfree/sashiko/src/main.rs) (`src/main.rs`):

```text
sashiko scan [OPTIONS] <PATHS>...
```

### 2.1 Arguments & Flags

| Flag / Argument | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `<PATHS>...` | `Vec<PathBuf>` | *(required)* | One or more repository-relative file or directory paths to scan (e.g., `fs/smb/server/`, `net/sched/sch_api.c`). |
| `--ref <REV>` | `Option<String>` | `HEAD` / mainline SHA | Git revision to scan. All file reads and `ToolBox` calls are pinned to this exact commit SHA. |
| `--repo <PATH>` | `Option<PathBuf>` | Current git worktree / `settings.git.repository_path` | Path to the target Git repository. |
| `--concurrency <N>` | `usize` | `4` | Maximum number of scan chunks evaluated concurrently. |
| `--max-files <N>` | `usize` | `100` | Safety cap on the number of source files expanded from `<PATHS>...` in one run (`0` = unlimited). |
| `--max-chunks <N>` | `Option<usize>` | `None` | Optional cap on the total number of chunks scanned. |
| `--exclude <GLOB>` | `Vec<String>` | `[]` | Additional glob patterns to exclude from target resolution. |
| `--submit-bugs` | `bool` | `false` | Submit discovered bugs into the configured database (`sashiko.db`) and run `BugWorker` deduplication and persistence. |
| `--prompt <TEXT>` | `Option<String>` | `None` | Extra instructions appended to the scan system prompt. |
| `--prompts <PATH>` | `Option<PathBuf>` | Project prompt bundle | Override prompt directory (`third_party/prompts/kernel` or `projects/sashiko/prompts`). |
| `--format <FMT>` | `OutputFormat` | `text` | Output format (`text` or `json`). |
| `--color <MODE>` | `ColorMode` | `auto` | Terminal color mode (`auto`, `always`, `never`). |

---

## 3. Architecture & Data Flow

```mermaid
flowchart TD
    CLI["sashiko scan <paths>..."] --> Resolve["1. Target Resolution (git ls-tree at target_sha)\nFilter .c / .h / .rs & exclude patterns"]
    Resolve --> Chunker["2. Tree-sitter Structural Chunker\nExtract FunctionSpan inventory + intra-file call graph\nGroup into bounded ScanChunks (~600 lines) + Header Prefetch"]

    Chunker --> Semaphore["Bounded Concurrency Pool (Semaphore)"]

    subgraph ScanWorkflow ["3. Two-Phase Chunk Discovery Workflow (src/workflows/linux_scan.rs)"]
        Semaphore --> S1["Stage 1: pre-screen\nSelect subsystem/*.md & patterns/*.md guides"]
        S1 --> S2["Stage 2: triage (Exploration Pass)\nIdentify high-risk entry points, invariants,\nfocus_targets & relevant specialist stages"]
        S2 --> CheckEmpty{"relevant_stages\nempty?"}
        CheckEmpty -- "Yes (benign/boilerplate)" --> SkipChunk["Early Exit Chunk"]
        CheckEmpty -- "No" --> S3["Stage 3: Parallel Specialist Stages (Map)\nexecution-flow | resources | locking | security | hardware\n(Full Git ToolBox @ target_sha)"]
        S3 --> CheckConcerns{"all_concerns &\nall_dismissed empty?"}
        CheckConcerns -- "Yes" --> SkipChunk
        CheckConcerns -- "No" --> S4["Stage 4: verification (Reduce)\nDeduplicate, filter < High severity,\nclassify findings / hard_cases / dismissed"]
        S4 --> S5["Stage 5: Parallel post-verification\nDeep cross-file tool verification of hard_cases"]
    end

    S5 --> Candidates{"Candidate BugInputs\ndiscovered?"}
    Candidates -- "Yes" --> ModeCheck{"--submit-bugs?"}
    ModeCheck -- "false (Standalone CLI)" --> StandaloneBug["4a. Standalone linux_bug Pipeline (linux_bug.rs)\nNormalizeSession -> VerifySession -> TracingSession -> SeveritySession"]
    ModeCheck -- "true (Database Persistence)" --> SubmitBug["4b. BugWorker::submit_and_process_bug (linux_bug.rs)\nAtomic Insert+Claim -> Normalize -> Verify -> Dedup -> Trace -> Severity -> Report"]
    StandaloneBug --> Output["5. Render Structured ScanSummary (text / json)"]
    SubmitBug --> Output
```

---

## 4. Detailed Component Design

### 4.1 Target File Resolution & Filtering (`src/scan.rs`)

1. **Revision Resolution**:
   - If `--ref` is provided, resolve it to a full 40-character commit SHA via `git rev-parse --verify --end-of-options <rev>^{commit}` using `git_cmd::in_dir_async`.
   - If `--ref` is omitted, resolve `HEAD` (falling back to `origin/master` or `origin/main` for bare repositories).
2. **Path Enumeration**:
   - Sanitize each user-supplied path via `sanitize_scan_path` (rejecting leading `-` or `..` traversal) and invoke:
     ```text
     git ls-tree -r -z --name-only <target_sha> -- <paths...>
     ```
   - Retain files ending in `.c`, `.h`, or `.rs`.
   - Exclude paths matching default noisy non-production patterns (`Documentation/*`, `samples/*`, `scripts/*`, `tools/testing/selftests/*`, `*.mod.c`) and user `--exclude` globs, unless the user explicitly targeted that file path.
   - Enforce `--max-files` (default `100`).

### 4.2 Tree-sitter Structural Indexing & Chunking (`src/scan.rs`)

`src/scan.rs` implements a deterministic structure-aware chunker built on `tree-sitter-c` (with a comment/literal-aware brace-depth scanner for `.rs`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FunctionSpan {
    pub name: String,
    pub signature: String,
    pub start_line: usize, // 1-based inclusive
    pub end_line: usize,   // 1-based inclusive
    pub called_functions: Vec<String>,
    pub risk_signals: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanChunk {
    pub file_path: String,
    pub chunk_index: usize,
    pub total_chunks: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub numbered_source: String,
    pub file_function_index: String,
    pub functions: Vec<FunctionSpan>,
    pub prefetched_context: String,
}
```

### 4.3 Two-Phase Chunk Discovery Workflow (`src/workflows/linux_scan.rs`)

The chunk discovery workflow is implemented as a declarative [`Workflow<LinuxScanState>`](file:///usr/local/google/home/kfree/sashiko/src/workflow/graph.rs) in `src/workflows/linux_scan.rs`:

1. **Phase 1, Stage 1: `pre-screen` (`ToolScope::None`, `max_turns: 1`)**:
   - Selects relevant `subsystem/*.md` and `patterns/*.md` guides from `subsystem/subsystem.md`.
2. **Phase 1, Stage 2: `triage` (`ToolScope::All`, `max_turns: 6`)**:
   - Audits the chunk's `numbered_source` and `chunk_functions_json` to emit `focus_targets` (`Vec<ScanFocusTarget>`) and `relevant_stages` (`Vec<String>`).
   - If `relevant_stages` is empty (benign definitions/boilerplate), Phase 2 is skipped.
3. **Phase 2, Stage 3: Parallel Specialist Audit Stages (`WorkflowStep::DynamicParallel`)**:
   - Runs the selected stages from `["execution-flow", "resources", "locking", "security", "hardware"]` with `ToolScope::All`, Anti-Charity directives, and Negative Data Tracking (`dismissed_concerns`).
4. **Phase 2, Stage 4 & Stage 5: `verification` and Parallel `post-verification`**:
   - Consolidates `all_concerns` and `all_dismissed_concerns`, filters out findings below `High` severity, and verifies contested or cross-file `hard_cases` using Git tools.
   - Confirmed findings are converted into standard [`BugInput`](file:///usr/local/google/home/kfree/sashiko/src/api.rs) payloads (`LinuxScanState::to_candidate_bug_inputs`).

### 4.4 Integration with `linux_bug` (`src/workflows/linux_bug.rs`, `src/worker/bug_worker.rs`, `src/scan.rs`)

1. **Intra-Scan Candidate Deduplication**:
   - Before invoking `linux_bug`, `run_scan` deduplicates candidate `BugInput`s across chunks by `(file, function_or_symbol, line_bucket)` and sparse-vector cosine similarity ($\ge 0.85$).
2. **Standalone `linux_bug` Pipeline (`run_standalone_bug_pipeline_for_project`)**:
   - When `--submit-bugs` is not enabled, `run_scan` runs each candidate `BugInput` through `run_standalone_bug_pipeline_for_project` in `src/workflows/linux_bug.rs`:
     1. `NormalizeSession`: normalizes the candidate into `canonical_title` (`<subsystem_prefix>: <defect in fn()>`, `< 80` chars, no fix-verbs), structured `canonical_description` (`Trigger / Preconditions`, `Call Chain`, `Failure Mechanism`, `Impact`), and `affected_source_files`.
     2. `VerifySession`: verifies reachability and call chains against the repository at `target_sha` using `ToolBox` and pre-fetched context, discarding false positives (`StandaloneBugOutcome::Discarded`).
     3. `TracingSession` (+ `deterministic_blame_fallback` + `format_commit`): traces `introduced_in_commit`.
     4. `SeveritySession`: calibrates `severity` and `severity_explanation` (filtering out `< High` findings).
3. **Database Submission Mode (`--submit-bugs`, `BugWorker::submit_and_process_bug`)**:
   - Validates upfront at the start of `run_scan` that `settings.database.url` is non-empty and opens/migrates `Database` before scanning any chunks.
   - For each candidate `BugInput`, `BugWorker::submit_and_process_bug` atomically inserts and claims the bug row in a single SQLite transaction (`Database::create_claimed_bug_with_enrichment` / `create_claimed_issue_for_project`) so no concurrent daemon `BugWorker` can steal the pending row, and runs `process_issue_worker_for_project` (serializing deduplication via `BUG_DEDUP_LOCK` and the cross-process `Database::acquire_bug_dedup_lease` in `meta`).

---

## 5. Implementation Plan

1. **Commit 1 — Design Document (`designs/DESIGN_SCAN_COMMAND.md`)**:
   - Specify the architecture, chunker, two-phase discovery workflow, and `linux_bug` pipeline integration.
2. **Commit 2 — Target File Resolution & Tree-sitter Chunker (`src/scan.rs`, `src/worker/prefetch.rs`, `src/lib.rs`)**:
   - Implement `resolve_scan_targets`, `sanitize_scan_path`, `extract_c_functions`, `extract_rust_functions`, `build_scan_chunks_for_file`, and header context prefetching.
3. **Commit 3 — Declarative Two-Phase Discovery Workflow (`src/workflows/linux_scan.rs`, `src/workflows/mod.rs`)**:
   - Implement `LinuxScanState`, `scan_prescreen_stage`, `scan_triage_stage`, specialist audit stages, `scan_verification_stage`, `scan_post_verification_stage`, and `to_candidate_bug_inputs`.
4. **Commit 4 — `linux_bug` Pipeline Integration & Scan Orchestrator (`src/db.rs`, `src/workflows/mod.rs`, `src/workflows/linux_bug.rs`, `src/worker/bug_worker.rs`, `src/scan.rs`)**:
   - Expose `linux_bug` unconditionally in `src/workflows/mod.rs` and add `run_standalone_bug_pipeline_for_project` in `src/workflows/linux_bug.rs`.
   - Add `Database::create_claimed_bug_with_enrichment`, `acquire_bug_dedup_lease`, `release_bug_dedup_lease`, and `BugWorker::submit_and_process_bug`.
   - Implement `run_scan` in `src/scan.rs`.
5. **Commit 5 — `sashiko scan` CLI Subcommand & Documentation (`src/main.rs`, `docs/sashiko-cli.md`, `GEMINI.md`)**:
   - Add `Commands::Scan`, `handle_scan_command`, `write_scan_result`, CLI tests, and documentation.
