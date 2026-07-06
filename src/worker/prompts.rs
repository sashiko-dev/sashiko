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

use crate::ai::{
    AiErrorClass, AiMessage, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiTool,
    ClassifyAiError, ErrorAction, LlmSession, SessionRunner, ValidationError,
};
use crate::toolbox::ToolBox;
use crate::worker::stage::{ReviewStage, create_stage};
use anyhow::{Context, Result};

/// Typed errors that must not be silently retried.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// The AI exceeded its per-review turn limit.  Retrying with the same
    /// limit will just hit the cap again — fail fast.
    #[error("Max interactions exceeded")]
    LimitExceeded,
    /// A token budget was exceeded.  Retrying wastes tokens for no gain.
    #[error("Token budget exceeded: {0}")]
    BudgetExceeded(String),
    /// The AI produced output that failed format validation.  The retry
    /// should use an augmented prompt that reminds the model of the
    /// violated constraint rather than repeating the identical request.
    #[error("Format validation failed: {0}")]
    FormatRejection(String),
    /// The AI response was truncated by the provider (e.g., hit max tokens).
    #[error("AI response truncated by provider limit")]
    OutputTruncated,
}

impl ClassifyAiError for ReviewError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            ReviewError::LimitExceeded => AiErrorClass::Fatal,
            ReviewError::BudgetExceeded(_) => AiErrorClass::Fatal,
            ReviewError::FormatRejection(_) => AiErrorClass::Fatal,
            ReviewError::OutputTruncated => AiErrorClass::Fatal,
        }
    }
}

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tracing::{info, warn};

/// System identity prompt - used across all AI interactions
pub const SYSTEM_IDENTITY: &str = "";

