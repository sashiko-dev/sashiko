# Sashiko Domain and Service Contracts

Invariants specific to an AI-driven code review daemon, CLI, and workflow engine, covering prompt-template trust boundaries, LLM session and tool sandboxing, daemon-worker IPC and outbox delivery, and persistence and feature-profile compatibility.

## PROMPT: Prompt Templates, Untrusted Input, and LLM Session Guardrails

- PROMPT-1: Where does untrusted patch, commit, mailing-list, or forge content enter prompt templates or tool outputs, and how is template directive expansion isolated from variable substitution?
- PROMPT-2: How are model-selected file paths, tool arguments, and JSON stage outputs validated, normalized, and bounded before affecting workflow state or filesystem access?
- PROMPT-3: How are turn budgets, validation retries, duplicate tool-call guards, token estimation limits, and response caching keys enforced across LLM sessions?

## WORKER: Daemon, Worker Subprocesses, Outbox Delivery, and Forge Sync

- WORKER-1: How do the server daemon and isolated worker subprocesses coordinate over stdio/IPC, shared git repositories, temporary worktrees, and per-run concurrency semaphores?
- WORKER-2: What state transitions, lease or lock rules, and idempotency guarantees govern patchset ingestion, review retries, outbox email routing, embargo policies, and forge webhook or comment delivery?

## STORE: Database Schema Evolution, Access Control, and Feature Profiles

- STORE-1: What schema migration invariants, transaction boundaries, parameter chunking limits, and read/write access filters govern persistent records and capability-scoped queries?
- STORE-2: Which interfaces, wire types, and modules are compiled conditionally across cargo feature profiles, and how is behavioral parity maintained between full-server and local-only builds?
