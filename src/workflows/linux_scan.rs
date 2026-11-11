// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Declarative two-phase workflow for active Linux kernel codebase scanning (`sashiko scan`).
//!
//! Deterministic subsystem `<maintainer_notes>` and snapshot `<code_notes>` are
//! injected per chunk ahead of execution. Phase 1 (`triage`) explores the target
//! source chunk, identifies high-risk functions and entry points (`focus_targets`),
//! and selects which specialist stages should run (early-exiting benign/boilerplate
//! chunks).
//!
//! Phase 2 runs parallel specialist stages (`implementation`, `execution-flow`,
//! `resources`, `locking`, `security`, `hardware`) with full Git tool access,
//! followed by `verification` and parallel `post-verification` so confirmed
//! actionable chunk findings (`Medium`, `High`, and `Critical`) can be converted
//! into candidate [`crate::api::BugInput`] payloads and fed into the
//! [`crate::workflows::linux_bug`] pipeline for ground-truth verification and
//! severity calibration.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::workflow::graph::Workflow;
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{ParallelPolicy, StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};
use crate::workflows::guard::normalize_stage_name;
use crate::workflows::linux_patch_review::{
    AnalysisStage, ConsolidationStage, MAX_POST_VERIFICATION_STAGES, POST_VERIFICATION_STAGE_NAMES,
    PostVerificationOutput, StageConcernsOutput, VerificationOutput,
    append_stage_dismissed_concerns_with_prompts, append_stage_items_with_prompts,
    batch_inconclusive_concerns_by_severity, collect_stage_prompts,
    enrich_post_verification_output, extra_prompt_paths_for_items, extract_item_stages_and_prompts,
    format_post_verification_feedback, format_verification_stage_feedback, push_unique_prompt,
    push_unique_string,
};

/// High-risk function or code range identified during Phase 1 triage.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ScanFocusTarget {
    pub function: String,
    #[serde(default)]
    pub lines: String,
    pub risk_category: String,
    pub hypothesis: String,
    #[serde(default)]
    pub callers_or_callees_to_check: Vec<String>,
}

/// Output of the Phase 1 `triage` stage.
#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct ScanTriageOutput {
    pub relevant_stages: Vec<String>,
    #[serde(default)]
    pub stage_efforts: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub focus_targets: Vec<ScanFocusTarget>,
}

/// Execution state for scanning a single source chunk in the Linux kernel.
#[derive(Clone, Debug, Default)]
pub struct LinuxScanState {
    pub chunk_id: String,
    pub target_sha: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub numbered_source: String,
    pub chunk_functions_json: String,
    pub file_function_index: String,
    pub intra_file_context: String,
    pub file_invariants: String,
    /// Normative architecture & contract specification produced by the isolated
    /// `ContractCartographer` agent (`Agent 1`), exposed ONLY to `contract-audit` (`Agent 2`).
    pub contract_notes: String,
    /// Pre-fetched 1-hop external call-sites and ops-table registrations for exported functions.
    pub external_callers: String,
    pub prefetched_context: String,
    pub maintainers_hint: Option<String>,
    /// Rendered `<maintainer_notes>` block for the system prompt, or empty.
    pub maintainer_notes: String,
    /// Rendered `<code_notes>` block for the system prompt, or empty.
    pub code_notes: String,
    /// Verbatim Linux kernel threat model (`Documentation/process/threat-model.rst`), when present.
    pub kernel_threat_model: Option<crate::threat_model::KernelThreatModel>,

    /// Optional manual stages filter (`--stages locking,security`).
    pub manual_stages: Option<Vec<String>>,
    /// Caller-supplied instructions appended to the shared system prompt.
    pub custom_prompt: Option<String>,
    /// Specialist stages selected by `triage` (or overridden by `manual_stages`).
    pub planned_stages: Vec<String>,
    /// Optional reasoning effort mode (`Mid`, `High`, or `Dynamic`).
    pub effort: Option<crate::ai::EffortMode>,
    /// Minimum severity floor requested for the scan (`Medium` when `None`).
    pub min_severity: Option<crate::db::SeverityFloor>,
    /// Per-stage reasoning efforts selected by `triage`.
    pub planned_efforts: std::collections::BTreeMap<String, crate::ai::ReasoningEffort>,
    /// High-risk functions and hypotheses identified by `triage`.
    pub focus_targets: Vec<ScanFocusTarget>,

    /// Aggregated raw concerns from specialist stages (`C1..Cn`).
    pub all_concerns: Vec<Value>,
    /// Aggregated raw dismissed concerns from specialist stages (`D1..Dm`).
    pub all_dismissed_concerns: Vec<Value>,

    /// Direct findings (`VF1..VFp`) emitted by `verification`.
    pub verification_findings: Vec<Value>,
    /// Direct dismissals (`VD1..VDq`) emitted by `verification`.
    pub verification_dismissed: Vec<Value>,
    /// Findings confirmed by parallel `post-verification` (`PVF1..PVFr`).
    pub post_verification_findings: Vec<Value>,
    /// Inconclusive concerns refuted by parallel `post-verification` (`PVD1..PVDs`).
    pub post_verification_dismissed: Vec<Value>,
    /// Deduplicated dismissed concerns across verification stages.
    pub deduplicated_dismissed_concerns: Vec<Value>,
    /// Contested or speculative candidates routed to `post-verification`.
    pub inconclusive_concerns: Vec<Value>,