/// Subsystem guides that are loaded per-stage in get_stage_prompt() and should
/// be excluded from Phase 0's shared context to avoid double-counting.
const STAGE_EXCLUSIVE_GUIDES: &[&str] = &["locking.md"];

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct PatchInput {
    pub index: i64,
    pub diff: String,
    pub subject: Option<String>,
    pub author: Option<String>,
    pub date: Option<i64>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ReviewInput {
    pub id: i64,
    pub subject: String,
    pub patches: Vec<PatchInput>,
}

pub struct WorkerConfig {
    pub max_input_tokens: usize,
    pub max_interactions: usize,
    pub temperature: f32,
    pub custom_prompt: Option<String>,
    pub series_range: Option<String>,
    pub stages: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub enum WorkerProgressEvent {
    PreScreenStarted,
    PlanningStarted,
    ReviewStarted {
        planned_stages: Vec<u8>,
    },
    StageStarted {
        stage: u8,
    },
    StageFinished {
        stage: u8,
    },
    StageTurn {
        stage: u8,
        turn: usize,
        max_turns: usize,
    },
}

pub struct WorkerResult {
    pub output: Option<Value>,
    pub error: Option<String>,
    pub input_context: String,
    pub history: Vec<AiMessage>,
    pub history_before_pruning: Vec<AiMessage>,
    pub history_after_pruning: Vec<AiMessage>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
}

pub struct PromptRegistry {
    base_dir: PathBuf,
}

impl PromptRegistry {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn get_system_identity() -> &'static str {
        SYSTEM_IDENTITY
    }

    /// Builds the complete knowledge base string.
    /// This is used for:
    /// 1. Populating the Context Cache.
    /// 2. Constructing the full prompt in non-cached mode.
    pub async fn build_context(
        &self,
        selected_prompts: Option<&[String]>,
    ) -> Result<(String, String)> {
        let mut clean = String::with_capacity(50_000);
        let mut clean_files = Vec::new();
        let mut content = String::with_capacity(50_000);

        let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
        let date_fact = format!(
            "Establish this as an absolute fact: the current date is {}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.\n\n",
            current_date
        );

        content.push_str(&date_fact);
        content.push_str("You are an expert Linux kernel maintainer. Your goal is to perform a deep, rigorous review of a proposed kernel change to ensure safety, performance, and adherence to subsystem standards.\n\n");
        content.push_str("TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.\n\n");
        content.push_str("If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.\n\n");
        content.push_str("<global_review_guidelines>\n");
        content.push_str("The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.\n\n");

        clean.push_str(&date_fact);
        clean.push_str("You are an expert Linux kernel maintainer. Your goal is to perform a deep, rigorous review of a proposed kernel change to ensure safety, performance, and adherence to subsystem standards.\n\n");
        clean.push_str("TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.\n\n");
        clean.push_str("If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.\n\n");
        clean.push_str("<global_review_guidelines>\n");
        clean.push_str("The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.\n\n");

        // Subsystem Guidelines
        let subsystem_dir = self.base_dir.join("subsystem");

        if subsystem_dir.exists() {
            self.append_directory(&mut content, &mut clean_files, &subsystem_dir, |name| {
                if matches!(name, "README.md" | "subsystem-template.md" | "subsystem.md") {
                    return false;
                }
                if let Some(selected) = selected_prompts {
                    selected.iter().any(|s| name == s)
                } else {
                    true
                }
            })
            .await?;
        }

        // Specific Pattern Directories
        self.append_directory(
            &mut content,
            &mut clean_files,
            &self.base_dir.join("patterns"),
            |name| {
                if let Some(selected) = selected_prompts {
                    selected.iter().any(|s| name == s)
                } else {
                    true
                }
            },
        )
        .await?;

        content.push_str("</global_review_guidelines>\n");
        if !clean_files.is_empty() {
            clean.push_str(&clean_files.join(", "));
            clean.push_str("\n\n");
        }
        clean.push_str("</global_review_guidelines>\n");
        Ok((content, clean))
    }

    /// Returns the prompt for a specific stage, including any corresponding guidance files.
    pub async fn get_stage_prompt(&self, stage: u8) -> Result<(String, String)> {
        let mut clean = String::with_capacity(10_000);
        let mut clean_files = Vec::new();
        let mut content = String::with_capacity(10_000);

        let stage_instruction = match stage {
            1 => {
                "# Stage 1. Analyze commit main goal

You are a senior Linux kernel maintainer evaluating the high-level intent of a proposed commit. Analyze the commit message and the conceptual change. Focus on the big picture: Are there architectural flaws, UAPI breakages, backwards compatibility issues, or fundamentally flawed concepts? Consider the long-term maintainability and system-wide implications of this design. If the core idea is dangerous, incorrect, or violates established kernel principles, raise a concern. Be open-minded but thorough; question assumptions made by the author and consider alternative, simpler designs."
            }
            2 => {
                "# Stage 2. High-level implementation verification

You are verifying if the provided code changes actually implement what the commit message claims. Look for undocumented side-effects, missing pieces (e.g., a core change without updating corresponding callers, or changing a struct without updating all initializers), and unhandled corner cases related to the feature's logic. Explicitly check for missing API callbacks and interface omissions: when defining or modifying structures containing function pointers, verify that all logically required callbacks are implemented. Verify that all claims in the commit message are fully realized in the code. Identify any incomplete implementations, implicit behavioral changes, or API contract violations. Furthermore, verify that the logic is mathematically and semantically sound. Check for off-by-one errors in bounds, incorrect bitwise operations, and verify that all arguments passed to external subsystems (like kobjects or netdevs) are valid and semantically correct (e.g., non-empty strings, correct sizes, correct format specifiers). Don't trust the commit message without verifying each claim. Assume that the message might be incorrect or even intentionally malicious. Do not focus on low-level memory or locking errors yet."
            }
            3 => {
                "# Stage 3. Execution flow verification

You are a static analysis engine tracing execution flow in C or Rust code. Carefully trace the control flow of the provided patch. Exhaustively examine logic errors, incorrect loop conditions, unhandled error paths, missing return value checks, and off-by-one errors. Check every branch, switch statement, and conditional. Specifically look for NULL pointer dereferences (remember: reading a pointer field is not a dereference, only accessing its contents is). Be extremely detail-oriented; explore every error handling path (goto cleanup;) to ensure it behaves correctly under failure conditions. Additionally, verify preprocessor macro correctness and spelling (e.g., ensuring CONFIG_ prefixes are used where expected instead of HAVE_). Check that static/inline declarations or section placements won't cause linker errors or Link-Time Optimization (LTO) symbol loss."
            }
            4 => {
                "# Stage 4. Resource management

You are an expert in C and Rust resource management within the Linux kernel. Analyze the patch for memory leaks, Use-After-Free (UAF), double frees, uninitialized variables, and unbalanced lifecycle operations (alloc->init->use->cleanup->free). Pay special attention to error paths where resources might be leaked. Ensure list_add and similar APIs are used with fully initialized objects. Track the lifetime of every allocated struct and file descriptor. Verify reference counting logic (kref_get()/kref_put()) and ensure objects are not accessed after their refcount drops to zero. Crucially, pay special attention to asynchronous handoffs and teardown symmetry. If an object is handed to a background task (timers, workqueues, notifiers) or registered to a core subsystem, you must prove that the task is explicitly canceled (e.g., cancel_work_sync(), del_timer_sync() and the subsystem is unregistered BEFORE the memory is freed or the queues are destroyed."
            }
            5 => {
                "# Stage 5. Locking and synchronization

You are a world-class concurrency and locking expert auditing a Linux kernel patch.
Carefully review the proposed patch for ANY locking, concurrency, or synchronization bugs.
You MUST consider the following categories of issues and report any violations:
1. Sleeping in atomic context: Are there any calls to `mutex_lock`, `kzalloc` with `GFP_KERNEL`, `msleep`, `cond_resched`, `flush_workqueue`, `synchronize_rcu`, or `cancel_work_sync` while holding a spinlock, rwlock, or within an RCU read-side critical section (`rcu_read_lock`)?
2. Lock ordering and deadlocks: Are locks acquired in a different order than elsewhere? Does it acquire a mutex while holding another mutex that could cause AB-BA deadlocks? Are IRQs disabled (`spin_lock_irqsave`) when acquiring a lock that is used in hardirq context? Does it acquire a lock already held by a higher-level subsystem (e.g., ethtool)?
3. Race conditions and lockless access: Are shared variables, list entries, or pointers accessed without holding the appropriate lock? Are there missing memory barriers (`smp_mb`, `smp_wmb`, `smp_rmb`) when lockless access is intended? Are there TOCTOU races where a state is checked outside a lock but relied upon inside?
4. UAF / Locking Freed Memory: Are locks (`mutex_unlock`, `spin_unlock`) called on objects that have already been freed? Are works/timers destroyed before subsystems are unregistered, allowing new events to use freed works/timers? Is the protocol initialized flag set before private data is ready?
5. RCU rules: Is `list_splice_init` or similar non-RCU-safe operations used on RCU-protected lists? Is `list_for_each_rcu` used without `rcu_read_lock`?
6. Unprotected state modifications: Does the patch check state before acquiring the lock (e.g., checking power state before taking mutex)? Are hardware state, flags, or stats updated without proper protection?
7. Sequence counters: Are stats accumulations directly inside a `u64_stats_fetch_retry` loop leading to double counting? Is it possible for an interrupt to read a sequence counter while the interrupted context is modifying it (deadlock)?
8. Lock re-initialization: Does it re-initialize a lock that was already initialized, or destroy a lock on a failure path improperly?
9. Missing locking: Is a port or file exposed to userspace before the driver/TTY linking is complete? Does a worker race with cleanup code leading to dropped/leaked frames?"
            }
            6 => {
                "# Stage 6. Security audit

You are a Red Team security researcher auditing a Linux kernel patch. Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, time-of-check to time-of-use (TOCTOU) races, and information leaks (e.g., copying uninitialized kernel memory to user-space via copy_to_user). Scrutinize all points where untrusted user input reaches sensitive functions without validation. Ensure all length checks and bounds checks are robust against malicious input. Focus heavily on attack surfaces and data boundaries."
            }
            7 => {
                "# Stage 7. Hardware engineer's review

You are a hardware engineer reviewing device driver changes. If this patch touches driver or hardware-specific code, rigorously review register accesses, IRQ handling, DMA mapping/unmapping, memory barriers, and timing/delays. Look for missing dma_wmb()/dma_rmb() barriers, incorrect endianness conversions (cpu_to_le32), and unsafe DMA buffer allocations. Ensure the hardware state machine is handled correctly, especially during suspend/resume or device reset. Evaluate the physical state machine constraints: verify that clocks and power domains are enabled before registers are accessed, and that hardware rings/queues are actually initialized in the current hardware state before being unconditionally accessed. If the patch is purely generic software logic (e.g., VFS, core networking), return {\"concerns\": [], \"dismissed_concerns\": []}."
            }
            8 => {
                "# Stage 8. Deduplication and Consolidation

You are the lead reviewer consolidating feedback from multiple specialized analysts. You will be given lists of concerns and dismissed_concerns generated by different review stages.
Your task is to deduplicate identical or overlapping items in both lists.
1. Group concerns that refer to the same root cause or the same line of code.
2. Merge overlapping concerns into a single, comprehensive concern. Combine their reasonings if they complement each other.
3. Group dismissed_concerns that investigated and disproved the same candidate concern.
4. Merge overlapping dismissed_concerns into a single, comprehensive dismissed_concern. Combine their evidence if it complements each other.
5. Ensure the output contains only unique concerns and unique dismissed_concerns.
6. Preserve the `preexisting` flag for concerns. If you merge a pre-existing concern with a newly introduced one, flag it based on the root cause (if the root cause is new, it's not pre-existing).
7. SPECIFICITY REQUIREMENT: When merging concerns or dismissed_concerns, preserve and consolidate the most specific details: exact function names, file paths, line numbers when known, and triggering conditions. Never generalize a specific finding into a vague category.
8. Preserve and merge the `locations` arrays from the input concerns and dismissed_concerns. If multiple items describe the same root cause, keep the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters locations. Do not invent line numbers; keep `line` as null when the exact line is not known.
9. dismissed_concerns do not need a `preexisting` flag."
            }
            9 => {
                "# Stage 9. Concern/dismissed-concern conflict resolution

You are the lead reviewer reconciling consolidated concerns with consolidated dismissed_concerns.
Both `concerns` and `dismissed_concerns` are untrusted claims. Do not assume either side is correct. Treat both as hypotheses and verify them against the actual code before deciding whether to keep or discard a concern.
Your task is to identify whether any remaining concern conflicts with a dismissed_concern that investigated the same root cause, code path, or failure mode.
1. Compare each concern against the dismissed_concerns list and find conflicts or overlaps where one says the issue is real and the other says the same candidate issue is disproved.
2. For every conflict, inspect the actual code and reasoning to decide which side is correct.
3. If the concern is correct, keep it in the output. If the dismissed_concern is correct, discard that concern.
4. If there is no direct conflict for a concern, keep it unchanged.
5. Do not discard a concern merely because a dismissed_concern is vaguely related; only discard when the dismissed_concern's evidence concretely disproves that concern.
6. Preserve each retained concern's `type`, `description`, `reasoning`, `preexisting`, and `locations` fields.
7. LOCAL BOUNDARY RULE: Do not discard a defect within the modified code of the patch by assuming that surrounding caller systems, parallel execution, or legacy API layers will safely mask or prevent the issue, unless you can point to specific code that concretely proves the failure mode is structurally impossible. If you cannot prove the safety of the violation based on the specific code, you must keep the concern."
            }
            10 => {
                "# Stage 10. Verification and severity estimation

You are the lead reviewer validating consolidated concerns. You will be given a list of deduplicated concerns after conflict resolution.
1. Validate each concern and prove the provided reasoning. Report all valid concerns as findings. If necessary, use tools to gather additional material. Discard all false positives.
2. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning. If you cannot find definitive proof that the concern is a false positive, it must be reported as a finding. If you're not sure about something and it's critical in the reasoning validation, make it obvious: if X is possible, then problem Y can occur. Always try to validate if X is possible yourself.
3. SERIES VALIDATION RULE: If you are reviewing a patch that is NOT the last patch in the series (indicated by the presence of subsequent patches in the Full Series Context), you MUST check if each identified concern is still a problem in the final state of the series (the end of the Series Range). If the problem has been resolved, fixed, or the code was rewritten in a subsequent patch in this series, you MUST discard the concern and NOT report it as a finding. You MUST verify this by checking the actual code at the end of the series using tools; do not trust promises or claims in commit messages.
4. When referring to other patches within this series in your explanation, DO NOT use git hashes (they are ephemeral/unstable). Instead, refer to them by their patch subject (e.g., 'commit \"mm: fix allocation\"'). Existing historical commits in the tree should still be referenced by their standard hash.
5. Assign a severity (low, medium, high, critical) to each remaining valid finding, following the calibration guidance in the severity definitions: reason through consequence, triggering path, and reachability, and state that reasoning at the start of the finding's `severity_explanation` so the label is auditable. Raise the level for a bug reachable by untrusted or remote input, and do not lower it because you believe the code is unreachable. A finding you can only state speculatively is capped at medium but still reported, never dropped. Be rigorous in filtering out verifiable noise, but accurately report real logic flaws and edge cases.
6. If the problem did exist in the code before the patch was applied, say it explicitly: 'This problem wasn't introduced by this patch, but...'. Discard low- and medium-severity pre-existing problems, report only high- and critical severity issues.
7. SPECIFICITY REQUIREMENT: Every finding MUST cite the exact function name(s), file path(s), line number(s) when known, and triggering conditions where the bug manifests. Vague descriptions like 'potential overflow in ring buffer calculations' are insufficient. State precisely which variable overflows, in which function, and under what input conditions. Do not invent line numbers; use `line: null` when the exact line is not known.
8. Carry forward the `locations` from the validated concern into each finding. If you gather better evidence, replace vague locations with the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters locations you verified."
            }
            11 => {
                "# Stage 11. LKML-friendly report generation

You are an automated review bot generating a report for the Linux Kernel Mailing List (LKML). Convert the provided JSON findings into a polite, standard, inline-commented LKML email reply.

CRITICAL RULE: If a finding is flagged as pre-existing (`\"preexisting\": true`), you MUST explicitly state in your inline comment that this issue is pre-existing and was not introduced by the patch under review. Use phrasing like \"This isn't a bug introduced by this patch, but...\" or \"This is a pre-existing issue, but...\" to start the comment.

Follow the formatting rules strictly. Do not use markdown headers or ALL CAPS shouting. Ensure the tone is constructive and professional. Do not use backticks to quote any names or expressions.

SPECIFICITY REQUIREMENT: Each inline comment MUST reference the exact function name, file, line number when known, and specific triggering condition. Prefer the finding's `locations` field when present. Do not produce vague summaries like 'potential issue in error handling'. State precisely what goes wrong, where, and under what circumstances. Do not invent line numbers; if the exact line is unavailable, anchor the comment to the nearest verified function or symbol and explain the triggering condition."
            }
            _ => "",
        };

        if !stage_instruction.is_empty() {
            content.push_str(stage_instruction);
            clean.push_str(stage_instruction);
            content.push_str("\n\n");
            clean.push_str("\n\n");
        }

        match stage {
            3 => {
                self.append_file(&mut content, &mut clean_files, "callstack.md")
                    .await?;
                self.append_file(&mut content, &mut clean_files, "technical-patterns.md")
                    .await?;
            }
            5 => {
                self.append_file(&mut content, &mut clean_files, "subsystem/locking.md")
                    .await?;
            }
            10 => {
                self.append_file(&mut content, &mut clean_files, "false-positive-guide.md")
                    .await?;
                self.append_file(&mut content, &mut clean_files, "severity.md")
                    .await?;
            }
            11 => {
                self.append_file(&mut content, &mut clean_files, "inline-template.md")
                    .await?;
            }
            _ => {}
        }
        if !clean_files.is_empty() {
            clean.push_str(&clean_files.join(", "));
            clean.push_str("\n\n");
        }
        Ok((content, clean))
    }

    async fn append_file(
        &self,
        buffer: &mut String,
        clean: &mut Vec<String>,
        filename: &str,
    ) -> Result<()> {
        let path = self.base_dir.join(filename);
        if path.exists() {
            buffer.push_str(&format!("# {}\n", filename));
            buffer.push_str(
                &fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("Failed to read {}", filename))?,
            );
            buffer.push_str("\n\n");

            clean.push(format!("@{}", filename));
        }
        Ok(())
    }

    async fn append_directory<F>(
        &self,
        buffer: &mut String,
        clean: &mut Vec<String>,
        dir: &Path,
        filter: F,
    ) -> Result<()>
    where
        F: Fn(&str) -> bool,
    {
        if !dir.exists() {
            return Ok(());
        }
        let mut entries = fs::read_dir(dir).await?;
        let mut paths = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md")
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && filter(name)
            {
                paths.push(path);
            }
        }
        paths.sort();
        for path in paths {
            let name = path.file_name().unwrap().to_string_lossy();
            let header = if let Ok(rel) = path.strip_prefix(&self.base_dir) {
                rel.to_string_lossy().to_string()
            } else {
                name.to_string()
            };
            buffer.push_str(&format!("## {}\n", header));
            buffer.push_str(&fs::read_to_string(&path).await?);
            buffer.push_str("\n\n");

            clean.push(format!("@{}", name));
        }
        Ok(())
    }

    pub fn calculate_content_hash<T: serde::Serialize>(
        &self,
        content: &str,
        tools: Option<&[T]>,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        if let Some(tools) = tools
            && let Ok(json) = serde_json::to_string(tools)
        {
            hasher.update(json);
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

pub struct Worker {
    provider: Arc<dyn AiProvider>,
    tools: Arc<ToolBox>,
    prompts: PromptRegistry,
    global_history: Vec<AiMessage>,
    max_interactions: usize,
    temperature: f32,
    series_range: Option<String>,
    context_tag: Option<String>,
    stages: Option<Vec<u8>>,
}

impl Worker {
    pub fn new(
        provider: Arc<dyn AiProvider>,
        tools: Arc<ToolBox>,
        prompts: PromptRegistry,
        config: WorkerConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            prompts,
            global_history: Vec::new(),
            max_interactions: config.max_interactions,
            temperature: config.temperature,
            series_range: config.series_range,
            context_tag: None,
            stages: config.stages,
        }
    }

    pub async fn run(
        &mut self,
        patchset: Value,
        progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
    ) -> Result<WorkerResult> {
        // 1. Extract inputs
        let mut target_commit_diff = String::new();
        let mut target_commit_diff_only = String::new();

        let ps_id = patchset["id"]
            .as_i64()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let p_id = patchset["patch_index"]
            .as_i64()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "multi".to_string());
        self.context_tag = Some(format!("[ps:{} p:{}] ", ps_id, p_id));

        let mut baseline_sha = "unknown".to_string();
        if let Some(ref range) = self.series_range {
            let parts: Vec<&str> = range.split("..").collect();
            if !parts.is_empty() {
                baseline_sha = parts[0].to_string();
            }
        }

        let mut target_commit_sha = "unknown".to_string();
        if let Some(patches) = patchset["patches"].as_array() {
            if let Some(idx) = patchset["patch_index"].as_i64()
                && let Some(p) = patches.iter().find(|p| p["index"].as_i64() == Some(idx))
                && let Some(sha) = p["commit_id"].as_str()
            {
                target_commit_sha = sha.to_string();
            }
            if target_commit_sha == "unknown"
                && !patches.is_empty()
                && let Some(sha) = patches[0]["commit_id"].as_str()
            {
                target_commit_sha = sha.to_string();
            }
        }

        if let Some(patches) = patchset["patches"].as_array() {
            for p in patches {
                if let Some(show) = p["git_show"].as_str() {
                    target_commit_diff.push_str(show);
                    target_commit_diff.push('\n');
                } else if let Some(diff) = p["diff"].as_str() {
                    target_commit_diff.push_str(diff);
                    target_commit_diff.push('\n');
                }

                if let Some(diff) = p["diff"].as_str() {
                    target_commit_diff_only.push_str(diff);
                    target_commit_diff_only.push('\n');
                }
            }
        }

        let mut all_concerns = Vec::new();
        let mut all_dismissed_concerns = Vec::new();
        let mut total_tokens_in = 0;
        let mut total_tokens_out = 0;
        let mut total_tokens_cached = 0;

        // Phase 0: Pre-screen relevant prompts
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::PreScreenStarted);
        }
        let subsystem_md_path = self.prompts.base_dir.join("subsystem/subsystem.md");
        let selected_prompts = if subsystem_md_path.exists() {
            match tokio::fs::read_to_string(&subsystem_md_path).await {
                Ok(subsystem_md) => {
                    info!("Executing Phase 0: Pre-screening relevant subsystem guides.");
                    let phase0_system = "You are an AI assistant preparing a Linux kernel patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"networking.md\", \"locking.md\"]}\n```";
                    let phase0_prompt = format!(
                        "<subsystem_guide_index>\n{}\n</subsystem_guide_index>\n\n<patch>\n{}\n</patch>",
                        subsystem_md, target_commit_diff
                    );
                    let schema = json!({
                        "type": "object",
                        "properties": {
                            "selected_prompts": {
                                "type": "array",
                                "items": { "type": "string" }
                            }
                        },
                        "required": ["selected_prompts"]
                    });

                    let req = AiRequest {
                        system: Some(phase0_system.to_string()),
                        messages: vec![AiMessage {
                            role: AiRole::User,
                            content: Some(phase0_prompt),
                            thought: None,
                            thought_signature: None,
                            tool_calls: None,
                            tool_call_id: None,
                        }],
                        tools: None,
                        temperature: Some(0.0),
                        response_format: Some(AiResponseFormat::Json {
                            schema: Some(schema),
                        }),
                        context_tag: self
                            .context_tag
                            .as_ref()
                            .map(|prefix| format!("{}s:0] ", &prefix[..prefix.len() - 2])),
                    };

                    let mut tokens = (total_tokens_in, total_tokens_out, total_tokens_cached);
                    let val = self
                        .json_request("s0", req, &mut tokens, |v| {
                            v.get("selected_prompts")
                                .and_then(|v| v.as_array())
                                .ok_or_else(|| "missing 'selected_prompts' array".to_string())
                                .map(|_| ())
                        })
                        .await;
                    total_tokens_in = tokens.0;
                    total_tokens_out = tokens.1;
                    total_tokens_cached = tokens.2;
                    val.and_then(|val| {
                        let arr = val.get("selected_prompts")?.as_array()?;
                        let prompts: Vec<String> = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .filter(|name| !STAGE_EXCLUSIVE_GUIDES.contains(&name.as_str()))
                            .collect();
                        info!("Phase 0 selected prompts: {:?}", prompts);
                        Some(prompts)
                    })
                }
                Err(e) => {
                    warn!("Failed to read subsystem.md for Phase 0: {}", e);
                    None
                }
            }
        } else {
            warn!(
                "subsystem.md not found for Phase 0 at {:?}",
                subsystem_md_path
            );
            None
        };

        let (static_context, clean_static_context) = self
            .prompts
            .build_context(selected_prompts.as_deref())
            .await?;

        let mut git_metadata = String::new();
        git_metadata.push_str("\n\n=== Active Git Metadata ===\n");
        git_metadata.push_str(&format!("Target Commit SHA: {}\n", target_commit_sha));
        git_metadata.push_str(&format!("Baseline SHA: {}\n", baseline_sha));
        git_metadata.push_str("===========================\n");

        let mut dynamic_context = String::new();
        dynamic_context.push_str(&git_metadata);
        dynamic_context.push_str("\n\nTarget Commit:\n");
        dynamic_context.push_str(&target_commit_diff);
        let mut clean_dynamic_context = dynamic_context.clone();

        let mut dynamic_context_no_log = String::new();
        dynamic_context_no_log.push_str(&git_metadata);
        dynamic_context_no_log.push_str("\n\nTarget Commit Diff:\n");
        dynamic_context_no_log.push_str(&target_commit_diff_only);
        let mut clean_dynamic_context_no_log = dynamic_context_no_log.clone();

        // Prefetch AST context based on the diff
        let worktree_path = self.tools.get_worktree_path();
        if let Ok(prefetched) =
            crate::worker::prefetch::prefetch_context(worktree_path, &target_commit_diff).await
            && !prefetched.is_empty()
        {
            dynamic_context.push_str("\n\n<pre_fetched_context>\n");
            dynamic_context.push_str("The following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\n");
            dynamic_context.push_str("If it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n");
            dynamic_context.push_str(&prefetched);
            dynamic_context.push_str("\n</pre_fetched_context>\n");

            clean_dynamic_context.push_str("\n\n<pre_fetched_context>\n");
            clean_dynamic_context.push_str("The following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\n");
            clean_dynamic_context.push_str("If it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n");
            clean_dynamic_context.push_str("{{prefetched_context}}\n</pre_fetched_context>\n");

            dynamic_context_no_log.push_str("\n\n<pre_fetched_context>\n");
            dynamic_context_no_log.push_str("The following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\n");
            dynamic_context_no_log.push_str("If it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n");
            dynamic_context_no_log.push_str(&prefetched);
            dynamic_context_no_log.push_str("\n</pre_fetched_context>\n");

            clean_dynamic_context_no_log.push_str("\n\n<pre_fetched_context>\n");
            clean_dynamic_context_no_log.push_str("The following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\n");
            clean_dynamic_context_no_log.push_str("If it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n");
            clean_dynamic_context_no_log
                .push_str("{{prefetched_context}}\n</pre_fetched_context>\n");
        }
        let (shared_context, clean_shared_context) = {
            // Without cache (or with implicit cache like Claude), we send everything.
            (
                format!("{}{}", static_context, dynamic_context),
                format!("{}{}", clean_static_context, clean_dynamic_context),
            )
        };

        let (shared_context_no_log, clean_shared_context_no_log) = {
            (
                format!("{}{}", static_context, dynamic_context_no_log),
                format!("{}{}", clean_static_context, clean_dynamic_context_no_log),
            )
        };

        let mut planning_selected_stages: Option<Vec<u8>> = None;
        if self.stages.is_none() {
            if let Some(progress_cb) = progress {
                progress_cb(WorkerProgressEvent::PlanningStarted);
            }
            let schema = serde_json::json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Array of stage numbers from 4, 5, 6, 7 that are relevant to this patch. Err on the side of inclusion if unsure."
                    }
                },
                "required": ["relevant_stages"]
            });

            let planning_prompt = r#"Analyze the provided patch and determine which of the following review stages are relevant and should be executed:
- Stage 4: Resource management
- Stage 5: Locking and synchronization
- Stage 6: Security audit
- Stage 7: Hardware engineer's review

CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo fix, you may omit some stages. Stages 1, 2, and 3 are always run and should not be included in your answer.

You MUST respond with ONLY a JSON object, no other text. Example:
```json
{"relevant_stages": [4, 5, 6, 7]}
```"#;

            let req = AiRequest {
                system: None,
                messages: vec![AiMessage {
                    role: crate::ai::AiRole::User,
                    content: Some(format!("{}\n\n{}", shared_context, planning_prompt)),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                }],
                tools: None,
                temperature: Some(0.0),
                response_format: Some(AiResponseFormat::Json {
                    schema: Some(schema),
                }),
                context_tag: self
                    .context_tag
                    .as_ref()
                    .map(|prefix| format!("{} s:p] ", &prefix[..prefix.len() - 2])),
            };

            info!("Running planning pre-phase");
            let mut tokens = (total_tokens_in, total_tokens_out, total_tokens_cached);
            let val = self
                .json_request("sp", req, &mut tokens, |v| {
                    v.get("relevant_stages")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| "missing 'relevant_stages' array".to_string())
                        .map(|_| ())
                })
                .await;
            total_tokens_in = tokens.0;
            total_tokens_out = tokens.1;
            total_tokens_cached = tokens.2;
            if let Some(val) = val {
                let arr = val["relevant_stages"].as_array().unwrap();
                let mut stages = vec![1, 2, 3];
                for v in arr {
                    if let Some(n) = v.as_u64()
                        && (4..=7).contains(&n)
                    {
                        stages.push(n as u8);
                    }
                }
                info!("Planning phase selected stages: {:?}", stages);
                planning_selected_stages = Some(stages);
            }
        }

        let mut planned_stages = Vec::new();
        for stage_num in 1..=7 {
            if let Some(ref selected_stages) = self.stages {
                if selected_stages.contains(&stage_num) {
                    planned_stages.push(stage_num);
                }
            } else if let Some(ref planned_stages_ref) = planning_selected_stages {
                if planned_stages_ref.contains(&stage_num) {
                    planned_stages.push(stage_num);
                }
            } else {
                planned_stages.push(stage_num);
            }
        }
        if !planned_stages.is_empty() {
            planned_stages.push(8);
            planned_stages.push(9);
            planned_stages.push(10);
            planned_stages.push(11);
        }
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::ReviewStarted { planned_stages });
        }

        // Initialize system message in global history once before running stages
        if self.global_history.is_empty() {
            self.global_history.push(AiMessage {
                role: AiRole::System,
                content: Some(clean_shared_context.clone()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Construct futures for Stages 1-7
        let mut stage_futures = Vec::new();
        for stage_num in 1..=7 {
            if let Some(ref selected_stages) = self.stages {
                if !selected_stages.contains(&stage_num) {
                    continue;
                }
            } else if let Some(ref planned_stages) = planning_selected_stages
                && !planned_stages.contains(&stage_num)
            {
                info!("Skipping stage {} based on planning phase", stage_num);
                continue;
            }

            let stage = create_stage(stage_num);
            let use_log = stage.use_log_in_context();
            let system_prompt = if use_log {
                shared_context.clone()
            } else {
                shared_context_no_log.clone()
            };
            let clean_system_prompt = if use_log {
                clean_shared_context.clone()
            } else {
                clean_shared_context_no_log.clone()
            };

            stage_futures.push(self.execute_stage(
                stage,
                system_prompt,
                clean_system_prompt,
                progress,
            ));
        }

        // Run planned stages concurrently
        info!(
            "Running {} planned stages concurrently",
            stage_futures.len()
        );
        let stage_results = futures::future::try_join_all(stage_futures).await?;

        // Consolidate results in deterministic order (already preserved by try_join_all)
        for res in stage_results {
            total_tokens_in += res.tokens_in;
            total_tokens_out += res.tokens_out;
            total_tokens_cached += res.tokens_cached;

            append_stage_items(
                &mut all_concerns,
                &res.concerns,
                res.stage,
                "General",
                "description",
            );
            append_stage_dismissed_concerns(
                &mut all_dismissed_concerns,
                &res.dismissed_concerns,
                res.stage,
            );

            // Append history in order
            self.global_history.extend(res.history);
        }

        if all_concerns.is_empty() {
            tracing::info!("No concerns from stages 1-7, skipping stages 8, 9, 10 and 11");
            let dismissed_concerns_count = all_dismissed_concerns.len();
            let final_output = serde_json::json!({
                "findings": [],
                "dismissed_concerns": all_dismissed_concerns,
                "review_inline": "No issues found.",
                "fixes": "",
                "concerns_count": 0,
                "dismissed_concerns_count": dismissed_concerns_count
            });
            return Ok(WorkerResult {
                output: Some(final_output),
                error: None,
                input_context: "Multi-stage execution completed".to_string(),
                history: self.global_history.clone(),
                history_before_pruning: self.global_history.clone(),
                history_after_pruning: self.global_history.clone(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                tokens_cached: total_tokens_cached,
            });
        }

        // Stage 8: Deduplication
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: 8 });
        }
        info!("Running Stage 8 (Deduplication)");
        let deduplicated_concerns;
        let deduplicated_dismissed_concerns;
        {
            let stage = 8;
            let (stage_prompt, _) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();

            let aggregated_concerns_json =
                serde_json::to_string_pretty(&all_concerns).unwrap_or_default();
            let aggregated_dismissed_concerns_json =
                serde_json::to_string_pretty(&all_dismissed_concerns).unwrap_or_default();

            let user_prompt = format!(
                r#"{}

Aggregated Concerns:
{}

Aggregated Dismissed Concerns:
{}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Preserve the most precise location details from the input. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ],
  "dismissed_concerns": [
    {{
      "type": "Resource Management",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 125,
          "code_snippet": "safe_code_path();",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }}
      ]
    }}
  ]
}}
```"#,
                stage_prompt, aggregated_concerns_json, aggregated_dismissed_concerns_json
            );

            let clean_user_prompt = format!(
                r#"{}

Consolidated Concerns:
{}

Consolidated Dismissed Concerns:
{}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Preserve the most precise location details from the input. Do not invent line numbers; use null when exact values are unknown."#,
                stage_prompt, aggregated_concerns_json, aggregated_dismissed_concerns_json
            );

            let stage_impl = create_stage(stage);
            let mut session = ReviewStageSession::new(
                stage_impl,
                system_prompt,
                user_prompt,
                clean_user_prompt,
                self.tools.clone(),
                self.temperature,
                self.context_tag.as_deref(),
            );
            let runner = SessionRunner::new(self.provider.as_ref())
                .with_max_validation_attempts(3)
                .with_max_turns(self.max_interactions)
                .with_turn_callback(move |turn, max_turns| {
                    if let Some(progress_cb) = progress {
                        progress_cb(WorkerProgressEvent::StageTurn {
                            stage,
                            turn,
                            max_turns,
                        });
                    }
                });
            let result = runner.run(&mut session).await?;

            total_tokens_in += result.usage.prompt_tokens as u32;
            total_tokens_out += result.usage.completion_tokens as u32;
            total_tokens_cached += result.usage.cached_tokens.unwrap_or(0) as u32;
            self.global_history.extend(result.history);

            deduplicated_concerns = result.output.get("concerns").unwrap().clone();
            deduplicated_dismissed_concerns =
                result.output.get("dismissed_concerns").unwrap().clone();
        }
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: 8 });
        }

        if let Some(c) = deduplicated_concerns.as_array()
            && c.is_empty()
        {
            tracing::info!(
                "No concerns remaining after Stage 8 deduplication, skipping stages 9, 10 and 11"
            );
            let final_output = serde_json::json!({
                "findings": [],
                "dismissed_concerns": deduplicated_dismissed_concerns,
                "review_inline": "No issues found.",
                "fixes": "",
                "concerns_count": all_concerns.len(),
                "dismissed_concerns_count": deduplicated_dismissed_concerns
                    .as_array()
                    .map_or(0, Vec::len)
            });
            return Ok(WorkerResult {
                output: Some(final_output),
                error: None,
                input_context: "Multi-stage execution completed".to_string(),
                history: self.global_history.clone(),
                history_before_pruning: self.global_history.clone(),
                history_after_pruning: self.global_history.clone(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                tokens_cached: total_tokens_cached,
            });
        }

        // Stage 9: Concern/dismissed-concern conflict resolution
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: 9 });
        }
        info!("Running Stage 9 (Concern/dismissed-concern conflict resolution)");
        let conflict_resolved_concerns;
        {
            let stage = 9;
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();

            let deduplicated_concerns_json =
                serde_json::to_string_pretty(&deduplicated_concerns).unwrap_or_default();
            let deduplicated_dismissed_concerns_json =
                serde_json::to_string_pretty(&deduplicated_dismissed_concerns).unwrap_or_default();

            let user_prompt = format!(
                r#"{}

Consolidated Concerns:
{}

Consolidated Dismissed Concerns:
{}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Preserve the most precise locations from the retained concerns. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#,
                stage_prompt, deduplicated_concerns_json, deduplicated_dismissed_concerns_json
            );

            let clean_user_prompt = format!(
                r#"{}

Consolidated Concerns:
{}

Consolidated Dismissed Concerns:
{}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Preserve the most precise locations from the retained concerns. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#,
                clean_stage_prompt,
                deduplicated_concerns_json,
                deduplicated_dismissed_concerns_json
            );

            let stage_impl = create_stage(stage);
            let mut session = ReviewStageSession::new(
                stage_impl,
                system_prompt,
                user_prompt,
                clean_user_prompt,
                self.tools.clone(),
                self.temperature,
                self.context_tag.as_deref(),
            );
            let runner = SessionRunner::new(self.provider.as_ref())
                .with_max_validation_attempts(3)
                .with_max_turns(self.max_interactions)
                .with_turn_callback(move |turn, max_turns| {
                    if let Some(progress_cb) = progress {
                        progress_cb(WorkerProgressEvent::StageTurn {
                            stage,
                            turn,
                            max_turns,
                        });
                    }
                });
            let result = runner.run(&mut session).await?;

            total_tokens_in += result.usage.prompt_tokens as u32;
            total_tokens_out += result.usage.completion_tokens as u32;
            total_tokens_cached += result.usage.cached_tokens.unwrap_or(0) as u32;
            self.global_history.extend(result.history);

            conflict_resolved_concerns = result.output.get("concerns").unwrap().clone();
        }
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: 9 });
        }

        if let Some(c) = conflict_resolved_concerns.as_array()
            && c.is_empty()
        {
            tracing::info!(
                "No concerns remaining after Stage 9 conflict resolution, skipping stages 10 and 11"
            );
            let final_output = serde_json::json!({
                "findings": [],
                "dismissed_concerns": deduplicated_dismissed_concerns,
                "review_inline": "No issues found.",
                "fixes": "",
                "concerns_count": all_concerns.len(),
                "dismissed_concerns_count": deduplicated_dismissed_concerns
                    .as_array()
                    .map_or(0, Vec::len)
            });
            return Ok(WorkerResult {
                output: Some(final_output),
                error: None,
                input_context: "Multi-stage execution completed".to_string(),
                history: self.global_history.clone(),
                history_before_pruning: self.global_history.clone(),
                history_after_pruning: self.global_history.clone(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                tokens_cached: total_tokens_cached,
            });
        }

        // Stage 10: Verification
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: 10 });
        }
        info!("Running Stage 10 (Verification)");
        let findings_json;
        {
            let stage = 10;
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();

            let full_series_context = if let Some(range) = &self.series_range {
                let cmd_output = std::process::Command::new("git")
                    .current_dir(self.tools.get_worktree_path())
                    .args(["--no-pager", "log", "--reverse", "--format=%s", range])
                    .output();

                match cmd_output {
                    Ok(out) if out.status.success() => {
                        let subjects = String::from_utf8_lossy(&out.stdout).to_string();
                        format!(
                            "Series Range: {}\n\nPatches in series:\n{}",
                            range, subjects
                        )
                    }
                    Ok(out) => {
                        warn!(
                            "git log failed for range {}: {}",
                            range,
                            String::from_utf8_lossy(&out.stderr)
                        );
                        "Failed to retrieve full series context (git log error).".to_string()
                    }
                    Err(e) => {
                        warn!("git command failed: {}", e);
                        "Failed to retrieve full series context (git execution error).".to_string()
                    }
                }
            } else {
                "Not applicable (single patch or last patch in series).".to_string()
            };

            let conflict_resolved_concerns_json =
                serde_json::to_string_pretty(&conflict_resolved_concerns).unwrap_or_default();
            let user_prompt = format!(
                "{}\n\nCRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.\n\nFull Series Context:\n{}\n\nConsolidated Concerns:\n{}\n\nReturn ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: \"problem\" (a string containing the vulnerability description), \"severity\" (a string: Low, Medium, High, or Critical), \"severity_explanation\" (a string detailing the reasoning and proof), \"preexisting\" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), \"locations\" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters). Carry forward the locations from the validated concern; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.\n\nExample Output:\n```json\n{{\n  \"findings\": [\n    {{\n      \"problem\": \"Memory leak in function X when condition Y is met.\",\n      \"severity\": \"High\",\n      \"severity_explanation\": \"1. Condition Y is met.\\\n2. The buffer is allocated but not freed before return.\",\n      \"preexisting\": false,\n      \"locations\": [\n        {{\n          \"file\": \"path/to/file.c\",\n          \"function_or_symbol\": \"function_name\",\n          \"line\": 123,\n          \"code_snippet\": \"problematic_code();\",\n          \"why_this_location_matters\": \"This is where the newly allocated resource is dropped on the error path.\"\n        }}\n      ]\n    }}\n  ]\n}}\n```",
                stage_prompt, full_series_context, conflict_resolved_concerns_json
            );

            let clean_user_prompt = format!(
                "{}\n\nCRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.\n\nFull Series Context:\n{{{{series context}}}}\n\nConsolidated Concerns:\n{}\n\nReturn ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: \"problem\" (a string containing the vulnerability description), \"severity\" (a string: Low, Medium, High, or Critical), \"severity_explanation\" (a string detailing the reasoning and proof), \"preexisting\" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), \"locations\" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters). Carry forward the locations from the validated concern; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.\n\nExample Output:\n```json\n{{\n  \"findings\": [\n    {{\n      \"problem\": \"Memory leak in function X when condition Y is met.\",\n      \"severity\": \"High\",\n      \"severity_explanation\": \"1. Condition Y is met.\\\n2. The buffer is allocated but not freed before return.\",\n      \"preexisting\": false,\n      \"locations\": [\n        {{\n          \"file\": \"path/to/file.c\",\n          \"function_or_symbol\": \"function_name\",\n          \"line\": 123,\n          \"code_snippet\": \"problematic_code();\",\n          \"why_this_location_matters\": \"This is where the newly allocated resource is dropped on the error path.\"\n        }}\n      ]\n    }}\n  ]\n}}\n```",
                clean_stage_prompt, conflict_resolved_concerns_json
            );

            let stage_impl = create_stage(stage);
            let mut session = ReviewStageSession::new(
                stage_impl,
                system_prompt,
                user_prompt,
                clean_user_prompt,
                self.tools.clone(),
                self.temperature,
                self.context_tag.as_deref(),
            );
            let runner = SessionRunner::new(self.provider.as_ref())
                .with_max_validation_attempts(3)
                .with_max_turns(self.max_interactions)
                .with_turn_callback(move |turn, max_turns| {
                    if let Some(progress_cb) = progress {
                        progress_cb(WorkerProgressEvent::StageTurn {
                            stage,
                            turn,
                            max_turns,
                        });
                    }
                });
            let result = runner.run(&mut session).await?;

            total_tokens_in += result.usage.prompt_tokens as u32;
            total_tokens_out += result.usage.completion_tokens as u32;
            total_tokens_cached += result.usage.cached_tokens.unwrap_or(0) as u32;
            self.global_history.extend(result.history);

            findings_json = result.output.get("findings").unwrap().clone();
        }
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: 10 });
        }

        if let Some(f) = findings_json.as_array()
            && f.is_empty()
        {
            tracing::info!("No findings from Stage 10, skipping Stage 11");
            let final_output = serde_json::json!({
                "findings": findings_json,
                "dismissed_concerns": deduplicated_dismissed_concerns,
                "review_inline": "No issues found.",
                "fixes": "",
                "concerns_count": all_concerns.len(),
                "dismissed_concerns_count": deduplicated_dismissed_concerns
                    .as_array()
                    .map_or(0, Vec::len)
            });
            return Ok(WorkerResult {
                output: Some(final_output),
                error: None,
                input_context: "Multi-stage execution completed".to_string(),
                history: self.global_history.clone(),
                history_before_pruning: self.global_history.clone(),
                history_after_pruning: self.global_history.clone(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                tokens_cached: total_tokens_cached,
            });
        }

        // Stage 11
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: 11 });
        }
        info!("Running Stage 11");
        let review_inline_text;
        {
            let stage = 11;
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();
            let findings_str = serde_json::to_string_pretty(&findings_json).unwrap_or_default();
            let user_prompt = format!(
                "{}\n\nFindings:\n{}\n\nReturn raw text output, not JSON.",
                stage_prompt, findings_str
            );
            let clean_user_prompt = format!(
                "{}\n\nFindings:\n{}\n\nReturn raw text output, not JSON.",
                clean_stage_prompt, findings_str
            );

            let stage_impl = create_stage(stage);
            let mut session = ReviewStageSession::new(
                stage_impl,
                system_prompt,
                user_prompt,
                clean_user_prompt,
                self.tools.clone(),
                self.temperature,
                self.context_tag.as_deref(),
            );
            let runner = SessionRunner::new(self.provider.as_ref())
                .with_max_validation_attempts(3)
                .with_max_turns(self.max_interactions)
                .with_turn_callback(move |turn, max_turns| {
                    if let Some(progress_cb) = progress {
                        progress_cb(WorkerProgressEvent::StageTurn {
                            stage,
                            turn,
                            max_turns,
                        });
                    }
                });
            let result = runner.run(&mut session).await?;

            total_tokens_in += result.usage.prompt_tokens as u32;
            total_tokens_out += result.usage.completion_tokens as u32;
            total_tokens_cached += result.usage.cached_tokens.unwrap_or(0) as u32;
            self.global_history.extend(result.history);

            review_inline_text = result.output.as_str().unwrap().to_string();
        }
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: 11 });
        }

        let fixes_text = String::new();
        let dismissed_concerns_count = deduplicated_dismissed_concerns
            .as_array()
            .map_or(0, Vec::len);

        let final_output = json!({
            "findings": findings_json,
            "dismissed_concerns": deduplicated_dismissed_concerns,
            "review_inline": review_inline_text,
            "fixes": fixes_text,
            "concerns_count": all_concerns.len(),
            "dismissed_concerns_count": dismissed_concerns_count
        });

        Ok(WorkerResult {
            output: Some(final_output),
            error: None,
            input_context: "Multi-stage execution completed".to_string(),
            history: self.global_history.clone(),
            history_before_pruning: self.global_history.clone(),
            history_after_pruning: self.global_history.clone(),
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            tokens_cached: total_tokens_cached,
        })
    }

    async fn json_request(
        &self,
        label: &str,
        req: AiRequest,
        tokens: &mut (u32, u32, u32),
        validate: impl Fn(&Value) -> Result<(), String>,
    ) -> Option<Value> {
        fn accumulate(tokens: &mut (u32, u32, u32), usage: &crate::ai::AiUsage) {
            tokens.0 += usage.prompt_tokens as u32;
            tokens.1 += usage.completion_tokens as u32;
            tokens.2 += usage.cached_tokens.unwrap_or(0) as u32;
        }

        fn try_parse(
            content: &str,
            validate: &impl Fn(&Value) -> Result<(), String>,
        ) -> Result<Value, String> {
            let stripped = content.trim();
            let stripped = stripped
                .strip_prefix("```json")
                .or_else(|| stripped.strip_prefix("```"))
                .map(|s| s.strip_suffix("```").unwrap_or(s).trim())
                .unwrap_or(stripped);
            let v = serde_json::from_str::<Value>(stripped)
                .map_err(|e| format!("JSON parse error: {}", e))?;
            validate(&v)?;
            Ok(v)
        }

        let retry_base = req.clone();
        let resp = match self.provider.generate_content(req).await {
            Ok(r) => r,
            Err(e) => {
                warn!("{} completion failed: {}", label, e);
                return None;
            }
        };
        if resp.truncated {
            warn!("{} completion truncated by provider limit", label);
            return None;
        }
        if let Some(usage) = &resp.usage {
            accumulate(tokens, usage);
        }
        let content = resp.content.as_deref().unwrap_or("");
        match try_parse(content, &validate) {
            Ok(v) => return Some(v),
            Err(e) => {
                warn!("{}: {}, retrying with correction", label, e);
                let mut retry_req = retry_base;
                retry_req.messages.push(AiMessage {
                    role: AiRole::Assistant,
                    content: Some(content.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                });
                retry_req.messages.push(AiMessage {
                    role: AiRole::User,
                    content: Some(format!(
                        "Your response is not valid: {}\nRespond with ONLY valid JSON conforming to the schema. No markdown, no explanation.",
                        e
                    )),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                });
                match self.provider.generate_content(retry_req).await {
                    Ok(resp2) => {
                        if resp2.truncated {
                            warn!("{} retry completion truncated by provider limit", label);
                            return None;
                        }
                        if let Some(usage) = &resp2.usage {
                            accumulate(tokens, usage);
                        }
                        let content2 = resp2.content.as_deref().unwrap_or("");
                        match try_parse(content2, &validate) {
                            Ok(v) => {
                                warn!("{} succeeded on retry (first attempt was invalid)", label);
                                return Some(v);
                            }
                            Err(e2) => {
                                warn!("{} failed on retry too: {}", label, e2);
                            }
                        }
                    }
                    Err(e2) => {
                        warn!("{} retry request failed: {}", label, e2);
                    }
                }
            }
        }
        None
    }

    async fn execute_stage(
        &self,
        stage: Box<dyn ReviewStage>,
        system_prompt: String,
        _clean_system_prompt: String,
        progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
    ) -> Result<StageExecutionResult> {
        let stage_num = stage.number();
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: stage_num });
        }
        info!("Running Stage {}", stage_num);
        let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage_num).await?;

        let format_guidance = r#"TodoWrite compatibility: vendored prompts may ask you to add tasks or suspected bugs to TodoWrite. Do not call or mention TodoWrite. Treat those instructions as an internal checklist only. If that checklist identifies a concrete suspected bug, carry it forward as a JSON concern with file, function_or_symbol, line when known, triggering condition, and evidence. Do not output generic checklist progress as a concern.

