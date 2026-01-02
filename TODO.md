# Sashiko Implementation TODO

## Phase 1: Foundation
- [x] Initialize Rust workspace and project structure.
- [x] Set up `config` crate for environment/file-based configuration.
- [x] Implement NNTP Ingestor (polling `nntp.lore.kernel.org`).
- [x] Set up libSQL/Turso schema for mailing lists and patchsets.
- [x] Implement internal task queue (tokio channels).
- [x] Implement article state tracking (high-water mark).
- [x] Set up structured logging (`tracing`) and observability baseline.

## Phase 2: Git Ops & Patch Processing
- [x] Implement Patch/Patchset parsing from raw emails.
- [x] Develop baseline detection logic (explicit and heuristic).
- [x] Set up sandboxed `git am` environment.
- [x] Implement Worktree Garbage Collector (pruning & disk limits).
- [x] **Task**: Download `lore.kernel.org` git archive (e.g., 2025 emails) for offline testing.
- [x] **Task**: Implement file-based/git-based ingestor to process downloaded archives.
- [x] **Task**: Add configuration option to disable NNTP and use local archives.
- [x] Implement patchset assembly (handling multi-part messages).

## Phase 3: AI Logic & Interaction Tracking
- [ ] Implement model-agnostic AI provider abstraction.
- [ ] Set up `ai_interactions` table and workflow engine for chain restoration.
- [ ] Integrate with `review-prompts` repository logic.
- [ ] Implement consensus/comparison logic for multiple LLM runs.

## Phase 4: Web API & Frontend
- [ ] Build Axum REST API for patches, reviews, and stats.
- [ ] Implement RBAC (Role-Based Access Control) for API endpoints.
- [ ] Implement minimalistic "kernel.org style" frontend (Raw HTML/JS/CSS).
- [ ] Add UI for re-running reviews and manual baseline overrides.
- [ ] Configure Nginx as static file server and reverse proxy.

## Phase 5: Email Loop & Refinement
- [ ] Implement outbound SMTP for review replies.
- [ ] Implement inbound email processing (reply tracking).
- [ ] Add in-memory LRU cache for recent articles.
- [ ] Stress test system with 20k/day message volume simulation.
- [ ] Final performance tuning and security hardening.