    /// Verified actionable findings (`Medium`, `High`, and `Critical`) discovered in this chunk.
    pub findings: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Stage Definitions & Tables
// ---------------------------------------------------------------------------

const STAGE_SCAN_IMPLEMENTATION_INSTRUCTION: &str = r#"# Active Codebase Scan: Implementation, Semantics & Accounting Audit

You are auditing existing Linux kernel C or Rust source code in `<target_chunk_source>` for semantic correctness, API contract violations, accounting bugs, and broken subsystem invariants (`Medium`, `High`, or `Critical`).
1. Audit EVERY function listed in `<chunk_functions>` systematically from top to bottom (use `<triage_focus_targets>` and `<file_invariants>` as extra cross-function leads, never as a reason to skip other functions in the chunk). Do not stop after finding one or two issues—multiple independent bugs often exist in the same file or even in the same function.
2. Compare functions, callbacks, and allocation/cleanup paths against sibling helpers in `<intra_file_context>`, `<pre_fetched_context>`, and the subsystem:
   - Pointer-vs-pointee & size-argument bugs: check every `memset`, `memcpy`, `memcmp`, `kzalloc`, `kcalloc`, and `copy_to_user`/`copy_from_user` for `sizeof(ptr)` instead of `sizeof(*ptr)` or mismatched struct/header sizes that leave trailing bytes uninitialized or copy the wrong length.
   - Wrong subject/entity variable bugs: in functions operating on a target task, vCPU, device, port, queue, or map alongside `current` or a parent container, verify every helper call passes the intended target entity (e.g., `target` vs `current` in ptrace/regset/cred/signal paths, or the right vCPU/map/vport struct) rather than querying the caller's own state.
   - Mathematical, bitwise, & parameter-type narrowing bugs: 32-bit shift overflows (`1 << n` or `BIT(n)` into 64-bit masks/addresses instead of `BIT_ULL(n)`), passing signed struct fields or return values (`s8`/`s16`/`int`/`ssize_t` that can hold negative sentinel values like `-1` or `-127` or `-ERRNO`) into narrow unsigned `u8`/`u16`/`unsigned` helper parameters or variables (silently wrapping negative sentinels into large positive levels/counts), inverted bitmask clearing (`& mask` vs `& ~mask`), and `ALIGN`/`round_up` extending past validated capacity.
   - Resource/cgroup accounting & container contracts: allocations or XArrays/IDRs omitting `__GFP_ACCOUNT` or `XA_FLAGS_ACCOUNT` where sibling paths account memory; manual XArray clearing loops omitting `xa_destroy()`; helpers mutating struct fields, indices, or IDs before an error check without restoring sentinel state on failure."#;

const STAGE_SCAN_EXECUTION_FLOW_INSTRUCTION: &str = r#"# Active Codebase Scan: Execution Flow & Error Path Audit

You are a static analysis engine auditing EVERY function listed in `<chunk_functions>` from top to bottom for control-flow, loop, bounds, and error-unwind defects (`Medium`, `High`, or `Critical`). Treat `<triage_focus_targets>` as supplemental hints only—never skip the remaining functions in the chunk, and check every error path in each function even if you already found another bug in that function.
1. Trace control flow across every branch, switch statement, loop condition, and `goto` error-handling label in each function:
   - Chronological `goto` & early-return unwind bugs: verify that every `goto err_*` and early `return` unwinds ALL and ONLY the allocations, mappings, locks, counters, and registrations acquired above that exact line (not jumping to a label that skips freeing an intermediate allocation, and not jumping above an initialization to free/unmap uninitialized state).
   - Unchecked lookup/erase/insert return values: check callers of `xa_erase()`, `xa_store()`, `idr_remove()`, hash/tree lookups, and `copy_from_user()` where a `NULL` / `xa_err` / existing-entry collision return means the entry was absent or insertion failed, yet execution continues to decrement counters, unpreserve pages, or leak the newly allocated or displaced object.
   - Partial-init, retry & termination bugs: resources or global IDs allocated during `*_alloc`/`*_init` leaked when a later step fails because `*_free` guards cleanup on a flag set later; nested retry loops (`xas_nomem`) missing error checks and spinning forever.
   - Off-by-one (`<` vs `<=`), missing wrap-around resets, and reachable NULL / ERR_PTR dereferences—including struct pointers (`pgt`, `collection`, `private_data`) that can be `NULL` before initialization, after detach/teardown, or after a preceding loop/helper (`may_block` / `cond_resched*`) temporarily dropped the lock, especially where neighboring lines use a safe accessor helper instead of raw `ptr->field` dereferencing."#;

const STAGE_SCAN_RESOURCES_INSTRUCTION: &str = r#"# Active Codebase Scan: Resource Lifecycle & Asynchronous Teardown Audit

You are an expert in Linux kernel resource management auditing EVERY function listed in `<chunk_functions>` from top to bottom for Use-After-Free (UAF), double-free, reference-count, stale-recycled-object, leak, and teardown-ordering defects (`Medium`, `High`, or `Critical`). Treat `<triage_focus_targets>` as supplemental leads only—audit every allocation, reallocation, recycle pool, refcount, callback, and cleanup path in the chunk.
1. Perform a chronological resource-lifecycle sweep on each function:
   - Allocation, overwrite & error-path leaks: check every `kmalloc`/`kzalloc`/`kvzalloc`/`kvmalloc`/`alloc_pages` and container insertion; verify that overwriting/replacing an entry or failing mid-loop frees both displaced and newly allocated objects, and check `krealloc`/`kvrealloc` callers for concurrent reader UAF on the old buffer or leak on failure.
   - Stale state on object reuse / custom freelists / per-CPU pools: when elements, descriptors, or chunks are recycled from a freelist, per-CPU cache, or pre-allocated pool rather than freshly `kzalloc`'d, verify that all flags, refcounts, embedded timers/work/locks, and pointers are reinitialized and torn down in valid order before reuse or free.
   - Asymmetric preserve/unpreserve, register/unregister, map/unmap, save/restore, and open/stop pairs across the file (cross-checking `<intra_file_context>` and `<file_invariants>`).
2. Scrutinize multi-phase framework callbacks and asynchronous work teardown:
   - Multi-phase teardown ordering (e.g., `css_offline` -> `css_released` -> `css_free_rwork_fn` -> `css_free`, or `unregister`/`del` vs `release`): verify whether core framework callers tear down `rstat`/`percpu_ref`/queues *before* the final release callback cancels async work.
   - Chained async work (`irq_work`, timer, or RCU callback queueing a `work_struct`): cancelling stage B (`cancel_work_sync`) requires synchronizing stage A (`irq_work_sync` / `timer_shutdown_sync`) *first*."#;

const STAGE_SCAN_LOCKING_INSTRUCTION: &str = r#"# Active Codebase Scan: Locking, Concurrency & Synchronization Audit

You are a Linux kernel concurrency and locking specialist auditing EVERY function listed in `<chunk_functions>` from top to bottom for synchronization, cross-lock race, atomic-context, and deadlock defects (`Medium`, `High`, or `Critical`). Treat `<triage_focus_targets>` as supplemental leads only—inspect all shared-state accesses, callbacks, and lock scopes in the chunk.
1. Check every function against `<file_invariants>` and `<intra_file_context>` for:
   - Cross-lock, lockless-reader, & check-then-use double-read TOCTOU races: writer mutating, unlinking, resetting (`field = -1` / `NULL`), or `kvrealloc`-resizing a shared struct field/array/list under Lock A (or locklessly on another CPU) while another function reads, indexes, or performs a non-`READ_ONCE` check-then-use (`if (p->cpu != -1) use(p->cpu)` or `if (p->ptr) use(p->ptr)`) without common serialization, allowing the second load to observe `-1`/`NULL`!
   - Execution-context, NMI/IRQ, & migration rules: sleeping calls (`mutex_lock`, `GFP_KERNEL`, `synchronize_rcu`, `cancel_work_sync`, `kvfree`/`vfree` where unsafe, user-copy) inside spinlock, rwlock, `rcu_read_lock()`, IRQ-disabled, or NMI context; non-NMI-safe locking or re-entrant deadlock in NMI/BPF/IRQ callbacks; per-CPU or BPF map operations missing required `migrate_disable()` or `rcu_read_lock()` protection.
   - Lock-drop & teardown pointer invalidation: holding locks or `kernfs` active protection across unbounded user-controlled work, lock-ordering inversions (AB-BA deadlocks), missing unlocks on error paths, and dereferencing a secondary pointer (`pgt`, `collection`, `cl`, etc.) after a loop/callee taking `may_block`/`gfp_t` (`cond_resched_lock` / `cond_resched_rwlock_write`) or during teardown where that pointer may already be `NULL` while sibling lines avoid dereferencing it.
2. Verify lock hierarchies and caller lock contexts in `<intra_file_context>` first, and use `git_grep` / bounded `git_read_files` when callers live in other files."#;

const STAGE_SCAN_SECURITY_INSTRUCTION: &str = r#"# Active Codebase Scan: Security & Trust Boundary Audit

You are a Linux kernel security researcher auditing EVERY function listed in `<chunk_functions>` for memory-safety, boundary, infoleak, integer-overflow, resource-exhaustion, and isolation defects (`Medium`, `High`, or `Critical`).
1. Scrutinize every trust boundary defined by the Linux kernel threat model (`Documentation/process/threat-model.rst`, provided below under `<kernel_threat_model>` when available):
   - Unprivileged local users, unprivileged user namespaces, ptrace/regset callers, BPF programs, and delegated cgroup hierarchies: check for uninitialized kernel stack/heap memory copied to userspace (`copy_to_user` / regset / netlink after partial `memset` or struct padding), wrong task/cred target checks, bounded global ID/resource exhaustion, missing `capable()`/`ns_capable()` checks, and privilege escalation.
   - Remote network peers & packet/sched classifiers: packet/header length vs buffer/fragment bounds, integer overflows in allocation sizes, class/qdisc state corruption, and bitmask validation bypasses.
   - Virtualization guest-to-host / nested hypervisor & MM permission boundaries (`kvm`, `vgic`, `vhost`, `virtio`, `mm`): guest-triggerable host NULL/UAF/OOB dereferences, unchecked guest register/ITS/MMU indices or addresses, missing architectural feature checks allowing guests to trap or corrupt host EL2/hypervisor state, IOMMU/DMA boundaries, and multi-layer fault/GUP permission mismatches (where a read fault calls `*faultin_pfn`/`get_user_pages` without `FOLL_WRITE` on a read-only memslot/VMA—yielding `writable == false`—yet downstream code marks the page dirty or installs a writable mapping using only the guest/upper-layer PTE write flag without intersecting `&& writable`).
2. For every concern you raise, state in `reasoning`: (1) the concrete actor and entry point, (2) required capabilities/configuration, and (3) whether it crosses a guaranteed kernel security boundary.
3. CRITICAL NON-DISMISSAL RULE: Never dismiss or omit a genuine memory-safety, bounds, race, leak, or accounting bug merely because the threat model classifies it as a regular bug rather than a security vulnerability (e.g., on a `CAP_SYS_ADMIN`, `debugfs`, or mounted-image path). Report every real bug in `concerns` with accurate privilege details so downstream severity calibration scores it properly."#;

const STAGE_SCAN_HARDWARE_INSTRUCTION: &str = r#"# Active Codebase Scan: Hardware, Architecture & Driver State Machine Audit

You are a CPU architecture, virtualization, and device-driver engineer auditing `<target_chunk_source>` for serious (`Medium`, `High`, or `Critical`) hardware, system-register, hypervisor, and driver state-machine bugs.
1. If the target chunk is purely generic high-level software logic with no CPU architecture, virtualization/hypervisor, interrupt-controller, DMA, or device-driver interaction, return `{"concerns": [], "dismissed_concerns": []}`.
2. Otherwise, audit EVERY function in `<chunk_functions>` for:
   - CPU architecture & hypervisor state (`arch/*`, `kvm`, `vgic`, `ptrace`, `iommu`): missing CPU feature / ID-register capability checks before accessing optional system registers or vector state (SVE/SME/NV/GIC), wrong trap routing or RES0/RES1/writable bitmask calculations, missing Stage-2/TLB/ICache invalidation, or corrupting host/guest saved register state across context switches.
   - MMIO, power & IRQ ordering: accessing MMIO registers or hardware queues before clocks/power domains/runtime PM are enabled or after disable; freeing/stopping queues or vports while NAPI, IRQ handlers, or adminq work remain active.
   - Register bitmask/shift & DMA bugs: wrong shift offsets, 32-bit `1 << n` overflow on 64-bit registers, overlapping bitmasks, DMA-mapping stack memory, wrong `dma_data_direction`, or missing `dma_mapping_error()` checks.
   - Open/stop, init/deinit, reset, or suspend/resume asymmetry that leaves IRQs enabled, state flags desynchronized, or hardware rings/resources leaked or double-freed."#;

const STAGE_SCAN_CONTRACT_AUDIT_INSTRUCTION: &str = r#"# Active Codebase Scan: Differential Contract-Breakage Audit (Agent 2)

You are an independent Contract-Breakage Auditor. An isolated Architecture Cartographer (Agent 1, who was forbidden from bug hunting and whose reasoning you cannot see) studied the full subsystem source, header definitions, and external kernel callers, and wrote `<architecture_contract_notes>` documenting how every data structure, lifecycle, lock, pointer/sentinel, and caller/callee interface in this file is SUPPOSED to work.

Your sole task is systematic differential verification: check EVERY function in `<chunk_functions>` from first to last—paying equal attention to short teardown, flush, unmap, and notifier helpers at the bottom of the chunk—against `<architecture_contract_notes>` and `<external_callers>` to find every place where the actual C/Rust implementation BREAKS a documented contract (`Medium`, `High`, or `Critical`):
1. **Object Creation, Teardown & Optional-vs-Base State Breakages:**
   - Teardown, flush, stop, or release hooks (`*flush*`, `*free*`, `*uninit*`, `*unpreserve*`, `*release*`, `*stop*`) invoked unconditionally by external callers (`<external_callers>`) that return early on an optional/lazy sub-feature condition (`if (!optional_count) return;`) and skip tearing down unconditionally-initialized base state at the bottom of the function!
   - Error-unwind `goto` labels, early returns, or retry paths that leak allocations, pages, refcounts, workqueues, fds, or global IDs, or call teardown helpers on zeroed/partially-initialized array slots; recycled/reused objects zeroing persistent embedded timers/locks or leaving per-CPU/status flags desynchronized.
2. **Synchronization, Lock-Drop & Check-Then-Use TOCTOU Breakages:**
   - Functions mutating, reallocating (`kvrealloc`/`krealloc`), unlinking, or resetting (`field = -1` / `NULL`) shared fields/arrays outside the lock documented for readers (`config_lock` vs `mmu_lock`, or lockless updates racing with non-`READ_ONCE` `if (p->field != -1) use(p->field)` double reads).
   - Sleeping/blocking inside atomic/RCU/NMI/IRQ contexts, or missing `migrate_disable()`/`rcu_read_lock()` required by per-CPU/BPF data structures.
3. **Nullability Window, Lock-Drop Invalidation & Sentinel/Type Domain Breakages:**
   - Dereferencing a struct pointer (`pgt`, `vncr_tlb`, `collection`, `cl`, etc.) in a path where `<architecture_contract_notes>`, `<external_callers>`, or sibling helpers show that pointer can be `NULL` (before lazy first-run init, after detach/free, during teardown, or after a preceding `may_block`/`cond_resched*` loop dropped and reacquired the lock—especially when adjacent lines use a safe non-dereferencing helper!).
   - Passing signed fields/returns (`s8`/`int` capable of negative sentinels like `-1` or `S1_MMU_DISABLED`) into narrow unsigned (`u8`/`u16`/`u32`) parameters, or unchecked `0`/overflow return values used in `~(size - 1)` range masks or bit shifts.
4. **Caller/Callee Interface, Permission Dataflow & Sibling Symmetry Breakages:**
   - Ignoring out-parameters (`writable` from `*faultin_pfn`/`get_user_pages` on read faults over read-only mappings when setting dirty/writable page protections; `xa_erase`/`cmpxchg` return values showing concurrent removal), operating on `current` instead of `target`, `sizeof(ptr)` instead of `sizeof(*ptr)`, or omitting a filter/check enforced by sibling handlers (`vcpu1`, `btf_type_is_void`, `cl_is_active`).

Verify any external uncertainty using bounded `git_read_files` / `git_grep` at `Target Revision SHA` before dismissing a suspected contract breakage."#;

pub static SCAN_CONTRACT_AUDIT_STAGE: AnalysisStage = AnalysisStage {
    name: "contract-audit",
    short: "Contract Audit",
    instruction: STAGE_SCAN_CONTRACT_AUDIT_INSTRUCTION,
    guides: &[],
    uses_commit_log: false,
    optional: false,
    wants_series_context: false,
};

const SCAN_STAGE_JSON_SCHEMA_INSTRUCTIONS: &str = r#"
TodoWrite compatibility: vendored prompts may ask you to add tasks or suspected bugs to TodoWrite. Do not call or mention TodoWrite. Treat those instructions as an internal checklist only. If that checklist identifies a concrete suspected bug, carry it forward as a JSON concern with file, function_or_symbol, line when known, triggering condition, and evidence. Do not output generic checklist progress as a concern.

Once you have gathered sufficient information, return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
If you find no Medium, High, or Critical bugs and no dismissed concerns, return `{"concerns": [], "dismissed_concerns": []}`.
- Set `"preexisting": true` on all concerns discovered during codebase scanning.
- Report distinct defects as separate entries in `'concerns'` (even if two bugs occur in the same function).
- Each object in `'concerns'` MUST use: `"type"`, `"description"`, `"reasoning"`, `"preexisting"` (`true`), and `"locations"` (non-empty array of `{"file", "function_or_symbol", "line", "code_snippet", "why_this_location_matters"}`).
- Each object in `'dismissed_concerns'` MUST use: `"type"`, `"description"`, `"reasoning"`, and `"locations"` (quoting the verbatim disproving guard, lock, cleanup path, or caller check in the repository).
- NO DISMISSAL WITHOUT VERIFIED PROOF: Never dismiss a candidate bug by assuming callers, framework teardown callbacks (such as `->css_free` or `->release`), or surrounding code handle it without inspecting ALL relevant callers and framework entry points and quoting the disproving code in `locations`. If you cannot prove safety from concrete code, report it in `'concerns'`."#;

pub static SCAN_ANALYSIS_STAGES: &[AnalysisStage] = &[
    AnalysisStage {
        name: "implementation",
        short: "Implementation",
        instruction: STAGE_SCAN_IMPLEMENTATION_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "execution-flow",
        short: "Execution Flow",
        instruction: STAGE_SCAN_EXECUTION_FLOW_INSTRUCTION,
        guides: &["callstack.md"],
        uses_commit_log: false,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "resources",
        short: "Resource Mgmt",
        instruction: STAGE_SCAN_RESOURCES_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "locking",
        short: "Locking & Sync",
        instruction: STAGE_SCAN_LOCKING_INSTRUCTION,
        guides: &["locking.md"],
        uses_commit_log: false,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "security",
        short: "Security Audit",
        instruction: STAGE_SCAN_SECURITY_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "hardware",
        short: "Hardware Review",
        instruction: STAGE_SCAN_HARDWARE_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
];

pub static SCAN_VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "verification",
    short: "Verification",
    wants_series_context: false,
};

pub static SCAN_POST_VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "post-verification",
    short: "Post-Verification",
    wants_series_context: false,
};

pub static SCAN_CONSOLIDATION_STAGES: &[&ConsolidationStage] =
    &[&SCAN_VERIFICATION, &SCAN_POST_VERIFICATION];

pub fn scan_analysis_stage_by_name(name: &str) -> Option<&'static AnalysisStage> {
    let normalized = normalize_stage_name(name);
    if normalized == SCAN_CONTRACT_AUDIT_STAGE.name {
        return Some(&SCAN_CONTRACT_AUDIT_STAGE);
    }
    SCAN_ANALYSIS_STAGES.iter().find(|s| s.name == normalized)
}

pub fn scan_consolidation_stage_by_name(name: &str) -> Option<&'static ConsolidationStage> {
    let normalized = normalize_stage_name(name);
    if POST_VERIFICATION_STAGE_NAMES.contains(&normalized.as_str()) {
        return Some(&SCAN_POST_VERIFICATION);
    }
    SCAN_CONSOLIDATION_STAGES
        .iter()
        .copied()
        .find(|s| s.name == normalized)
}

pub fn scan_stage_short_label(name: &str) -> Option<&'static str> {
    if let Some(def) = scan_analysis_stage_by_name(name) {
        return Some(def.short);
    }
    if let Some(cons) = scan_consolidation_stage_by_name(name) {
        return Some(cons.short);
    }
    match normalize_stage_name(name).as_str() {
        "triage" => Some("Chunk Triage"),
        "file-synthesis" => Some("File Synthesis"),
        "file-cartographer" => Some("Contract Cartographer"),
        "file-dedup" => Some("Candidate Dedup"),
        _ => None,
    }
}

pub fn is_known_scan_stage(name: &str) -> bool {
    scan_stage_short_label(name).is_some()
}

// ---------------------------------------------------------------------------
// System Prompt Template
// ---------------------------------------------------------------------------

pub fn linux_scan_system_prompt() -> PromptTemplate<LinuxScanState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();

    PromptTemplate::<LinuxScanState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.

You are an expert Linux kernel maintainer and static security auditor performing an active deep scan of the Linux kernel source tree to uncover real (`Medium`, `High`, and `Critical`) defects.

TOOL USAGE & EFFICIENCY: First check `<target_chunk_source>`, `<intra_file_context>`, `<file_invariants>`, and `<pre_fetched_context>` already provided below—do NOT re-read code lines already shown in your prompt. When you need external callers or definitions using tools (`git_read_files`, `git_grep`, `git_blame`, `git_log`, `git_show`), batch independent tool calls into a single turn, pass explicit `start_line` and `end_line` bounds (`<= 200` lines) to `git_read_files` instead of reading entire multi-thousand-line files, and always pass `Target Revision SHA` (`{{{{target_sha}}}}`).

<global_review_guidelines>
Any accompanying <maintainer_notes> blocks contain subsystem-specific priorities, architectural rules, and maintainer expectations that you MUST adhere to during your audit. Any accompanying <code_notes> blocks provide reference background notes generated from a fixed kernel snapshot to help you understand surrounding subsystem contracts, callers, and known pitfalls. However, <code_notes> and <file_invariants> are NOT substitutes for the live code under audit: always verify against verbatim code at Target Revision SHA before dismissing a suspected defect.
</global_review_guidelines>{{{{maintainer_notes}}}}{{{{code_notes}}}}

=== Active Scan Target Metadata ===
Target Revision SHA: {{{{target_sha}}}}
Chunk ID: {{{{chunk_id}}}}
Target File: {{{{file_path}}}} (lines {{{{start_line}}}}..={{{{end_line}}}}){{{{maintainers_block}}}}
===================================

<file_function_index>
Outline of all functions in {{{{file_path}}}}:
{{{{file_function_index}}}}
</file_function_index>{{{{file_invariants_block}}}}

<chunk_functions>
Functions defined in this chunk (lines {{{{start_line}}}}..={{{{end_line}}}}):
{{{{chunk_functions_json}}}}
</chunk_functions>