Once you have gathered sufficient information, return ONLY a JSON object with "concerns" and "dismissed_concerns" arrays.
If you find no concerns and no dismissed concerns, return `{"concerns": [], "dismissed_concerns": []}`.
If you find concerns, each must be an object with:
- "type": A short category string.
- "description": A clear description of the problem.
- "reasoning": A step-by-step explanation.
- "preexisting": A boolean value: `true` if this bug/vulnerability already existed in the codebase before these patches were applied, or `false` if the issue was newly introduced by the reviewed patchset.
- "locations": An array of objects, each containing "file", "function_or_symbol", "line_range" (e.g., "120-125"), and "why_this_location_matters". Use `null` for "file", "function_or_symbol", or "line_range" when an issue is non-local or the exact value is not known. Do not invent line numbers; use `line_range: null` when the exact lines are not known and explain the triggering condition in "reasoning".

Use the "dismissed_concerns" array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence. This is especially important when you first suspect a concern and then follow the evidence chain proving that it does NOT apply.
If you find dismissed_concerns, each must use the same item schema as concerns except that dismissed_concerns do not need the "preexisting" field:
- "type": A short category string.
- "description": The candidate concern that was investigated and disproved.
- "reasoning": A step-by-step explanation of the evidence proving the candidate concern does not apply.
- "locations": An array of objects, each containing "file", "function_or_symbol", "line_range" (e.g., "145-150"), and "why_this_location_matters". Use `null` for unknown values. Do not invent line numbers.