<target_chunk_source>
{{{{numbered_source}}}}
</target_chunk_source>{{{{intra_file_context_block}}}}{{{{prefetched_block}}}}{{{{focus_targets_block}}}}{{{{custom_prompt_block}}}}"#
    ))
    .with_var("target_sha", |s: &LinuxScanState| s.target_sha.clone())
    .with_var("chunk_id", |s: &LinuxScanState| s.chunk_id.clone())
    .with_var("file_path", |s: &LinuxScanState| s.file_path.clone())
    .with_var("start_line", |s: &LinuxScanState| s.start_line.to_string())
    .with_var("end_line", |s: &LinuxScanState| s.end_line.to_string())
    .with_var("maintainer_notes", |s: &LinuxScanState| {
        s.maintainer_notes.clone()
    })
    .with_var("code_notes", |s: &LinuxScanState| s.code_notes.clone())
    .with_var("file_function_index", |s: &LinuxScanState| {
        s.file_function_index.clone()
    })
    .with_var("file_invariants_block", |s: &LinuxScanState| {
        if s.file_invariants.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<file_invariants>\nFile-wide locking, nullable-state, execution-context, and lifecycle contracts synthesized across `{}`:\n{}\n</file_invariants>",
                s.file_path,
                s.file_invariants.trim()
            )
        }
    })
    .with_var("chunk_functions_json", |s: &LinuxScanState| {
        s.chunk_functions_json.clone()
    })
    .with_var("numbered_source", |s: &LinuxScanState| {
        s.numbered_source.clone()
    })
    .with_var("intra_file_context_block", |s: &LinuxScanState| {
        if s.intra_file_context.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<intra_file_context>\nFile-local types, macros, ops tables, 1-hop static callees, and intra-file caller call-sites from other sections of `{}` at revision `{}`:\n\n{}\n</intra_file_context>",
                s.file_path, s.target_sha, s.intra_file_context
            )
        }
    })
    .with_var("maintainers_block", |s: &LinuxScanState| {
        s.maintainers_hint
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map_or_else(String::new, |h| {
                if h.starts_with("MAINTAINERS") || h.starts_with("<official_maintainers") {
                    format!("\n{h}")
                } else {
                    format!("\nMAINTAINERS Subsystem(s): {h}")
                }
            })
    })
    .with_var("prefetched_block", |s: &LinuxScanState| {
        if s.prefetched_context.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following external definitions (structs, enums, macros, and callees) referenced by this chunk were pre-fetched at revision `{}`:\n\n{}\n</pre_fetched_context>",
                s.target_sha, s.prefetched_context
            )
        }
    })
    .with_var("focus_targets_block", |s: &LinuxScanState| {
        if s.focus_targets.is_empty() {
            String::new()
        } else {
            let json_str = serde_json::to_string_pretty(&s.focus_targets).unwrap_or_default();
            format!(
                "\n\n<triage_focus_targets>\nPhase 1 triage highlighted the following cross-function hypotheses as supplemental leads (do NOT restrict your audit to these functions—you MUST still audit EVERY function in <target_chunk_source> from top to bottom):\n{json_str}\n</triage_focus_targets>"
            )
        }
    })
    .with_var("custom_prompt_block", |s: &LinuxScanState| {
        s.custom_prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map_or_else(String::new, |p| {
                format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
            })
    })
}

/// Context-isolated system prompt for `contract-audit` (`Agent 2`), receiving ONLY
/// `Agent 1`'s `<architecture_contract_notes>`, `<external_callers>`, and raw chunk context
/// without `<triage_focus_targets>` anchoring or `Agent 1`'s chain-of-thought.
pub fn linux_contract_audit_system_prompt() -> PromptTemplate<LinuxScanState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();

    PromptTemplate::<LinuxScanState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.

You are an independent Linux kernel Contract-Breakage Auditor (Agent 2) verifying implementation code against `<architecture_contract_notes>` synthesized by an isolated Architecture Cartographer (Agent 1).

TOOL USAGE & EFFICIENCY: First check `<architecture_contract_notes>`, `<external_callers>`, `<target_chunk_source>`, `<intra_file_context>`, and `<pre_fetched_context>` already provided below—do NOT re-read code lines already shown in your prompt. When you need additional external callers or definitions using tools (`git_read_files`, `git_grep`, `git_blame`, `git_log`, `git_show`), batch independent tool calls into a single turn, pass explicit `start_line` and `end_line` bounds (`<= 200` lines) to `git_read_files`, and always pass `Target Revision SHA` (`{{{{target_sha}}}}`).{{{{maintainer_notes}}}}

=== Active Scan Target Metadata ===
Target Revision SHA: {{{{target_sha}}}}
Chunk ID: {{{{chunk_id}}}}
Target File: {{{{file_path}}}} (lines {{{{start_line}}}}..={{{{end_line}}}}){{{{maintainers_block}}}}
===================================

<architecture_contract_notes>
Normative subsystem data-structure lifecycles, synchronization protocols, pointer/sentinel domains, and caller/callee contracts documented by Agent 1 (verify every function in <target_chunk_source> against these rules, treating live code at Target Revision SHA as ground truth):
{{{{contract_notes}}}}
</architecture_contract_notes>{{{{external_callers_block}}}}

<file_function_index>
Outline of all functions in {{{{file_path}}}}:
{{{{file_function_index}}}}
</file_function_index>

<chunk_functions>
Functions defined in this chunk (lines {{{{start_line}}}}..={{{{end_line}}}}):
{{{{chunk_functions_json}}}}
</chunk_functions>

<target_chunk_source>
{{{{numbered_source}}}}
</target_chunk_source>{{{{intra_file_context_block}}}}{{{{prefetched_block}}}}{{{{custom_prompt_block}}}}"#
    ))
    .with_var("target_sha", |s: &LinuxScanState| s.target_sha.clone())
    .with_var("chunk_id", |s: &LinuxScanState| s.chunk_id.clone())
    .with_var("file_path", |s: &LinuxScanState| s.file_path.clone())
    .with_var("start_line", |s: &LinuxScanState| s.start_line.to_string())
    .with_var("end_line", |s: &LinuxScanState| s.end_line.to_string())
    .with_var("maintainer_notes", |s: &LinuxScanState| {
        s.maintainer_notes.clone()
    })
    .with_var("contract_notes", |s: &LinuxScanState| {
        s.contract_notes.trim().to_string()
    })
    .with_var("external_callers_block", |s: &LinuxScanState| {
        if s.external_callers.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<external_callers>\n1-hop external kernel call-sites and ops-table registrations invoking exported functions in this chunk at revision `{}`:\n\n{}\n</external_callers>",
                s.target_sha,
                s.external_callers.trim()
            )
        }
    })
    .with_var("file_function_index", |s: &LinuxScanState| {
        s.file_function_index.clone()
    })
    .with_var("chunk_functions_json", |s: &LinuxScanState| {
        s.chunk_functions_json.clone()
    })
    .with_var("numbered_source", |s: &LinuxScanState| {
        s.numbered_source.clone()
    })
    .with_var("intra_file_context_block", |s: &LinuxScanState| {
        if s.intra_file_context.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<intra_file_context>\nFile-local types, macros, ops tables, 1-hop static callees, and intra-file caller call-sites from other sections of `{}` at revision `{}`:\n\n{}\n</intra_file_context>",
                s.file_path, s.target_sha, s.intra_file_context
            )
        }
    })
    .with_var("maintainers_block", |s: &LinuxScanState| {
        s.maintainers_hint
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map_or_else(String::new, |h| {
                if h.starts_with("MAINTAINERS") || h.starts_with("<official_maintainers") {
                    format!("\n{h}")
                } else {
                    format!("\nMAINTAINERS Subsystem(s): {h}")
                }
            })
    })
    .with_var("prefetched_block", |s: &LinuxScanState| {
        if s.prefetched_context.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following external definitions (structs, enums, macros, and callees) referenced by this chunk were pre-fetched at revision `{}`:\n\n{}\n</pre_fetched_context>",
                s.target_sha, s.prefetched_context
            )
        }
    })
    .with_var("custom_prompt_block", |s: &LinuxScanState| {
        s.custom_prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map_or_else(String::new, |p| {
                format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
            })
    })
}

// ---------------------------------------------------------------------------
// Validation & Reduction Helpers
// ---------------------------------------------------------------------------

/// Checks whether a snapshot scan item contains at least one location entry with
/// non-empty `file`, `function_or_symbol`, and verbatim `code_snippet`.
///
/// Unlike patch-diff review locations, snapshot codebase scanning operates on a
/// single revision (`target_sha`) and therefore does not require a diff `side`.
pub fn has_valid_scan_proof_location(item: &Value) -> bool {
    let Some(locations) = item.get("locations").and_then(Value::as_array) else {
        return false;
    };
    locations.iter().any(|loc| {
        let has_file = loc
            .get("file")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_symbol = loc
            .get("function_or_symbol")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_snippet = loc
            .get("code_snippet")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        has_file && has_symbol && has_snippet
    })
}

const FORBIDDEN_TITLE_FIX_PREFIXES: &[&str] = &[
    "fix ",
    "fixes ",
    "fixing ",
    "resolve ",
    "resolves ",
    "resolving ",
    "prevent ",
    "prevents ",
    "preventing ",
    "avoid ",
    "avoids ",
    "avoiding ",
];

fn split_canonical_title_segments(trimmed: &str) -> Result<Vec<&str>, String> {
    let bytes = trimmed.as_bytes();
    let mut segments = Vec::new();
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let run_start = i;
            while i < bytes.len() && bytes[i] == b':' {
                i += 1;
            }
            let run_len = i - run_start;
            if run_len == 1 {
                segments.push(trimmed[seg_start..run_start].trim());
                seg_start = i;
            } else if run_len == 2 {
                let prev_ok = trimmed[..run_start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_whitespace());
                let next_ok = trimmed[i..]
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_whitespace());
                if !prev_ok || !next_ok {
                    return Err(format!(
                        "problem (canonical title) contains a malformed '::' separator; use '<subsystem_prefix>: <defect in fn()>' and only use '::' inside Rust/C++ identifiers like 'Type::method()'. Got: '{trimmed}'."
                    ));
                }
            } else {
                return Err(format!(
                    "problem (canonical title) contains an invalid colon sequence ('{}'). Got: '{trimmed}'.",
                    &trimmed[run_start..i]
                ));
            }
        } else {
            i += 1;
        }
    }
    segments.push(trimmed[seg_start..].trim());
    if segments.len() < 2 {
        return Err(format!(
            "problem (canonical title) must follow '<subsystem_prefix>: <defect in fn()>' format with a colon. Got: '{trimmed}'."
        ));
    }
    if segments.iter().any(|seg| seg.is_empty()) {
        return Err(format!(
            "problem (canonical title) must have both a non-empty subsystem prefix and defect description separated by ':'. Got: '{trimmed}'."
        ));
    }
    Ok(segments)
}

pub fn validate_canonical_bug_title(title: &str) -> Result<(), String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err("problem (canonical title) cannot be empty.".to_string());
    }
    if trimmed.chars().count() >= 80 {
        return Err(format!(
            "problem (canonical title) must be strictly under 80 characters (got {} chars): '{}'.",
            trimmed.chars().count(),
            trimmed
        ));
    }
    if trimmed.contains('`') {
        return Err(format!(
            "problem (canonical title) must not contain backticks ('`'): '{}'.",
            trimmed
        ));
    }
    let segments = split_canonical_title_segments(trimmed)?;
    for segment in &segments[1..] {
        let desc_lower = segment.to_ascii_lowercase();
        if FORBIDDEN_TITLE_FIX_PREFIXES
            .iter()
            .any(|forbidden| desc_lower.starts_with(forbidden))
        {
            return Err(format!(
                "problem (canonical title) must describe the existing defect rather than a patch/fix; do not start the description after ':' with action verbs like 'fix', 'prevent', 'avoid', or 'resolve'. Got: '{}'.",
                trimmed
            ));
        }
    }
    Ok(())
}

pub fn validate_canonical_bug_description(desc: &str) -> Result<(), String> {
    let trimmed = desc.trim();
    if trimmed.is_empty() {
        return Err("canonical_description cannot be empty.".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    if !lower.contains("trigger") && !lower.contains("precondition") {
        return Err(
            "canonical_description must include a 'Trigger / Preconditions:' section.".to_string(),
        );
    }
    if !lower.contains("failure mechanism") {
        return Err(
            "canonical_description must include a 'Failure Mechanism:' section.".to_string(),
        );
    }
    if !lower.contains("impact") {
        return Err("canonical_description must include an 'Impact:' section.".to_string());
    }
    Ok(())
}

pub fn validate_scan_finding_object(finding: &Value, idx: usize) -> Result<(), String> {
    let problem = finding.get("problem").and_then(Value::as_str).unwrap_or("");
    validate_canonical_bug_title(problem).map_err(|e| format!("findings[{idx}]: {e}"))?;

    let canonical_desc = finding
        .get("canonical_description")
        .and_then(Value::as_str)
        .unwrap_or("");
    validate_canonical_bug_description(canonical_desc)
        .map_err(|e| format!("findings[{idx}]: {e}"))?;

    let has_severity = finding
        .get("severity")
        .and_then(Value::as_str)
        .is_some_and(|s| {
            matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "low" | "medium" | "high" | "critical" | "unknown"
            )
        });
    let has_explanation = finding
        .get("severity_explanation")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if !has_severity || !has_explanation {
        return Err(format!(
            "findings[{idx}] must have valid 'severity' (Low, Medium, High, Critical, or Unknown) and non-empty 'severity_explanation'."
        ));
    }
    if !has_valid_scan_proof_location(finding) {
        return Err(format!(
            "findings[{idx}] must include a non-empty 'locations' array with at least one entry containing non-empty 'file', 'function_or_symbol', and verbatim 'code_snippet'."
        ));
    }
    Ok(())
}

fn validate_scan_concerns_output(
    output: &StageConcernsOutput,
    _state: &LinuxScanState,
) -> Result<(), String> {
    for (idx, concern) in output.concerns.iter().enumerate() {
        let has_desc = concern
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = concern
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc || !has_reasoning {
            return Err(format!(
                "concerns[{idx}] must have non-empty 'description' and 'reasoning' strings."
            ));
        }
        if !has_valid_scan_proof_location(concern) {
            return Err(format!(
                "concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and 'code_snippet'."
            ));
        }
    }

    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let has_desc = dismissed
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = dismissed
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc || !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must have non-empty 'description' and 'reasoning' strings."
            ));
        }
        if !has_valid_scan_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and verbatim disproving 'code_snippet'."
            ));
        }
    }
    Ok(())
}

fn format_scan_concerns_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays.",
        violation
    )
}

fn extract_non_empty_source_ids<'a>(
    collection_name: &str,
    idx: usize,
    item: &'a Value,
    valid_ids_label: &str,
) -> Result<Vec<&'a str>, String> {
    let Some(arr) = item.get("source_ids").and_then(Value::as_array) else {
        return Err(format!(
            "{collection_name}[{idx}] must include a non-empty 'source_ids' array referencing input item ID(s) ({valid_ids_label})."
        ));
    };
    let ids: Vec<&str> = arr
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() || ids.len() != arr.len() {
        return Err(format!(
            "{collection_name}[{idx}] must have a non-empty 'source_ids' array of non-empty string IDs ({valid_ids_label})."
        ));
    }
    Ok(ids)
}