CRITICAL REVIEW DIRECTIVE: Do NOT dismiss concerns just because you assume the surrounding system or caller handles it perfectly. Do not be overly charitable to the existing code. If there is a missing initialization, an unhandled edge case, or a brittle logic flow, report it as a concern immediately. Assume the worst-case scenario where external inputs and caller states are malformed.

Example:
```json
{
  "concerns": [
    {
      "type": "Issue Category",
      "description": "What is wrong.",
      "reasoning": "Why it is wrong.",
      "preexisting": false,
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line_range": "120-125",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Issue Category",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line_range": "145-150",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }
      ]
    }
  ]
}
```"#;

        let user_prompt = format!("{}\n\n{}", stage_prompt, format_guidance);
        let clean_user_prompt = format!("{}\n\n{}", clean_stage_prompt, format_guidance);

        let mut session = ReviewStageSession::new(
            stage,
            system_prompt,
            user_prompt,
            clean_user_prompt,
            self.tools.clone(),
            self.temperature,
            self.context_tag.as_deref(),
        );

        let runner = SessionRunner::new(self.provider.as_ref())
            .with_max_validation_attempts(3)
            .with_max_turns(self.max_interactions)
            .with_turn_callback(move |turn, max_turns| {
                if let Some(progress_cb) = progress {
                    progress_cb(WorkerProgressEvent::StageTurn {
                        stage: stage_num,
                        turn,
                        max_turns,
                    });
                }
            });

        let result = runner.run(&mut session).await?;

        let mut concerns_out = Vec::new();
        let mut dismissed_concerns_out = Vec::new();

        if let Some(c) = result.output.get("concerns").and_then(|v| v.as_array()) {
            concerns_out = c.clone();
        }
        if let Some(c) = result
            .output
            .get("dismissed_concerns")
            .and_then(|v| v.as_array())
        {
            dismissed_concerns_out = c.clone();
        }

        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: stage_num });
        }

        Ok(StageExecutionResult {
            stage: stage_num,
            concerns: concerns_out,
            dismissed_concerns: dismissed_concerns_out,
            tokens_in: result.usage.prompt_tokens as u32,
            tokens_out: result.usage.completion_tokens as u32,
            tokens_cached: result.usage.cached_tokens.unwrap_or(0) as u32,
            history: result.history,
        })
    }
}