pub fn validate_scan_verification_output(
    output: &VerificationOutput,
    state: &LinuxScanState,
) -> Result<(), String> {
    if !output.inconclusive_concerns.is_empty() {
        return Err(format!(
            "inconclusive_concerns contains {} entries, but active scan chunk consolidation must return an empty 'inconclusive_concerns': [] array. Emit every plausible Medium, High, or Critical candidate defect directly in 'findings' (cross-file tool verification runs after cross-chunk deduplication in the linux_bug pipeline), and place only concretely disproved or Low-trivia items in 'dismissed_concerns'.",
            output.inconclusive_concerns.len()
        ));
    }

    for (idx, finding) in output.findings.iter().enumerate() {
        validate_scan_finding_object(finding, idx)?;
    }

    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let desc = dismissed
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let has_reasoning = dismissed
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if desc.is_empty() || !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must have non-empty 'description' and 'reasoning' strings."
            ));
        }
        if !has_valid_scan_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and disproving 'code_snippet'."
            ));
        }
    }

    if !state.all_concerns.is_empty()
        && output.findings.is_empty()
        && output.dismissed_concerns.is_empty()
    {
        return Err(
            "At least one concern was raised by the analysis stages, so 'findings' and 'dismissed_concerns' cannot both be empty. Every consolidated concern must be classified either into 'findings' (if plausible) or into 'dismissed_concerns' (if concretely disproved by verbatim code)."
                .to_string(),
        );
    }

    if !state.all_dismissed_concerns.is_empty()
        && output.dismissed_concerns.is_empty()
        && output.findings.is_empty()
    {
        return Err(
            "At least one dismissed_concern was provided in input, so 'dismissed_concerns' and 'findings' cannot both be empty. Every input dismissed_concern must be classified into either 'dismissed_concerns' (if backed by concrete disproving code) or promoted to 'findings' (if relying on unproven cross-file assumptions)."
                .to_string(),
        );
    }

    let concern_ids: Vec<&str> = state
        .all_concerns
        .iter()
        .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::trim))
        .filter(|id| !id.is_empty())
        .collect();
    let dismissed_ids: Vec<&str> = state
        .all_dismissed_concerns
        .iter()
        .filter_map(|d| d.get("id").and_then(Value::as_str).map(str::trim))
        .filter(|id| !id.is_empty())
        .collect();

    if !concern_ids.is_empty() || !dismissed_ids.is_empty() {
        let all_valid: Vec<&str> = concern_ids
            .iter()
            .chain(dismissed_ids.iter())
            .copied()
            .collect();
        let valid_label = all_valid.join(", ");
        let mut accounted = std::collections::BTreeSet::new();

        for (collection_name, items) in [
            ("findings", &output.findings),
            ("dismissed_concerns", &output.dismissed_concerns),
        ] {
            for (idx, item) in items.iter().enumerate() {
                let sids = extract_non_empty_source_ids(collection_name, idx, item, &valid_label)?;
                for sid in sids {
                    if !all_valid.contains(&sid) {
                        return Err(format!(
                            "{collection_name}[{idx}].source_ids contains unknown ID '{sid}'. Valid input IDs are: {valid_label}."
                        ));
                    }
                    accounted.insert(sid);
                }
            }
        }

        let missing: Vec<&str> = all_valid
            .into_iter()
            .filter(|id| !accounted.contains(id))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "Verification output failed to account for input ID(s): {}. Every input concern (C*) and dismissed_concern (D*) ID must appear in 'source_ids' of at least one item in 'findings' or 'dismissed_concerns'.",
                missing.join(", ")
            ));
        }
    }

    Ok(())
}

pub fn validate_scan_post_verification_batch_items(
    output: &PostVerificationOutput,
    batch: &[Value],
) -> Result<(), String> {
    if output.findings.is_empty() && output.dismissed_concerns.is_empty() {
        return Err(
            "post-verification must not return both empty 'findings' and empty 'dismissed_concerns'. Every candidate inconclusive concern must be either validated in 'findings' or disproved in 'dismissed_concerns' with concrete code evidence in 'locations'."
                .to_string(),
        );
    }
    let expected_ids: Vec<&str> = batch
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str).map(str::trim))
        .filter(|id| !id.is_empty())
        .collect();
    let required = if expected_ids.is_empty() {
        batch.len().max(1)
    } else {
        1
    };
    let total = output.findings.len() + output.dismissed_concerns.len();
    if total < required {
        return Err(format!(
            "post-verification returned {total} item(s) across 'findings' and 'dismissed_concerns', but this batch has {required} candidate inconclusive concern(s). Every candidate in the batch must be accounted for in either 'findings' or 'dismissed_concerns'."
        ));
    }
    for (idx, finding) in output.findings.iter().enumerate() {
        validate_scan_finding_object(finding, idx)?;
    }
    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let has_desc = dismissed
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = dismissed
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc || !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must be an object with non-empty 'description' and 'reasoning' strings."
            ));
        }
        if !has_valid_scan_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include a non-empty 'locations' array with at least one entry containing non-empty 'file', 'function_or_symbol', and verbatim disproving 'code_snippet'."
            ));
        }
    }
    if expected_ids.is_empty() {
        return Ok(());
    }

    let valid_label = expected_ids.join(", ");
    let mut accounted = std::collections::BTreeSet::new();

    for (collection_name, items) in [
        ("findings", &output.findings),
        ("dismissed_concerns", &output.dismissed_concerns),
    ] {
        for (idx, item) in items.iter().enumerate() {
            let sids = extract_non_empty_source_ids(collection_name, idx, item, &valid_label)?;
            for sid in sids {
                if !expected_ids.contains(&sid) {
                    return Err(format!(
                        "{collection_name}[{idx}].source_ids contains unknown inconclusive concern ID '{sid}'. Valid candidate ID(s) in this batch: {valid_label}."
                    ));
                }
                accounted.insert(sid);
            }
        }
    }

    let missing: Vec<&str> = expected_ids
        .into_iter()
        .filter(|id| !accounted.contains(id))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "post-verification output failed to account for candidate inconclusive concern ID(s): {}. Every candidate ID in the batch must appear in 'source_ids' of at least one item in 'findings' or 'dismissed_concerns'.",
            missing.join(", ")
        ));
    }
    Ok(())
}

pub(crate) fn sanitize_repo_relative_path(raw: &str) -> Option<&str> {
    let mut cleaned = raw.trim();
    while let Some(rest) = cleaned.strip_prefix("./") {
        cleaned = rest;
    }
    if cleaned.is_empty()
        || cleaned.starts_with('-')
        || cleaned.starts_with('/')
        || cleaned.contains('\0')
        || cleaned.chars().any(char::is_control)
    {
        return None;
    }
    let path = std::path::Path::new(cleaned);
    if !path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(cleaned)
}

fn ensure_affected_source_files(finding: &mut Value, default_file: &str) {
    let mut files = Vec::new();
    if let Some(arr) = finding
        .get("affected_source_files")
        .and_then(Value::as_array)
    {
        for v in arr {
            if let Some(f) = v.as_str().and_then(sanitize_repo_relative_path) {
                push_unique_string(&mut files, f);
            }
        }
    }
    if let Some(locs) = finding.get("locations").and_then(Value::as_array) {
        for loc in locs {
            if let Some(f) = loc
                .get("file")
                .and_then(Value::as_str)
                .and_then(sanitize_repo_relative_path)
            {
                push_unique_string(&mut files, f);
            }
        }
    }
    if files.is_empty()
        && let Some(default_clean) = sanitize_repo_relative_path(default_file)
    {
        push_unique_string(&mut files, default_clean);
    }
    if let Some(map) = finding.as_object_mut() {
        map.insert("affected_source_files".to_string(), json!(files));
        map.insert("preexisting".to_string(), json!(true));
    }
}

pub fn is_high_or_critical_finding(finding: &Value) -> bool {
    finding
        .get("severity")
        .and_then(Value::as_str)
        .is_some_and(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "high" | "critical"))
}

pub fn is_actionable_scan_finding(finding: &Value) -> bool {
    is_actionable_scan_finding_for_floor(finding, crate::db::SeverityFloor::Medium)
}

pub fn is_actionable_scan_finding_for_floor(
    finding: &Value,
    floor: crate::db::SeverityFloor,
) -> bool {
    // Retain at least Medium candidates before Phase 2 so SeveritySession can
    // calibrate borderline issues, while also admitting Low when --min-severity low.
    let prefilter = floor.min(crate::db::SeverityFloor::Medium);
    finding
        .get("severity")
        .and_then(Value::as_str)
        .map(crate::db::Severity::from_str)
        .is_some_and(|sev| prefilter.admits(sev))
}

pub fn scan_kernel_threat_model_severity_section(state: &LinuxScanState) -> String {
    let rendered = crate::threat_model::KernelThreatModel::render_for_severity(
        state.kernel_threat_model.as_ref(),
    );
    if rendered.is_empty() {
        String::new()
    } else {
        format!("\n\n{rendered}")
    }
}

pub(crate) fn record_verified_scan_findings(state: &mut LinuxScanState, findings: Vec<Value>) {
    let floor = state
        .min_severity
        .unwrap_or(crate::db::SeverityFloor::Medium);
    for mut finding in findings {
        ensure_affected_source_files(&mut finding, &state.file_path);
        if is_actionable_scan_finding_for_floor(&finding, floor) {
            state.findings.push(finding);
        } else {
            let sev = finding
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("Low");
            let desc = finding
                .get("problem")
                .and_then(Value::as_str)
                .unwrap_or("Low-severity scan finding");
            let expl = finding
                .get("severity_explanation")
                .and_then(Value::as_str)
                .unwrap_or("");
            state.deduplicated_dismissed_concerns.push(json!({
                "origin": "scan_severity_filter",
                "type": "Below Scan Threshold",
                "description": desc,
                "reasoning": format!("Filtered before linux_bug pipeline because preliminary severity is {sev}: {expl}"),
                "locations": finding.get("locations").cloned().unwrap_or_else(|| json!([])),
            }));
        }
    }
}

pub fn apply_scan_verification_stage_output(
    state: &mut LinuxScanState,
    mut out: VerificationOutput,
) {
    let mut temp_pv = PostVerificationOutput {
        findings: std::mem::take(&mut out.findings),
        dismissed_concerns: std::mem::take(&mut out.dismissed_concerns),
    };
    let mut combined_sources =
        Vec::with_capacity(state.all_concerns.len() + state.all_dismissed_concerns.len());
    combined_sources.extend_from_slice(&state.all_concerns);
    combined_sources.extend_from_slice(&state.all_dismissed_concerns);
    enrich_post_verification_output(
        &[],
        &combined_sources,
        &mut temp_pv,
        scan_analysis_stage_by_name,
    );
    out.findings = temp_pv.findings;
    out.dismissed_concerns = temp_pv.dismissed_concerns;

    for (idx, finding) in out.findings.iter_mut().enumerate() {
        ensure_affected_source_files(finding, &state.file_path);
        if let Some(map) = finding.as_object_mut() {
            let vf_id = format!("VF{}", idx.saturating_add(1));
            map.insert("id".to_string(), json!(vf_id));
            map.insert("stage_item_id".to_string(), json!(vf_id));
            map.insert("origin".to_string(), json!("verification"));
        }
    }
    for (idx, inconclusive) in out.inconclusive_concerns.iter_mut().enumerate() {
        let (mut stages, mut prompts) =
            extract_item_stages_and_prompts(inconclusive, scan_analysis_stage_by_name);
        if let Some(sids) = inconclusive.get("source_ids").and_then(Value::as_array) {
            for sid in sids.iter().filter_map(Value::as_str) {
                for src in &combined_sources {
                    if src.get("id").and_then(Value::as_str) == Some(sid) {
                        let (src_stages, src_prompts) =
                            extract_item_stages_and_prompts(src, scan_analysis_stage_by_name);
                        for s in src_stages {
                            push_unique_string(&mut stages, &s);
                        }
                        for p in src_prompts {
                            push_unique_prompt(&mut prompts, &p);
                        }
                    }
                }
            }
        }
        if let Some(map) = inconclusive.as_object_mut() {
            map.insert(
                "id".to_string(),
                json!(format!("I{}", idx.saturating_add(1))),
            );
            if !stages.is_empty() {
                map.insert("stage".to_string(), json!(stages[0]));
                map.insert("stages".to_string(), json!(stages));
            }
            map.insert("prompts".to_string(), json!(prompts));
        }
    }
    let planned_batches = batch_inconclusive_concerns_by_severity(&out.inconclusive_concerns);
    for (batch_idx, batch) in planned_batches.iter().enumerate() {
        let assigned_stage = POST_VERIFICATION_STAGE_NAMES[batch_idx];
        for batch_item in batch {
            if let Some(hid) = batch_item.get("id").and_then(Value::as_str) {
                for hc in &mut out.inconclusive_concerns {
                    if hc.get("id").and_then(Value::as_str) == Some(hid)
                        && let Some(map) = hc.as_object_mut()
                    {
                        map.insert("assigned_stage".to_string(), json!(assigned_stage));
                    }
                }
            }
        }
    }
    for (idx, dismissed) in out.dismissed_concerns.iter_mut().enumerate() {
        if let Some(map) = dismissed.as_object_mut() {
            map.insert(
                "id".to_string(),
                json!(format!("VD{}", idx.saturating_add(1))),
            );
            map.insert("origin".to_string(), json!("verification"));
        }
    }

    state.verification_findings.extend(out.findings.clone());
    state
        .verification_dismissed
        .extend(out.dismissed_concerns.clone());
    record_verified_scan_findings(state, out.findings);
    state.inconclusive_concerns = out.inconclusive_concerns;
    state
        .deduplicated_dismissed_concerns
        .extend(out.dismissed_concerns);
}

pub fn apply_scan_post_verification_stage_output(
    state: &mut LinuxScanState,
    stage_name: &str,
    batch: &[Value],
    mut out: PostVerificationOutput,
) {
    enrich_post_verification_output(&[], batch, &mut out, scan_analysis_stage_by_name);
    for finding in &mut out.findings {
        ensure_affected_source_files(finding, &state.file_path);
        let pv_idx = state.post_verification_findings.len().saturating_add(1);
        let pvf_id = format!("PVF{pv_idx}");
        if let Some(map) = finding.as_object_mut() {
            map.insert("origin".to_string(), json!(stage_name));
            map.insert("post_verification_stage".to_string(), json!(stage_name));
            map.insert("id".to_string(), json!(pvf_id));
            map.insert("stage_item_id".to_string(), json!(pvf_id));
        }
        state.post_verification_findings.push(finding.clone());
    }
    for dismissed in &mut out.dismissed_concerns {
        let pvd_idx = state.post_verification_dismissed.len().saturating_add(1);
        let pvd_id = format!("PVD{pvd_idx}");
        if let Some(map) = dismissed.as_object_mut() {
            map.insert("id".to_string(), json!(pvd_id));
            map.insert("origin".to_string(), json!(stage_name));
            map.insert("post_verification_stage".to_string(), json!(stage_name));
        }
        state.post_verification_dismissed.push(dismissed.clone());
    }
    record_verified_scan_findings(state, out.findings);
    state
        .deduplicated_dismissed_concerns
        .extend(out.dismissed_concerns);
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

pub fn validate_scan_triage_output(
    out: &ScanTriageOutput,
    _state: &LinuxScanState,
) -> Result<(), String> {
    for raw_stage in &out.relevant_stages {
        if scan_analysis_stage_by_name(raw_stage).is_none() {
            return Err(format!(
                "relevant_stages contains unknown stage {raw_stage:?}; expected one of implementation, execution-flow, resources, locking, security, hardware."
            ));
        }
    }
    for (raw_stage, raw_effort) in &out.stage_efforts {
        if scan_analysis_stage_by_name(raw_stage).is_none() {
            return Err(format!(
                "stage_efforts contains unknown stage {raw_stage:?}."
            ));
        }
        if crate::ai::ReasoningEffort::from_str_loose(raw_effort).is_none() {
            return Err(format!(
                "stage_efforts[{raw_stage:?}] has invalid effort {raw_effort:?}; expected \"medium\" or \"high\"."
            ));
        }
    }
    Ok(())
}

pub fn validate_scan_triage_output_with_effort(
    out: &ScanTriageOutput,
    state: &LinuxScanState,
) -> Result<(), String> {
    validate_scan_triage_output(out, state)?;
    for raw_stage in &out.relevant_stages {
        if let Some(def) = scan_analysis_stage_by_name(raw_stage) {
            let has_effort = out
                .stage_efforts
                .keys()
                .any(|k| scan_analysis_stage_by_name(k).is_some_and(|d| d.name == def.name));
            if !has_effort {
                return Err(format!(
                    "stage_efforts must provide an effort (\"medium\" or \"high\") for every stage in relevant_stages (missing {:?}).",
                    def.name
                ));
            }
        }
    }
    Ok(())
}

pub fn format_scan_triage_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Respond with ONLY a valid JSON object containing \"relevant_stages\" and \"focus_targets\" (plus \"stage_efforts\" when requested).",
        violation
    )
}