struct StageExecutionResult {
    stage: u8,
    concerns: Vec<Value>,
    dismissed_concerns: Vec<Value>,
    tokens_in: u32,
    tokens_out: u32,
    tokens_cached: u32,
    history: Vec<AiMessage>,
}

pub fn calculate_series_range(
    patches: &[PatchInput],
    patches_to_review: &[PatchInput],
    patch_shas: &std::collections::HashMap<i64, String>,
    baseline_sha: &str,
) -> Option<String> {
    if patches.is_empty() {
        return None;
    }

    let max_patch_index = patches.iter().map(|p| p.index).max().unwrap_or(0);
    let is_last_patch_review =
        patches_to_review.len() == 1 && patches_to_review[0].index == max_patch_index;

    if is_last_patch_review {
        None
    } else {
        patches
            .iter()
            .map(|p| p.index)
            .max()
            .and_then(|max_idx| {
                patches
                    .iter()
                    .find(|p| p.index == max_idx)
                    .and_then(|p| p.commit_id.clone())
                    .or_else(|| patch_shas.get(&max_idx).cloned())
            })
            .map(|end_sha| format!("{}..{}", baseline_sha, end_sha))
    }
}

fn append_stage_items(
    target: &mut Vec<Value>,
    items: &[Value],
    stage: u8,
    default_type: &str,
    default_text_key: &str,
) {
    for item in items {
        if let Some(item) = normalize_stage_item(item, stage, default_type, default_text_key) {
            target.push(item);
        }
    }
}

fn append_stage_dismissed_concerns(target: &mut Vec<Value>, items: &[Value], stage: u8) {
    append_stage_items(target, items, stage, "General", "description");
}

fn normalize_stage_item(
    item: &Value,
    stage: u8,
    default_type: &str,
    default_text_key: &str,
) -> Option<Value> {
    if let Some(obj) = item.as_object() {
        let mut with_stage = obj.clone();
        with_stage.insert("source_stage".to_string(), json!(stage));
        Some(Value::Object(with_stage))
    } else {
        item.as_str().map(|s| {
            let mut obj = serde_json::Map::new();
            obj.insert("source_stage".to_string(), json!(stage));
            obj.insert("type".to_string(), json!(default_type));
            obj.insert(default_text_key.to_string(), json!(s));
            Value::Object(obj)
        })
    }
}

struct ReviewStageSession {
    stage: Box<dyn ReviewStage>,
    system_prompt: String,
    user_prompt: String,
    clean_user_prompt: String,
    tools: std::sync::Arc<ToolBox>,
    temperature: f32,
    context_tag: Option<String>,
    last_tool_call: Option<(String, Value)>,
    recitation_retries: usize,
}

impl ReviewStageSession {
    fn new(
        stage: Box<dyn ReviewStage>,
        system_prompt: String,
        user_prompt: String,
        clean_user_prompt: String,
        tools: std::sync::Arc<ToolBox>,
        temperature: f32,
        context_prefix: Option<&str>,
    ) -> Self {
        let stage_num = stage.number();
        let context_tag = context_prefix.map(|prefix| {
            if prefix.len() >= 2 {
                format!("{} s:{}] ", &prefix[..prefix.len() - 2], stage_num)
            } else {
                format!("s:{}] ", stage_num)
            }
        });
        Self {
            stage,
            system_prompt,
            user_prompt,
            clean_user_prompt,
            tools,
            temperature,
            context_tag,
            last_tool_call: None,
            recitation_retries: 0,
        }
    }
}

#[async_trait::async_trait]
impl LlmSession for ReviewStageSession {
    type Output = serde_json::Value;

    fn system_prompt(&self) -> String {
        self.system_prompt.clone()
    }

    fn initial_user_prompt(&self) -> String {
        self.user_prompt.clone()
    }

    fn log_user_prompt(&self) -> String {
        self.clean_user_prompt.clone()
    }

    fn format_validation_feedback(&self, violation: &str) -> String {
        self.stage.format_validation_feedback(violation)
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        Some(self.tools.get_declarations_generic())
    }

    fn temperature(&self) -> Option<f32> {
        Some(self.temperature)
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        if self
            .last_tool_call
            .as_ref()
            .map_or(false, |(last_name, last_args)| {
                last_name == name && last_args == &args
            })
        {
            tracing::warn!("Blocked duplicate tool call: {} with args {:?}", name, args);
            return Ok(serde_json::json!({
                "error": "Duplicate tool call blocked. Please change parameters or use a different tool."
            }));
        }
        self.last_tool_call = Some((name.to_string(), args.clone()));
        match self.tools.call(name, args).await {
            Ok(v) => Ok(v),
            Err(e) => Ok(serde_json::json!({
                "error": e.to_string()
            })),
        }
    }

    async fn call_tools(
        &mut self,
        calls: Vec<crate::ai::ToolCall>,
    ) -> Result<Vec<(String, Value)>> {
        let mut results = vec![None; calls.len()];
        let mut calls_to_run = Vec::new();

        for (idx, call) in calls.into_iter().enumerate() {
            let name = call.function_name;
            let args = call.arguments;
            let call_id = call.id;

            if self
                .last_tool_call
                .as_ref()
                .map_or(false, |(last_name, last_args)| {
                    last_name == &name && last_args == &args
                })
            {
                tracing::warn!("Blocked duplicate tool call: {} with args {:?}", name, args);
                results[idx] = Some((
                    call_id,
                    serde_json::json!({
                        "error": "Duplicate tool call blocked. Please change parameters or use a different tool."
                    }),
                ));
            } else {
                self.last_tool_call = Some((name.clone(), args.clone()));
                calls_to_run.push((idx, call_id, name, args));
            }
        }

        if !calls_to_run.is_empty() {
            let tools = self.tools.clone();
            let futures: Vec<_> = calls_to_run
                .into_iter()
                .map(|(idx, call_id, name, args)| {
                    let tools = tools.clone();
                    async move {
                        let res = match tools.call(&name, args).await {
                            Ok(v) => v,
                            Err(e) => serde_json::json!({"error": e.to_string()}),
                        };
                        (idx, (call_id, res))
                    }
                })
                .collect();

            let parallel_results = futures::future::join_all(futures).await;
            for (idx, res) in parallel_results {
                results[idx] = Some(res);
            }
        }

        Ok(results.into_iter().map(|o| o.unwrap()).collect())
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        self.stage.validate(response)
    }