pub fn build_scan_stage_efforts_schema(stages: &[AnalysisStage]) -> Value {
    let mut props = serde_json::Map::new();
    for def in stages {
        props.insert(
            def.name.to_string(),
            json!({
                "type": "string",
                "enum": ["medium", "high"]
            }),
        );
    }
    // Leave required omitted so dynamic-effort early exits on benign
    // boilerplate chunks can emit an empty stage_efforts object under
    // constrained JSON schema decoding. Non-empty selections are checked by
    // validate_scan_triage_output_with_effort and defaulted by
    // apply_scan_triage_output.
    json!({
        "type": "object",
        "properties": props,
    })
}

pub fn apply_scan_triage_output(
    dynamic_effort: bool,
    out: ScanTriageOutput,
    state: &mut LinuxScanState,
) {
    let mut triage_selected = std::collections::BTreeSet::new();
    for raw_name in out.relevant_stages {
        if let Some(def) = scan_analysis_stage_by_name(&raw_name) {
            triage_selected.insert(def.name);
        }
    }
    // Honor explicit early-exit (`relevant_stages: []`); otherwise ensure all
    // non-optional core scan stages run in canonical order alongside any
    // optional stages (`security`, `hardware`) selected by triage.
    let stages: Vec<String> = if triage_selected.is_empty() {
        Vec::new()
    } else {
        SCAN_ANALYSIS_STAGES
            .iter()
            .filter(|def| !def.optional || triage_selected.contains(def.name))
            .map(|def| def.name.to_string())
            .collect()
    };
    let mut efforts = std::collections::BTreeMap::new();
    if dynamic_effort {
        for (raw_stage, raw_effort) in out.stage_efforts {
            if let Some(def) = scan_analysis_stage_by_name(&raw_stage)
                && let Some(parsed) = crate::ai::ReasoningEffort::from_str_loose(&raw_effort)
            {
                let clamped = match parsed {
                    crate::ai::ReasoningEffort::Low => crate::ai::ReasoningEffort::Medium,
                    other => other,
                };
                efforts.insert(def.name.to_string(), clamped);
            }
        }
        for stage_name in &stages {
            efforts
                .entry(stage_name.clone())
                .or_insert(crate::ai::ReasoningEffort::Medium);
        }
    }
    state.planned_stages = stages;
    state.planned_efforts = efforts;
    state.focus_targets = out.focus_targets;
}

pub fn scan_triage_stage_with_effort(
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<LinuxScanState, ScanTriageOutput> {
    let dynamic_effort = effort == Some(crate::ai::EffortMode::Dynamic);
    let stage_names: Vec<&'static str> = SCAN_ANALYSIS_STAGES.iter().map(|d| d.name).collect();

    let focus_targets_schema = json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "function": { "type": "string" },
                "lines": { "type": "string" },
                "risk_category": { "type": "string" },
                "hypothesis": { "type": "string" },
                "callers_or_callees_to_check": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["function", "risk_category", "hypothesis"]
        }
    });

    let (user_prompt, output_format) = if dynamic_effort {
        let stage_efforts_schema = build_scan_stage_efforts_schema(SCAN_ANALYSIS_STAGES);
        (
            r#"Perform a Phase 1 exploration and triage pass over the target kernel source chunk:
1. Select which specialist audit stages should run on this chunk (`relevant_stages`):
   - Core stages (`implementation`, `execution-flow`, `resources`, `locking`) should ALWAYS be included whenever the chunk contains non-trivial executable logic.
   - `security`: MUST be included whenever the chunk touches syscalls, ioctls, netlink, bpf, ptrace/regsets, `procfs`/`sysfs`/`cgroupfs`/`kernfs` handlers, mmap/fault paths, packet/sched classifiers, namespaces/capabilities, `copy_from_user`/`copy_to_user`, `kvm`/`vgic`/`vhost`/`virtio`, or resource/cgroup memory accounting and bounded ID tables.
   - `hardware`: Include when the chunk touches CPU architecture / system registers (`arch/*`, `kvm`, `vgic`, `ptrace`, `iommu`), device driver MMIO registers, bitmasks, DMA mappings, IRQ/NAPI handlers, runtime PM / clocks, or open/stop/suspend/resume/reset state machines.
   CRITICAL EARLY-EXIT RULE: ONLY if this chunk consists purely of benign boilerplate (e.g., static tables, constant definitions, tracepoints, simple getters/setters, or trivial wrappers with no plausible Medium/High/Critical bug surface), return `"relevant_stages": []`, `"focus_targets": []`, and `"stage_efforts": {}`.
2. Identify cross-function risk leads in `focus_targets` (each object requiring `"function"`, `"risk_category"`, and `"hypothesis"`, plus optional `"lines"` and `"callers_or_callees_to_check"`), stating specific cross-file callers, framework callbacks (such as `css_free_rwork_fn`), or paired init/teardown helpers that specialist stages should inspect with Git tools (specialist stages will still audit every function in the chunk).
3. For each selected stage in `relevant_stages`, assign `"medium"` or `"high"` reasoning effort in `stage_efforts`.

Respond with ONLY a JSON object. Example:
```json
{
  "relevant_stages": ["implementation", "execution-flow", "resources", "locking", "security"],
  "focus_targets": [
    {
      "function": "smb2_ioctl",
      "lines": "140-215",
      "risk_category": "resources",
      "hypothesis": "Early return when out_buf_len < sizeof(...) may leak allocated payload buffer",
      "callers_or_callees_to_check": ["smb2_allocate_rsp_buf", "ksmbd_iov_pin_rsp"]
    }
  ],
  "stage_efforts": {
    "implementation": "medium",
    "execution-flow": "high",
    "resources": "high",
    "locking": "medium",
    "security": "high"
  }
}
```"#,
            OutputFormat::json_with_schema(json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "string", "enum": stage_names }
                    },
                    "focus_targets": focus_targets_schema,
                    "stage_efforts": stage_efforts_schema,
                },
                "required": ["relevant_stages", "focus_targets", "stage_efforts"]
            }))
            .with_validator(validate_scan_triage_output_with_effort)
            .with_feedback_formatter(format_scan_triage_feedback),
        )
    } else {
        (
            r#"Perform a Phase 1 exploration and triage pass over the target kernel source chunk:
1. Select which specialist audit stages should run on this chunk (`relevant_stages`):
   - Core stages (`implementation`, `execution-flow`, `resources`, `locking`) should ALWAYS be included whenever the chunk contains non-trivial executable logic.
   - `security`: MUST be included whenever the chunk touches syscalls, ioctls, netlink, bpf, ptrace/regsets, `procfs`/`sysfs`/`cgroupfs`/`kernfs` handlers, mmap/fault paths, packet/sched classifiers, namespaces/capabilities, `copy_from_user`/`copy_to_user`, `kvm`/`vgic`/`vhost`/`virtio`, or resource/cgroup memory accounting and bounded ID tables.
   - `hardware`: Include when the chunk touches CPU architecture / system registers (`arch/*`, `kvm`, `vgic`, `ptrace`, `iommu`), device driver MMIO registers, bitmasks, DMA mappings, IRQ/NAPI handlers, runtime PM / clocks, or open/stop/suspend/resume/reset state machines.
   CRITICAL EARLY-EXIT RULE: ONLY if this chunk consists purely of benign boilerplate (e.g., static tables, constant definitions, tracepoints, simple getters/setters, or trivial wrappers with no plausible Medium/High/Critical bug surface), return `"relevant_stages": []` and `"focus_targets": []`.
2. Identify cross-function risk leads in `focus_targets` (each object requiring `"function"`, `"risk_category"`, and `"hypothesis"`, plus optional `"lines"` and `"callers_or_callees_to_check"`), stating specific cross-file callers, framework callbacks (such as `css_free_rwork_fn`), or paired init/teardown helpers that specialist stages should inspect with Git tools (specialist stages will still audit every function in the chunk).

Respond with ONLY a JSON object. Example:
```json
{
  "relevant_stages": ["implementation", "execution-flow", "resources", "locking", "security"],
  "focus_targets": [
    {
      "function": "smb2_ioctl",
      "lines": "140-215",
      "risk_category": "resources",
      "hypothesis": "Early return when out_buf_len < sizeof(...) may leak allocated payload buffer",
      "callers_or_callees_to_check": ["smb2_allocate_rsp_buf", "ksmbd_iov_pin_rsp"]
    }
  ]
}
```"#,
            OutputFormat::json_with_schema(json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "string", "enum": stage_names }
                    },
                    "focus_targets": focus_targets_schema,
                },
                "required": ["relevant_stages", "focus_targets"]
            }))
            .with_validator(validate_scan_triage_output)
            .with_feedback_formatter(format_scan_triage_feedback),
        )
    };

    Stage::builder("triage")
        .system_prompt(linux_scan_system_prompt())
        .user_prompt(PromptTemplate::<LinuxScanState>::new(user_prompt))
        .output_format(output_format)
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::lightweight_stage_effort),
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(move |state, out: ScanTriageOutput| {
            apply_scan_triage_output(dynamic_effort, out, state);
        })
        .build()
}

pub fn scan_analysis_stage_user_prompt(
    def: &'static AnalysisStage,
) -> PromptTemplate<LinuxScanState> {
    let threat_model_placeholder = if def.name == "security" {
        "{{kernel_threat_model_section}}"
    } else {
        ""
    };
    let mut user_template = PromptTemplate::<LinuxScanState>::new(format!(
        "{}{}\n\n{}",
        def.instruction, threat_model_placeholder, SCAN_STAGE_JSON_SCHEMA_INSTRUCTIONS
    ));
    for guide in def.guides {
        user_template = user_template.include_file(*guide);
    }
    if def.name == "security" {
        user_template =
            user_template.with_var("kernel_threat_model_section", |s: &LinuxScanState| {
                let rendered = crate::threat_model::KernelThreatModel::render_for_security_stage(
                    s.kernel_threat_model.as_ref(),
                );
                if rendered.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{rendered}")
                }
            });
    }
    user_template
}

fn scan_analysis_stage(
    def: &'static AnalysisStage,
    max_turns: usize,
    temperature: f32,
    reasoning_effort: Option<crate::ai::ReasoningEffort>,
) -> Box<dyn ExecutableStage<LinuxScanState>> {
    let sys_prompt = if def.name == SCAN_CONTRACT_AUDIT_STAGE.name {
        linux_contract_audit_system_prompt()
    } else {
        linux_scan_system_prompt()
    };
    let user_template = scan_analysis_stage_user_prompt(def);

    Box::new(
        Stage::builder(def.name)
            .system_prompt(sys_prompt)
            .user_prompt(user_template)
            .output_format(
                OutputFormat::json()
                    .with_validator(validate_scan_concerns_output)
                    .with_feedback_formatter(format_scan_concerns_feedback),
            )
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                reasoning_effort,
                ..Default::default()
            })
            .reduce(
                move |state: &mut LinuxScanState, out: StageConcernsOutput| {
                    let prompts = collect_stage_prompts(&[], def.guides);
                    append_stage_items_with_prompts(
                        &mut state.all_concerns,
                        &out.concerns,
                        def.name,
                        "General",
                        &prompts,
                    );
                    append_stage_dismissed_concerns_with_prompts(
                        &mut state.all_dismissed_concerns,
                        &out.dismissed_concerns,
                        def.name,
                        &prompts,
                    );
                },
            )
            .build(),
    )
}

pub fn resolve_scan_stage_effort(
    state: &LinuxScanState,
    stage_name: &str,
) -> Option<crate::ai::ReasoningEffort> {
    state
        .effort
        .map(|mode| mode.analysis_stage_effort(state.planned_efforts.get(stage_name).copied()))
}

pub fn resolve_scan_analysis_stages_with_options(
    state: &LinuxScanState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxScanState>>> {
    let mut selected_stages: Vec<String> = if let Some(ref manual) = state.manual_stages {
        manual.clone()
    } else {
        // When `triage` ran and selected an empty `planned_stages`, honor the
        // early-exit decision by returning no specialist stages.
        let mut planned = state.planned_stages.clone();
        if !planned.is_empty()
            && !state.contract_notes.trim().is_empty()
            && !planned.iter().any(|s| s == SCAN_CONTRACT_AUDIT_STAGE.name)
        {
            planned.insert(0, SCAN_CONTRACT_AUDIT_STAGE.name.to_string());
        }
        planned
    };
    if state.contract_notes.trim().is_empty() && state.manual_stages.is_none() {
        selected_stages.retain(|s| s != SCAN_CONTRACT_AUDIT_STAGE.name);
    }

    let mut stages = Vec::new();
    for name in selected_stages {
        match scan_analysis_stage_by_name(&name) {
            Some(def) => {
                let effort = resolve_scan_stage_effort(state, def.name);
                stages.push(scan_analysis_stage(def, max_turns, temperature, effort));
            }
            None => tracing::warn!("Ignoring unknown scan stage {:?}", name),
        }
    }
    stages
}

const STAGE_SCAN_VERIFICATION_INSTRUCTION: &str = r#"# Scan Candidate Consolidation, Deduplication & Canonical Bug Formatting

You are the lead Linux kernel maintainer consolidating `concerns` and `dismissed_concerns` produced by parallel specialist scan stages on an existing kernel source chunk.
Every candidate you emit in `findings` will be deduplicated across chunks of the file and then verified against the full repository by the Phase 2 `linux_bug` adversarial verifier (which has full Git tools to inspect cross-file callers, callees, locking, and lifecycle invariants). Therefore, do NOT defer plausible defects to `"inconclusive_concerns"`—`"inconclusive_concerns"` MUST always be `[]`.

Your task is to:
1. **Deduplicate only identical root-cause + function defects:** Merge duplicate items ONLY when they share the exact same root cause AND the same function/lifecycle phase. NEVER merge distinct bugs in the same function (e.g., an error-unwind leak vs. a signedness/shift bug in the same function) or setup vs. teardown bugs across separate functions.
2. **Classify every consolidated candidate into `findings` or `dismissed_concerns`:**
   - **`findings`:** Emit every plausible `Medium`, `High`, or `Critical` kernel defect (`C*`) that is NOT concretely disproved by verbatim code in the chunk, `<intra_file_context>`, `<prefetched_context>`, or a competing `D*`. If a `dismissed_concern` (`D*`) dismissed a real defect by assuming an unverified cross-file caller/framework invariant without verbatim disproving code from all callers, promote that `D*` into `findings` so Phase 2 verifies the callers with Git tools!
   - **`dismissed_concerns`:** Place items here ONLY when backed by a concrete verbatim disproving `code_snippet` in `locations` proving the failure mode is impossible (or when an item is pure `Low` style/hygiene trivia per `false-positive-guide.md`). Both `D*` and concretely refuted `C*` IDs may be placed in `dismissed_concerns`.

### CRITICAL SEVERITY & CANONICAL BUGREPO FORMAT REQUIREMENTS
Active codebase scanning forwards `Medium`, `High`, and `Critical` kernel defects to the `linux_bug` verification and severity calibration pipeline (filtering out `Low` hygiene/trivia).
- **LINEAGE REQUIREMENT (`source_ids`):** Every input item in Aggregated Concerns has an `"id"` (`"C1"`, `"C2"`, ...) and every input item in Aggregated Dismissed Concerns has an `"id"` (`"D1"`, `"D2"`, ...). Every output object across `findings` and `dismissed_concerns` MUST include a non-empty `"source_ids"` array listing the exact input `"id"` string(s) merged into that item. Every input `"C*"` and `"D*"` ID MUST appear in at least one output item's `"source_ids"`.
- **`findings` required keys:**
  1. `"source_ids"`: non-empty array of input `C*` and/or promoted `D*` IDs.
  2. `"problem"` (Canonical Title):
     - Format: `"<subsystem_prefix>: <defect or broken invariant in function_name()>"` (e.g. `"smb: server: memory leak in parse_durable_handle_context()"` or `"mm: memcontrol: private_id leak in __mem_cgroup_free()"`).
     - Strictly under 80 characters, NO backticks (`), NO markdown.
     - Do NOT start the description after `:` with patch/fix verbs (`fix`, `fixes`, `prevent`, `avoid`, `resolve`). State the defect directly.
  3. `"canonical_description"`:
     - A structured multi-line technical description containing all 4 canonical sections:
       `Trigger / Preconditions: ...`
       `Call Chain: ...`
       `Failure Mechanism: ...`
       `Impact: ...`
  4. `"severity"`: `"Low"`, `"Medium"`, `"High"`, `"Critical"`, or `"Unknown"` calibrated per `severity.md` and `<kernel_threat_model>` (when provided below). Note: leaks of bounded global IDs/resources, guest-triggerable `WARN_ON*`, error-path page/memory leaks, and broken hardware/register masks or type truncations are `Medium` or `High`, never `Low`.
  5. `"severity_explanation"`: Detailed step-by-step proof of consequence, actor/privilege & trust boundary, and triggering preconditions.
  6. `"affected_source_files"`: Non-empty array of repository-relative file paths (e.g. `["fs/smb/server/smb2pdu.c"]`).
  7. `"locations"`: Non-empty array of verified location objects (`file`, `function_or_symbol`, `line`, `code_snippet`, `why_this_location_matters`).
- **`inconclusive_concerns`:** MUST always be an empty array `[]`.
- **`dismissed_concerns` required keys:**
  1. `"source_ids"`: non-empty array of input `D*` and/or concretely disproved `C*` IDs.
  2. `"type"`: short category string.
  3. `"description"`: the concern that was investigated and dismissed.
  4. `"reasoning"`: concrete code explanation proving safety (or explaining why it is pure `Low` style trivia).
  5. `"locations"`: non-empty array of location objects (`file`, `function_or_symbol`, `line`, `code_snippet`, `why_this_location_matters`) with a non-empty verbatim disproving `code_snippet`."#;

pub fn scan_verification_stage_with_effort(
    _max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<LinuxScanState, VerificationOutput> {
    let user_template = PromptTemplate::<LinuxScanState>::new(format!(
        r#"{STAGE_SCAN_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>{{{{kernel_threat_model_section}}}}@includes

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'findings', 'inconclusive_concerns' (MUST be []), and 'dismissed_concerns' arrays.
- Every input "C*" and "D*" ID MUST appear in at least one output item's "source_ids" across 'findings' or 'dismissed_concerns'.
- Emit every plausible Medium, High, or Critical defect directly in 'findings' so Phase 2 cross-file verification checks it once after cross-chunk deduplication."#
    ))
    .include_file("false-positive-guide.md")
    .include_file("severity.md")
    .include_files_from_state(|s: &LinuxScanState| {
        let mut combined =
            Vec::with_capacity(s.all_concerns.len() + s.all_dismissed_concerns.len());
        combined.extend_from_slice(&s.all_concerns);
        combined.extend_from_slice(&s.all_dismissed_concerns);
        extra_prompt_paths_for_items(&[], &combined, scan_analysis_stage_by_name)
    })
    .with_var(
        "kernel_threat_model_section",
        scan_kernel_threat_model_severity_section,
    )
    .with_var("aggregated_concerns", |s: &LinuxScanState| {
        serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
    })
    .with_var("aggregated_dismissed_concerns", |s: &LinuxScanState| {
        serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
    });

    Stage::builder(SCAN_VERIFICATION.name)
        .system_prompt(linux_scan_system_prompt())
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(validate_scan_verification_output)
                .with_feedback_formatter(format_verification_stage_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::verification_stage_effort),
            ..Default::default()
        })
        .reduce(|state, out: VerificationOutput| {
            apply_scan_verification_stage_output(state, out);
        })
        .build()
}

const STAGE_SCAN_POST_VERIFICATION_INSTRUCTION: &str = r#"# Per-Finding Deep Codebase Post-Verification & Canonical Bug Formatting

You are the lead Linux kernel maintainer performing deep, tool-assisted verification of a speculative or contested candidate issue (`inconclusive_concerns`) against the repository at `Target Revision SHA`.
1. Use `git_read_files`, `git_grep`, `git_blame`, and `git_show` at the target revision to answer each candidate's `verification_question` and verify ALL callers, callees, lock contexts, and lifecycle paths.
2. Every candidate `"I*"` ID in this batch MUST be accounted for in either `findings` or `dismissed_concerns` via a non-empty `"source_ids"` array.
3. To dismiss a candidate in `dismissed_concerns`, you MUST use the required keys:
   - `"source_ids"`: non-empty array of batch `"I*"` IDs.
   - `"description"`: the candidate issue that was disproved.
   - `"reasoning"`: step-by-step explanation of how the inspected code disproves the candidate.
   - `"locations"`: non-empty array of location objects (`file`, `function_or_symbol`, `line`, `code_snippet`, `why_this_location_matters`) quoting the verbatim disproving code proving the failure mode is impossible across all callers.
4. If the defect is real and reachable, emit it in `findings` using the exact canonical bug repository format:
   - `"source_ids"`: non-empty array of batch `"I*"` IDs.
   - `"problem"`: `"<subsystem_prefix>: <defect in function_name()>"` (strictly < 80 chars, NO backticks, NO fix-verbs like `fix`, `prevent`, `avoid`, `resolve` after `:`).
   - `"canonical_description"`: multi-line string containing `Trigger / Preconditions:`, `Call Chain:`, `Failure Mechanism:`, and `Impact:`.
   - `"severity"`: `"Low"`, `"Medium"`, `"High"`, `"Critical"`, or `"Unknown"` per `severity.md`.
   - `"severity_explanation"`: concrete step-by-step proof of reachability and impact.
   - `"affected_source_files"`: non-empty array of repository-relative file paths.
   - `"locations"`: non-empty array of verified location objects (`file`, `function_or_symbol`, `line`, `code_snippet`, `why_this_location_matters`)."#;

pub fn scan_post_verification_stage_with_effort(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<LinuxScanState, PostVerificationOutput> {
    let candidate_json = serde_json::to_string_pretty(&batch).unwrap_or_default();
    let batch_for_prompts = batch.clone();
    let batch_for_validate = batch.clone();
    let batch_for_reduce = batch;

    let user_template = PromptTemplate::<LinuxScanState>::new(format!(
        r#"{STAGE_SCAN_POST_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>{{{{kernel_threat_model_section}}}}@includes

Candidate Inconclusive Concern(s) to Verify:
{{{{candidate_inconclusive_concerns}}}}

Return ONLY a JSON object with 'findings' and 'dismissed_concerns' arrays accounting for every candidate I* ID in this batch."#
    ))
    .include_file("false-positive-guide.md")
    .include_file("severity.md")
    .include_files_from_state(move |_: &LinuxScanState| {
        extra_prompt_paths_for_items(&[], &batch_for_prompts, scan_analysis_stage_by_name)
    })
    .with_var(
        "kernel_threat_model_section",
        scan_kernel_threat_model_severity_section,
    )
    .with_var("candidate_inconclusive_concerns", move |_: &LinuxScanState| {
        candidate_json.clone()
    });

    Stage::builder(stage_name)
        .system_prompt(linux_scan_system_prompt())
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(move |out, _state| {
                    validate_scan_post_verification_batch_items(out, &batch_for_validate)
                })
                .with_feedback_formatter(format_post_verification_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::verification_stage_effort),
            ..Default::default()
        })
        .reduce(move |state, out: PostVerificationOutput| {
            apply_scan_post_verification_stage_output(state, stage_name, &batch_for_reduce, out);
        })
        .build()
}

pub fn resolve_scan_post_verification_stages_with_options(
    state: &LinuxScanState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxScanState>>> {
    let effort = state.effort;
    let batches = batch_inconclusive_concerns_by_severity(&state.inconclusive_concerns);
    batches
        .into_iter()
        .take(MAX_POST_VERIFICATION_STAGES)
        .enumerate()
        .map(|(idx, batch)| {
            let stage_name = POST_VERIFICATION_STAGE_NAMES[idx];
            Box::new(scan_post_verification_stage_with_effort(
                stage_name,
                batch,
                max_turns,
                temperature,
                effort,
            )) as Box<dyn ExecutableStage<LinuxScanState>>
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Complete Linux Scan Workflow Graph
// ---------------------------------------------------------------------------

impl LinuxScanState {
    /// Converts confirmed `High` and `Critical` chunk findings into standard
    /// [`crate::api::BugInput`] payloads ready for the [`crate::workflows::linux_bug`]
    /// pipeline.
    pub fn to_candidate_bug_inputs(
        &self,
        maintainers: Option<&crate::maintainers::MaintainersIndex>,
    ) -> Vec<crate::api::BugInput> {
        let mut inputs = Vec::with_capacity(self.findings.len());
        for finding in &self.findings {
            let problem = finding
                .get("problem")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if problem.is_empty() {
                continue;
            }
            let canonical_desc = finding
                .get("canonical_description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let severity_expl = finding
                .get("severity_explanation")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let reasoning = match (!canonical_desc.is_empty(), !severity_expl.is_empty()) {
                (true, true) => {
                    format!("{canonical_desc}\n\nSeverity Explanation:\n{severity_expl}")
                }
                (true, false) => canonical_desc.to_string(),
                (false, true) => severity_expl.to_string(),
                (false, false) => String::new(),
            };

            let mut source_files: Vec<String> = finding
                .get("affected_source_files")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .filter_map(sanitize_repo_relative_path)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if source_files.is_empty()
                && let Some(default_clean) = sanitize_repo_relative_path(&self.file_path)
            {
                source_files.push(default_clean.to_string());
            }

            let subsystems: Vec<crate::db::AttributedSubsystem> = maintainers
                .map(|mindex| {
                    mindex
                        .match_files(&source_files)
                        .into_iter()
                        .map(crate::db::AttributedSubsystem::from_maintainers)
                        .collect()
                })
                .unwrap_or_default();

            inputs.push(crate::api::BugInput {
                problem,
                reasoning,
                locations: finding.get("locations").cloned(),
                subsystems,
                source_files,
                commit_sha: Some(self.target_sha.clone()),
                patchset_id: None,
                patch_id: None,
                baseline_sha: Some(self.target_sha.clone()),
                review_id: None,
                min_severity: self.min_severity,
            });
        }
        inputs
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContractCartographerOutput {
    #[serde(default)]
    pub file_invariants: String,
    #[serde(default)]
    pub architecture_contract_notes: String,
}

struct ContractCartographerSession<'a> {
    file_path: &'a str,
    target_sha: &'a str,
    maintainers_hint: Option<&'a str>,
    maintainer_notes: &'a str,
    numbered_full_source: &'a str,
    external_headers: &'a str,
    external_callers: &'a str,
    tools: Option<std::sync::Arc<crate::toolbox::ToolBox>>,
    last_tool_call: Option<(String, Value)>,
    temperature: f32,
    effort: Option<crate::ai::ReasoningEffort>,
    context_tag: Option<String>,
}

#[async_trait::async_trait]
impl crate::ai::session::LlmSession for ContractCartographerSession<'_> {
    type Output = ContractCartographerOutput;

    fn system_prompt(&self) -> String {
        let maint = self
            .maintainers_hint
            .filter(|h| !h.trim().is_empty())
            .map(|h| format!("\nMAINTAINERS Subsystem(s): {h}"))
            .unwrap_or_default();
        let notes = if self.maintainer_notes.trim().is_empty() {
            String::new()
        } else {
            format!("\n\n{}", self.maintainer_notes.trim())
        };
        format!(
            "You are a Linux kernel Architecture & Design-Contract Cartographer (Agent 1) documenting `{}` at commit `{}`.{}{}\n\n\
            STRICT ROLE & CONTEXT ISOLATION:\n\
            - Do NOT hunt for bugs, do NOT speculate about defects, and do NOT rationalize questionable implementation lines.\n\
            - Your sole responsibility is to reconstruct and write down the NORMATIVE design contracts—how data structures, lifecycles, locks, pointers/sentinels, and caller/callee interfaces are SUPPOSED to work—derived from struct/type definitions, initialization/allocation sites, external callers (`<external_callers>`), vtable/ops registrations, docstrings, and majority patterns across sibling functions.\n\
            - CRITICAL ANTI-RATIONALIZATION RULE: Never treat an individual function's early return, missing NULL/bounds check, or lock omission as part of the intended specification unless an explicit comment or hardware/ABI rule justifies it. Explicitly distinguish base VM/device/map state (allocated unconditionally for every instance) from optional/lazy feature state (allocated only when an optional feature is enabled) when documenting teardown/flush/release hooks!\n\
            - `<full_file_source>`, `<external_header_definitions>`, and `<external_callers>` are already provided below; only invoke Git tools (`git_read_files` with `<= 150`-line ranges, or `git_grep`) if a critical external struct or caller contract is missing.",
            self.file_path, self.target_sha, maint, notes
        )
    }

    fn initial_user_prompt(&self) -> String {
        let ext_hdr = if self.external_headers.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<external_header_definitions>\n{}\n</external_header_definitions>",
                self.external_headers.trim()
            )
        };
        let ext_callers = if self.external_callers.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<external_callers>\n1-hop external call-sites and ops-table registrations invoking `{}`:\n{}\n</external_callers>",
                self.file_path,
                self.external_callers.trim()
            )
        };
        format!(
            r####"Study the complete numbered source of `{file_path}` at commit `{target_sha}` together with its external headers and external callers:

<full_file_source>
{full_source}
</full_file_source>{ext_hdr}{ext_callers}

Produce a JSON object with two fields:
1. `"file_invariants"` (concise bullet list, <= 2500 chars): high-signal lock->field rules, atomic/RCU/NMI/IRQ contexts, nullable pointer lifecycles, and base-vs-optional teardown obligations for downstream bug verifiers.
2. `"architecture_contract_notes"` (structured markdown specification, <= 8500 chars) organized into 4 sections for an isolated downstream Contract-Breakage Auditor (Agent 2):
   - `### 1. Data Structures & Lifecycles (Create -> Transitions / Recycling -> Teardown)`: For every key struct, array, XArray/hash/list, and per-CPU/fixmap state, list EVERY dynamically allocated sub-buffer, IRQ, timer, or delayed_work field alongside the exact teardown/error-unwind function that must free or cancel it; state whether each sub-structure exists unconditionally for every instance or lazily for optional features; document what fields recycled objects must preserve vs reset; and state the canonical predicate/flag that defines whether an object is active/linked in a scheduler/hash/list (versus secondary counters or lengths that can diverge).
   - `### 2. Synchronization, Lock-to-Field Protocol & Lock-Drop Windows`: Exact lock(s) required to read vs mutate/resize/reset each shared field or array, per-CPU/IRQ/NMI/RCU/migrate_disable requirements, and which loops/callees (`may_block`, `cond_resched*`, `gfp_t`) can drop and reacquire locks mid-function.
   - `### 3. Pointer Nullability Windows, Sentinels & Type/Value Domains`: Every pointer or index field that can be `NULL`, `-1`, `ERR_PTR`, or a negative sentinel (e.g., `S1_MMU_DISABLED`), the exact lifecycle windows in which it can be `NULL`/`-1` (before first-run init, after detach/unmap, during VM/device teardown), and parameter signedness/width/range contracts of internal helpers.
   - `### 4. External Caller Preconditions, Permission Dataflow & Sibling Symmetry`: Which external callers invoke each exported function (and under what locks/configurations), how multi-stage translations/faults must intersect permissions (e.g., upper-layer PTE write flags vs host/backing `writable` out-parameters and `xa_erase` return values), and invariants shared across sibling functions.

Return ONLY a valid JSON object matching:
```json
{{
  "file_invariants": "- ...",
  "architecture_contract_notes": "### 1. Data Structures & Lifecycles...\n..."
}}
```"####,
            file_path = self.file_path,
            target_sha = self.target_sha,
            full_source = self.numbered_full_source,
            ext_hdr = ext_hdr,
            ext_callers = ext_callers,
        )
    }

    fn tools(&self) -> Option<Vec<crate::ai::AiTool>> {
        self.tools.as_ref().map(|tb| tb.get_declarations_generic())
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> anyhow::Result<Value> {
        let Some(ref tb) = self.tools else {
            anyhow::bail!("No toolbox configured for ContractCartographerSession");
        };
        let repeated = self
            .last_tool_call
            .as_ref()
            .is_some_and(|last| last.0 == name && last.1 == args);
        if repeated {
            tracing::warn!("Blocked duplicate tool call: {} with args {:?}", name, args);
            return Ok(json!({
                "error": "Duplicate tool call blocked. Please change parameters or use a different tool."
            }));
        }
        self.last_tool_call = Some((name.to_string(), args.clone()));
        match tb.call(name, args).await {
            Ok(v) => Ok(v),
            Err(e) => Ok(json!({ "error": e.to_string() })),
        }
    }

    async fn call_tools(
        &mut self,
        calls: Vec<crate::ai::ToolCall>,
    ) -> anyhow::Result<Vec<(String, Value)>> {
        let Some(ref tb) = self.tools else {
            anyhow::bail!("No toolbox configured for ContractCartographerSession");
        };
        let mut results: Vec<Option<(String, Value)>> = vec![None; calls.len()];
        let mut to_run = Vec::new();

        for (idx, call) in calls.into_iter().enumerate() {
            let repeated = self
                .last_tool_call
                .as_ref()
                .is_some_and(|last| last.0 == call.function_name && last.1 == call.arguments);
            if repeated {
                tracing::warn!(
                    "Blocked duplicate tool call: {} with args {:?}",
                    call.function_name,
                    call.arguments
                );
                results[idx] = Some((
                    call.id,
                    json!({
                        "error": "Duplicate tool call blocked. Please change parameters or use a different tool."
                    }),
                ));
            } else {
                self.last_tool_call = Some((call.function_name.clone(), call.arguments.clone()));
                to_run.push((idx, call));
            }
        }

        let futures = to_run.into_iter().map(|(idx, call)| {
            let tools = tb.clone();
            async move {
                let res = match tools.call(&call.function_name, call.arguments).await {
                    Ok(v) => v,
                    Err(e) => json!({ "error": e.to_string() }),
                };
                (idx, (call.id, res))
            }
        });
        for (idx, res) in futures_util::future::join_all(futures).await {
            results[idx] = Some(res);
        }

        Ok(results.into_iter().flatten().collect())
    }

    fn temperature(&self) -> Option<f32> {
        Some(self.temperature)
    }

    fn reasoning_effort(&self) -> Option<crate::ai::ReasoningEffort> {
        self.effort
    }

    fn response_format(&self) -> Option<crate::ai::AiResponseFormat> {
        Some(crate::ai::AiResponseFormat::Json { schema: None })
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(
        &mut self,
        response: &crate::ai::AiResponse,
    ) -> std::result::Result<Self::Output, crate::ai::session::ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        let parsed: ContractCartographerOutput =
            crate::workflow::output::parse_json_from_text(text)
                .map_err(crate::ai::session::ValidationError::FormatViolation)?;
        if parsed.file_invariants.trim().is_empty()
            || parsed.architecture_contract_notes.trim().is_empty()
        {
            return Err(crate::ai::session::ValidationError::FormatViolation(
                "Both 'file_invariants' and 'architecture_contract_notes' must be non-empty strings."
                    .to_string(),
            ));
        }
        Ok(parsed)
    }
}