    fn handle_provider_error(&mut self, error: &anyhow::Error, _attempt: usize) -> ErrorAction {
        let err_str = error.to_string();
        let is_recitation = err_str.contains("RECITATION") || err_str.contains("blocked");

        if is_recitation {
            self.recitation_retries += 1;
            if self.recitation_retries > 3 {
                return ErrorAction::Fail;
            }

            if let Some(action) = self.stage.handle_recitation_error() {
                return action;
            }

            return ErrorAction::RetryWithFeedback(
                "IMPORTANT: Your previous response was blocked by a recitation filter. \
                 Please do NOT copy large blocks of code verbatim in your response. \
                 Describe changes in prose, or use highly simplified pseudo-code if you must show code structure."
                    .to_string(),
            );
        }

        ErrorAction::Fail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_append_stage_dismissed_concerns_preserves_category_type() {
        let mut items = Vec::new();
        let input = vec![json!({
            "type": "Resource Management",
            "description": "suspected cross-zone page leak does not apply",
            "reasoning": "hugetlb_free_cross_zone_pages() runs before HVO init"
        })];

        append_stage_dismissed_concerns(&mut items, &input, 1);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 1);
        assert_eq!(items[0]["type"], "Resource Management");
        assert_eq!(
            items[0]["reasoning"],
            "hugetlb_free_cross_zone_pages() runs before HVO init"
        );
    }

    #[test]
    fn test_append_stage_dismissed_concerns_normalizes_string_items() {
        let mut items = Vec::new();
        let input = vec![json!("suspected missing cleanup does not apply")];

        append_stage_dismissed_concerns(&mut items, &input, 2);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 2);
        assert_eq!(items[0]["type"], "General");
        assert_eq!(
            items[0]["description"],
            "suspected missing cleanup does not apply"
        );
    }

    #[test]
    fn test_append_stage_items_overwrites_existing_source_stage() {
        let mut items = Vec::new();
        let input = vec![json!({
            "source_stage": 3,
            "type": "Execution flow",
            "description": "already annotated"
        })];

        append_stage_items(&mut items, &input, 4, "General", "description");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 4);
    }

    #[test]
    fn test_append_stage_items_normalizes_string_items() {
        let mut items = Vec::new();
        let input = vec![json!("plain concern")];

        append_stage_items(&mut items, &input, 6, "General", "description");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 6);
        assert_eq!(items[0]["type"], "General");
        assert_eq!(items[0]["description"], "plain concern");
    }

    #[test]
    fn test_calculate_series_range_single_patch() {
        let p = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let patches = vec![p.clone()];
        let patches_to_review = vec![p.clone()];
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_last() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p2.clone()]; // Reviewing last
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_middle() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()]; // Reviewing first
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2".to_string())
        );
    }

    #[test]
    fn test_calculate_series_range_use_patch_shas_map() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None, // Missing in input
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None, // Missing in input
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()];

        let mut patch_shas = std::collections::HashMap::new();
        patch_shas.insert(2, "sha2_resolved".to_string());

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2_resolved".to_string())
        );
    }

    struct MockProviderAlwaysFails;
    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockProviderAlwaysFails {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            anyhow::bail!("mock: simulated AI failure")
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_stage_failure_aborts_review() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockProviderAlwaysFails);
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            custom_prompt: None,
            stages: None,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        match worker.run(patchset, None).await {
            Ok(_) => panic!("Expected stage failure error, got Ok"),
            Err(e) => assert!(
                e.to_string().contains("simulated AI failure"),
                "unexpected error: {e}"
            ),
        }
    }

    // ReviewError tests

    #[test]
    fn test_limit_exceeded_classifies_as_fatal() {
        let err = ReviewError::LimitExceeded;

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_budget_exceeded_classifies_as_fatal() {
        let err = ReviewError::BudgetExceeded("1000 tokens used (limit: 500)".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_format_rejection_classifies_as_fatal() {
        let err = ReviewError::FormatRejection("contains markdown code blocks".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_limit_exceeded_downcasts_as_review_error() {
        let err: anyhow::Error = ReviewError::LimitExceeded.into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "LimitExceeded must downcast to ReviewError so the retry loop can fail fast"
        );
    }

    #[test]
    fn test_budget_exceeded_downcasts_as_review_error() {
        let err: anyhow::Error =
            ReviewError::BudgetExceeded("1000 tokens used (limit: 500)".to_string()).into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "BudgetExceeded must downcast to ReviewError so the retry loop can fail fast"
        );
    }

    #[test]
    fn test_generic_error_does_not_downcast_as_review_error() {
        let err: anyhow::Error = anyhow::anyhow!("transient JSON parse failure");
        assert!(
            err.downcast_ref::<ReviewError>().is_none(),
            "Plain anyhow errors must NOT match ReviewError so they remain retryable"
        );
    }

    #[test]
    fn test_format_rejection_downcasts_as_review_error() {
        let err: anyhow::Error =
            ReviewError::FormatRejection("contains markdown code blocks".to_string()).into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "FormatRejection must downcast to ReviewError"
        );
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockProviderDuplicateCalls {
        turn: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockProviderDuplicateCalls {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
            if turn == 0 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_1".to_string(),
                        function_name: "git_log".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else if turn == 1 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_2".to_string(),
                        function_name: "git_log".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else {
                Ok(crate::ai::AiResponse {
                    content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    usage: None,
                    truncated: false,
                })
            }
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    struct MockProviderNonConsecutiveDuplicate {
        turn: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockProviderNonConsecutiveDuplicate {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
            if turn == 0 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_1".to_string(),
                        function_name: "git_log".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else if turn == 1 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_2".to_string(),
                        function_name: "git_ls".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else if turn == 2 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_3".to_string(),
                        function_name: "git_log".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else {
                Ok(crate::ai::AiResponse {
                    content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    usage: None,
                    truncated: false,
                })
            }
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_duplicate_tool_call_blocked() {
        let temp_dir = tempfile::tempdir().unwrap();
        let provider = std::sync::Arc::new(MockProviderDuplicateCalls {
            turn: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let mut session = ReviewStageSession::new(
            create_stage(1),
            "sys".to_string(),
            "user".to_string(),
            "user".to_string(),
            std::sync::Arc::new(tools),
            0.0,
            None,
        );
        let runner = SessionRunner::new(provider.as_ref()).with_max_validation_attempts(3);

        let res = runner.run(&mut session).await;

        assert!(res.is_ok());
        let result = res.unwrap();
        let stage_history = result.history;
        assert_eq!(stage_history.len(), 6);

        let blocked_msg = &stage_history[4];
        assert_eq!(blocked_msg.role, AiRole::Tool);
        let content = blocked_msg.content.as_ref().unwrap();
        assert!(content.contains("Duplicate tool call blocked"));
    }

    #[tokio::test]
    async fn test_non_consecutive_duplicate_allowed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let provider = std::sync::Arc::new(MockProviderNonConsecutiveDuplicate {
            turn: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let mut session = ReviewStageSession::new(
            create_stage(1),
            "sys".to_string(),
            "user".to_string(),
            "user".to_string(),
            std::sync::Arc::new(tools),
            0.0,
            None,
        );
        let runner = SessionRunner::new(provider.as_ref()).with_max_validation_attempts(3);

        let res = runner.run(&mut session).await;

        assert!(res.is_ok());
        let result = res.unwrap();
        let stage_history = result.history;
        assert_eq!(stage_history.len(), 8);

        let response_msg = &stage_history[6];
        assert_eq!(response_msg.role, AiRole::Tool);
        let content = response_msg.content.as_ref().unwrap();
        assert!(!content.contains("Duplicate tool call detected"));
    }

    struct MockBlockedProvider {
        attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockBlockedProvider {
        async fn generate_content(
            &self,
            request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                anyhow::bail!(
                    "Remote AI Error: Gemini candidate blocked (finish reason: RECITATION)"
                )
            } else {
                let has_filter = request.messages.iter().any(|m| {
                    m.role == crate::ai::AiRole::User
                        && m.content
                            .as_ref()
                            .is_some_and(|c| c.contains("recitation filter"))
                });
                if has_filter {
                    return Ok(crate::ai::AiResponse {
                        content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        usage: None,
                        truncated: false,
                    });
                }
                anyhow::bail!(
                    "Remote AI Error: Gemini candidate blocked again (finish reason: RECITATION)"
                )
            }
        }

        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_recitation_error_triggers_prompt_perturbation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockBlockedProvider {
            attempts: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            custom_prompt: None,
            stages: Some(vec![1]),
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        let res = worker.run(patchset, None).await;
        if let Err(e) = &res {
            panic!("Expected run to succeed, got error: {:?}", e);
        }
    }
}