/// Runs Agent 1 (`ContractCartographer`): a context-isolated specification pass that
/// documents normative data-structure lifecycles, locking protocols, nullability/sentinel
/// domains, and external caller contracts without hunting for bugs.
#[allow(clippy::too_many_arguments)]
pub async fn run_contract_cartographer_pass(
    provider: &dyn crate::ai::AiProvider,
    tools: Option<std::sync::Arc<crate::toolbox::ToolBox>>,
    file_path: &str,
    target_sha: &str,
    maintainers_hint: Option<&str>,
    maintainer_notes: &str,
    numbered_full_source: &str,
    external_headers: &str,
    external_callers: &str,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> anyhow::Result<crate::ai::session::SessionResult<ContractCartographerOutput>> {
    let max_turns = if tools.is_some() { 3 } else { 1 };
    let runner = crate::ai::session::SessionRunner::new(provider).with_max_turns(max_turns);
    let mut session = ContractCartographerSession {
        file_path,
        target_sha,
        maintainers_hint,
        maintainer_notes,
        numbered_full_source,
        external_headers,
        external_callers,
        tools,
        last_tool_call: None,
        temperature,
        effort: effort.map(crate::ai::EffortMode::verification_stage_effort),
        context_tag: Some(format!("[scan:file-cartographer:{file_path}] ")),
    };
    runner.run(&mut session).await
}

pub fn build_linux_scan_workflow() -> Workflow<LinuxScanState> {
    build_linux_scan_workflow_with_options(20, 0.0)
}

pub fn build_linux_scan_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<LinuxScanState> {
    build_linux_scan_workflow_with_effort(max_turns, temperature, None)
}

pub fn build_linux_scan_workflow_with_effort(
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Workflow<LinuxScanState> {
    let analysis_max_turns = max_turns.min(4);
    Workflow::builder("linux_scan")
        .dynamic_parallel(
            scan_triage_stage_with_effort(temperature, effort),
            move |state| {
                resolve_scan_analysis_stages_with_options(state, analysis_max_turns, temperature)
            },
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty() && s.all_dismissed_concerns.is_empty(),
            "No concerns or dismissed concerns raised in scan specialist stages",
        )
        .stage(scan_verification_stage_with_effort(1, temperature, effort))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_stage_names_are_unique_and_resolvable() {
        let mut seen = std::collections::BTreeSet::new();
        for def in SCAN_ANALYSIS_STAGES {
            assert!(seen.insert(def.name), "duplicate stage name {}", def.name);
            assert!(is_known_scan_stage(def.name));
            assert!(std::ptr::eq(
                scan_analysis_stage_by_name(def.name).unwrap(),
                def
            ));
        }
        assert!(!is_known_scan_stage("pre-screen"));
        assert!(is_known_scan_stage("triage"));
        assert!(is_known_scan_stage("verification"));
        assert!(is_known_scan_stage("post-verification-1"));
        assert!(!is_known_scan_stage("origin-tracing"));
        assert!(!is_known_scan_stage("report-1"));
        assert!(!is_known_scan_stage("nonexistent"));
    }

    #[test]
    fn test_scan_stage_guides_exist_in_linux_prompt_tree() {
        let prompts_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("projects")
            .join("linux")
            .join("prompts");
        for def in SCAN_ANALYSIS_STAGES {
            for guide in def.guides {
                let path = prompts_root.join(guide);
                assert!(
                    path.is_file(),
                    "scan stage {} references missing prompt file {}",
                    def.name,
                    path.display()
                );
            }
        }
    }

    #[test]
    fn test_validate_canonical_bug_title_enforces_bugrepo_rules() {
        assert!(
            validate_canonical_bug_title(
                "smb: server: memory leak in parse_durable_handle_context()"
            )
            .is_ok()
        );
        assert!(
            validate_canonical_bug_title("rust: sync: use-after-free in Arc::from_raw()").is_ok()
        );
        assert!(
            validate_canonical_bug_title("rust: sync: fix use-after-free in Arc::from_raw()")
                .is_err()
        );
        assert!(validate_canonical_bug_title("Arc::from_raw() use-after-free").is_err());
        assert!(validate_canonical_bug_title("smb:: memory leak in foo()").is_err());
        assert!(validate_canonical_bug_title("missing colon in title").is_err());
        assert!(validate_canonical_bug_title("smb: server: ").is_err());
        assert!(validate_canonical_bug_title("smb: : memory leak in foo()").is_err());
        assert!(validate_canonical_bug_title("smb: `backticks` in title()").is_err());
        assert!(
            validate_canonical_bug_title("smb: fix memory leak in parse_durable_handle_context()")
                .is_err()
        );
        assert!(
            validate_canonical_bug_title(
                "smb: server: fix memory leak in parse_durable_handle_context()"
            )
            .is_err()
        );
        assert!(
            validate_canonical_bug_title("smb: prevent use-after-free in foo_release()").is_err()
        );
        let too_long = format!("smb: {}", "a".repeat(80));
        assert!(validate_canonical_bug_title(&too_long).is_err());
    }

    #[test]
    fn test_sanitize_repo_relative_path_rejects_flags_and_traversal() {
        assert_eq!(
            sanitize_repo_relative_path("./fs/smb/server/smb2pdu.c"),
            Some("fs/smb/server/smb2pdu.c")
        );
        assert_eq!(sanitize_repo_relative_path("--output=/tmp/pwn"), None);
        assert_eq!(sanitize_repo_relative_path("-f"), None);
        assert_eq!(sanitize_repo_relative_path("/etc/passwd"), None);
        assert_eq!(sanitize_repo_relative_path("../secret.c"), None);
        assert_eq!(sanitize_repo_relative_path("fs/../secret.c"), None);
        assert_eq!(sanitize_repo_relative_path(""), None);
    }

    #[test]
    fn test_validate_scan_triage_output_with_effort_requires_stage_efforts() {
        let state = LinuxScanState::default();
        let empty_early_exit = ScanTriageOutput {
            relevant_stages: vec![],
            focus_targets: vec![],
            stage_efforts: std::collections::BTreeMap::new(),
        };
        assert!(validate_scan_triage_output_with_effort(&empty_early_exit, &state).is_ok());

        let missing_efforts = ScanTriageOutput {
            relevant_stages: vec!["resources".to_string(), "locking".to_string()],
            focus_targets: vec![],
            stage_efforts: std::collections::BTreeMap::from([(
                "resources".to_string(),
                "high".to_string(),
            )]),
        };
        assert!(validate_scan_triage_output(&missing_efforts, &state).is_ok());
        assert!(validate_scan_triage_output_with_effort(&missing_efforts, &state).is_err());

        let complete_efforts = ScanTriageOutput {
            relevant_stages: vec!["resources".to_string(), "locking".to_string()],
            focus_targets: vec![],
            stage_efforts: std::collections::BTreeMap::from([
                ("resources".to_string(), "high".to_string()),
                ("locking".to_string(), "medium".to_string()),
            ]),
        };
        assert!(validate_scan_triage_output_with_effort(&complete_efforts, &state).is_ok());
    }

    #[test]
    fn test_validate_canonical_bug_description_requires_sections() {
        let valid = "Trigger / Preconditions: Invalid sharename.\nCall Chain: smb2_open() -> parse_durable_handle_context().\nFailure Mechanism: Returns rc without freeing fp.\nImpact: Kernel memory leak.";
        assert!(validate_canonical_bug_description(valid).is_ok());
        assert!(validate_canonical_bug_description("Missing sections").is_err());
    }

    #[test]
    fn test_validate_scan_concerns_and_findings_do_not_require_diff_side() {
        let state = LinuxScanState::default();
        let concerns_out = StageConcernsOutput {
            concerns: vec![json!({
                "type": "Resource Leak",
                "description": "Leaks buf on early return",
                "reasoning": "Allocated at line 100 and not freed before return -EINVAL",
                "preexisting": true,
                "locations": [{
                    "file": "fs/smb/server/smb2pdu.c",
                    "function_or_symbol": "smb2_ioctl",
                    "line": 120,
                    "code_snippet": "return -EINVAL;"
                }]
            })],
            dismissed_concerns: vec![json!({
                "type": "Locking",
                "description": "Potential unlocked read",
                "reasoning": "Guarded by session_lock in caller",
                "locations": [{
                    "file": "fs/smb/server/smb2pdu.c",
                    "function_or_symbol": "smb2_process",
                    "line": 80,
                    "code_snippet": "mutex_lock(&sess->session_lock);"
                }]
            })],
        };
        assert!(validate_scan_concerns_output(&concerns_out, &state).is_ok());

        let finding = json!({
            "problem": "smb: memory leak in smb2_ioctl() on error path",
            "canonical_description": "Trigger: Invalid req.\nFailure Mechanism: Early return skips kfree.\nImpact: Kernel memory leak.",
            "severity": "High",
            "severity_explanation": "Unbounded memory leak reachable from client.",
            "locations": [{
                "file": "fs/smb/server/smb2pdu.c",
                "function_or_symbol": "smb2_ioctl",
                "line": 120,
                "code_snippet": "return -EINVAL;"
            }]
        });
        assert!(validate_scan_finding_object(&finding, 0).is_ok());
    }

    #[test]
    fn test_record_verified_scan_findings_retains_medium_plus_and_tracks_low_dismissal() {
        let mut state = LinuxScanState {
            file_path: "fs/smb/server/smb2pdu.c".to_string(),
            target_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            ..Default::default()
        };
        let findings = vec![
            json!({
                "id": "VF1",
                "problem": "smb: comment typo in foo()",
                "canonical_description": "Trigger: x\nFailure Mechanism: y\nImpact: z",
                "severity": "Low",
                "severity_explanation": "Low hygiene issue",
                "locations": [{"file": "fs/smb/server/smb2pdu.c", "function_or_symbol": "foo", "code_snippet": "x();"}]
            }),
            json!({
                "id": "VF2",
                "problem": "smb: error-path leak in baz()",
                "canonical_description": "Trigger: x\nFailure Mechanism: y\nImpact: z",
                "severity": "Medium",
                "severity_explanation": "Medium error-path leak",
                "locations": [{"file": "fs/smb/server/smb2pdu.c", "function_or_symbol": "baz", "code_snippet": "return -ENOMEM;"}]
            }),
            json!({
                "id": "VF3",
                "problem": "smb: use-after-free in bar()",
                "canonical_description": "Trigger: x\nFailure Mechanism: y\nImpact: z",
                "severity": "High",
                "severity_explanation": "High UAF",
                "locations": [{"file": "fs/smb/server/smb2pdu.c", "function_or_symbol": "bar", "code_snippet": "kfree(p);"}]
            }),
        ];
        record_verified_scan_findings(&mut state, findings);
        assert_eq!(state.findings.len(), 2);
        assert_eq!(
            state.findings[0]["problem"],
            "smb: error-path leak in baz()"
        );
        assert_eq!(state.findings[1]["problem"], "smb: use-after-free in bar()");
        assert_eq!(
            state.findings[1]["affected_source_files"],
            json!(["fs/smb/server/smb2pdu.c"])
        );
        assert_eq!(state.deduplicated_dismissed_concerns.len(), 1);
        assert_eq!(
            state.deduplicated_dismissed_concerns[0]["origin"],
            "scan_severity_filter"
        );

        let inputs = state.to_candidate_bug_inputs(None);
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[1].problem, "smb: use-after-free in bar()");
        assert!(inputs[1].reasoning.contains("Failure Mechanism: y"));
        assert!(inputs[1].reasoning.contains("High UAF"));
        assert_eq!(
            inputs[1].source_files,
            vec!["fs/smb/server/smb2pdu.c".to_string()]
        );
        assert_eq!(
            inputs[1].commit_sha.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn test_scan_security_and_verification_render_kernel_threat_model() {
        let model = crate::threat_model::KernelThreatModel {
            text: "Guaranteed boundary: unprivileged local users.".to_string(),
            commit: "05441d697f7f0000000000000000000000000000".to_string(),
            git_ref: "linus/master".to_string(),
        };
        let state_with = LinuxScanState {
            kernel_threat_model: Some(model),
            ..Default::default()
        };
        let state_without = LinuxScanState::default();

        let sec_def = scan_analysis_stage_by_name("security").unwrap();
        let sec_with = scan_analysis_stage_user_prompt(sec_def).render_for_log(&state_with);
        assert!(sec_with.contains(
            "<kernel_threat_model source=\"Documentation/process/threat-model.rst @ linus/master (05441d697f7f)\">"
        ));
        assert!(!sec_with.contains("{{kernel_threat_model_section}}"));

        let sec_without = scan_analysis_stage_user_prompt(sec_def).render_for_log(&state_without);
        assert!(!sec_without.contains("<kernel_threat_model source="));
        assert!(!sec_without.contains("{{kernel_threat_model_section}}"));

        for non_sec in [
            "implementation",
            "execution-flow",
            "resources",
            "locking",
            "hardware",
        ] {
            let def = scan_analysis_stage_by_name(non_sec).unwrap();
            let rendered = scan_analysis_stage_user_prompt(def).render_for_log(&state_with);
            assert!(
                !rendered.contains("<kernel_threat_model source="),
                "{non_sec} must not include <kernel_threat_model>"
            );
        }

        let sev_section = scan_kernel_threat_model_severity_section(&state_with);
        assert!(sev_section.contains("<kernel_threat_model source="));
        assert!(scan_kernel_threat_model_severity_section(&state_without).is_empty());
    }

    #[test]
    fn test_triage_empty_relevant_stages_resolves_zero_specialist_stages() {
        let state = LinuxScanState::default();
        let stages = resolve_scan_analysis_stages_with_options(&state, 10, 0.0);
        assert!(stages.is_empty());
    }

    #[test]
    fn test_linux_scan_system_prompt_interpolates_notes_and_maintainers_hint() {
        let state = LinuxScanState {
            chunk_id: "fs/smb/server/smb2pdu.c:1-200".to_string(),
            file_path: "fs/smb/server/smb2pdu.c".to_string(),
            target_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            start_line: 1,
            end_line: 200,
            maintainers_hint: Some(
                "COMMON INTERNET FILE SYSTEM CLIENT (CIFS and SMB3)".to_string(),
            ),
            maintainer_notes: "\n\n<maintainer_notes>\nSMB rule\n</maintainer_notes>".to_string(),
            code_notes: "\n\n<code_notes>\nSMB note\n</code_notes>".to_string(),
            ..Default::default()
        };
        let rendered = linux_scan_system_prompt().render_for_log(&state);
        assert!(
            rendered.contains("MAINTAINERS Subsystem(s): COMMON INTERNET FILE SYSTEM CLIENT"),
            "rendered system prompt should contain interpolated maintainers_hint: {rendered}"
        );
        let m_pos = rendered.find("<maintainer_notes>").unwrap();
        let c_pos = rendered.find("<code_notes>").unwrap();
        assert!(
            m_pos < c_pos,
            "maintainer_notes must precede code_notes: {rendered}"
        );
        assert!(!rendered.contains("{{maintainers_block}}"));
        assert!(!rendered.contains("{{maintainer_notes}}"));
        assert!(!rendered.contains("{{code_notes}}"));
    }

    #[test]
    fn test_validate_scan_post_verification_batch_items_accepts_finding_without_preexisting() {
        let batch = vec![json!({
            "id": "I1",
            "type": "Resource Leak",
            "description": "Potential leak on error path",
            "estimated_severity": "High",
            "signal_reason": "cross_file",
            "concern_arguments": "Allocated buffer not freed",
            "dismissal_arguments": null,
            "verification_question": "Does caller free buf on error?",
            "locations": [{
                "file": "fs/smb/server/smb2pdu.c",
                "function_or_symbol": "smb2_ioctl()",
                "line": 120,
                "code_snippet": "return -EINVAL;",
                "why_this_location_matters": "Early return"
            }]
        })];
        let output = PostVerificationOutput {
            findings: vec![json!({
                "source_ids": ["I1"],
                "problem": "smb: memory leak in smb2_ioctl() on error path",
                "canonical_description": "Trigger / Preconditions: Invalid ioctl request.\nCall Chain: smb2_ioctl().\nFailure Mechanism: Early return skips kfree(buf).\nImpact: Unbounded kernel memory leak.",
                "severity": "High",
                "severity_explanation": "Unprivileged client can trigger repeated allocations.",
                "affected_source_files": ["fs/smb/server/smb2pdu.c"],
                "locations": [{
                    "file": "fs/smb/server/smb2pdu.c",
                    "function_or_symbol": "smb2_ioctl()",
                    "line": 120,
                    "code_snippet": "return -EINVAL;",
                    "why_this_location_matters": "Returns without freeing buf"
                }]
            })],
            dismissed_concerns: vec![],
        };
        assert!(validate_scan_post_verification_batch_items(&output, &batch).is_ok());
    }

    #[test]
    fn test_validate_scan_verification_output_rejects_empty_output_when_inputs_lack_ids() {
        let state = LinuxScanState {
            all_concerns: vec![json!({
                "type": "Resource Leak",
                "description": "Unchecked leak",
                "reasoning": "Missing kfree",
                "locations": [{"file": "a.c", "function_or_symbol": "foo", "code_snippet": "return -1;"}]
            })],
            ..Default::default()
        };
        let empty_out = VerificationOutput {
            findings: vec![],
            inconclusive_concerns: vec![],
            dismissed_ids: vec![],
            dismissed_concerns: vec![],
        };
        let err = validate_scan_verification_output(&empty_out, &state).unwrap_err();
        assert!(err.contains("At least one concern was raised"));
    }

    #[test]
    fn test_record_verified_scan_findings_respects_low_floor() {
        let low_finding = json!({
            "problem": "minor counter drift",
            "severity": "Low",
            "severity_explanation": "cosmetic stats counter",
            "locations": []
        });

        let mut default_state = LinuxScanState {
            file_path: "mm/memcontrol.c".to_string(),
            ..Default::default()
        };
        record_verified_scan_findings(&mut default_state, vec![low_finding.clone()]);
        assert!(default_state.findings.is_empty());
        assert_eq!(default_state.deduplicated_dismissed_concerns.len(), 1);

        let mut low_floor_state = LinuxScanState {
            file_path: "mm/memcontrol.c".to_string(),
            min_severity: Some(crate::db::SeverityFloor::Low),
            ..Default::default()
        };
        record_verified_scan_findings(&mut low_floor_state, vec![low_finding]);
        assert_eq!(low_floor_state.findings.len(), 1);
    }

    #[test]
    fn test_dynamic_effort_scan_triage_schema_and_reducer_support_early_exit_and_core_stages() {
        let schema = build_scan_stage_efforts_schema(SCAN_ANALYSIS_STAGES);
        assert_eq!(schema["type"], "object");
        assert!(
            schema.get("required").is_none(),
            "scan stage_efforts schema must not require keys so early-exit can emit {{}}"
        );
        let props = schema["properties"].as_object().unwrap();
        for def in SCAN_ANALYSIS_STAGES {
            assert!(props.contains_key(def.name));
        }

        let mut early_exit_state = LinuxScanState::default();
        let early_exit_out = ScanTriageOutput {
            relevant_stages: vec![],
            focus_targets: vec![],
            stage_efforts: std::collections::BTreeMap::new(),
        };
        assert!(
            validate_scan_triage_output_with_effort(&early_exit_out, &early_exit_state).is_ok()
        );
        apply_scan_triage_output(true, early_exit_out, &mut early_exit_state);
        assert!(early_exit_state.planned_stages.is_empty());
        assert!(early_exit_state.planned_efforts.is_empty());

        let mut active_state = LinuxScanState::default();
        let active_out = ScanTriageOutput {
            relevant_stages: vec!["security".to_string()],
            focus_targets: vec![],
            stage_efforts: std::collections::BTreeMap::from([(
                "security".to_string(),
                "high".to_string(),
            )]),
        };
        assert!(validate_scan_triage_output_with_effort(&active_out, &active_state).is_ok());
        apply_scan_triage_output(true, active_out, &mut active_state);
        assert_eq!(
            active_state.planned_stages,
            vec![
                "implementation",
                "execution-flow",
                "resources",
                "locking",
                "security",
            ]
        );
        assert_eq!(
            active_state.planned_efforts.get("security"),
            Some(&crate::ai::ReasoningEffort::High)
        );
        assert_eq!(
            active_state.planned_efforts.get("implementation"),
            Some(&crate::ai::ReasoningEffort::Medium)
        );
    }

    #[test]
    fn test_contract_audit_context_isolation_and_stage_resolution() {
        let state = LinuxScanState {
            chunk_id: "arch/arm64/kvm/nested.c:817-1170".to_string(),
            file_path: "arch/arm64/kvm/nested.c".to_string(),
            target_sha: "08f97454b7fa39bfcf82524955c0000000000000".to_string(),
            start_line: 817,
            end_line: 1170,
            contract_notes: "### 1. Canonical Stage-2 MMU must be torn down unconditionally"
                .to_string(),
            external_callers: "virt/kvm/kvm_main.c:1200: kvm_arch_flush_shadow_all(kvm);"
                .to_string(),
            focus_targets: vec![ScanFocusTarget {
                function: "invalidate_vncr_va".to_string(),
                lines: "920-940".to_string(),
                risk_category: "locking".to_string(),
                hypothesis: "check vncr_tlb".to_string(),
                callers_or_callees_to_check: vec![],
            }],
            planned_stages: vec!["implementation".to_string(), "resources".to_string()],
            ..Default::default()
        };

        let track_a = linux_scan_system_prompt().render_for_log(&state);
        assert!(
            !track_a.contains("<architecture_contract_notes>"),
            "Track A domain specialists must remain isolated from Agent 1 contract notes"
        );
        assert!(track_a.contains("<triage_focus_targets>"));

        let track_b = linux_contract_audit_system_prompt().render_for_log(&state);
        assert!(track_b.contains("<architecture_contract_notes>"));
        assert!(track_b.contains("Canonical Stage-2 MMU must be torn down unconditionally"));
        assert!(track_b.contains("<external_callers>"));
        assert!(
            !track_b.contains("<triage_focus_targets>"),
            "Agent 2 contract-audit must not be anchored by Track A triage focus_targets"
        );

        let stages = resolve_scan_analysis_stages_with_options(&state, 4, 0.0);
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].name(), "contract-audit");

        let mut early_exit_state = state.clone();
        early_exit_state.planned_stages.clear();
        assert!(resolve_scan_analysis_stages_with_options(&early_exit_state, 4, 0.0).is_empty());
    }

    #[tokio::test]
    async fn test_contract_cartographer_session_duplicate_guard_and_error_json() {
        use crate::ai::session::LlmSession;
        let tmp = tempfile::tempdir().unwrap();
        let tb = std::sync::Arc::new(crate::toolbox::ToolBox::new(tmp.path().to_path_buf()));
        let mut session = ContractCartographerSession {
            file_path: "arch/arm64/kvm/nested.c",
            target_sha: "08f97454b7fa39bfcf82524955c771d2d693d6fe",
            maintainers_hint: None,
            maintainer_notes: "",
            numbered_full_source: "1: int f(void) { return 0; }",
            external_headers: "",
            external_callers: "",
            tools: Some(tb),
            last_tool_call: None,
            temperature: 0.0,
            effort: None,
            context_tag: None,
        };

        let call = crate::ai::ToolCall {
            id: "c1".to_string(),
            function_name: "nonexistent_tool".to_string(),
            arguments: json!({"path": "foo.c"}),
            thought_signature: None,
        };
        let dup_call = crate::ai::ToolCall {
            id: "c2".to_string(),
            function_name: "nonexistent_tool".to_string(),
            arguments: json!({"path": "foo.c"}),
            thought_signature: None,
        };
        let results = session.call_tools(vec![call, dup_call]).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results[0].1.get("error").is_some(),
            "unknown tool must return JSON error object rather than aborting session"
        );
        assert!(
            results[1]
                .1
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("Duplicate tool call blocked")),
            "consecutive identical tool call must be blocked"
        );
    }
}
