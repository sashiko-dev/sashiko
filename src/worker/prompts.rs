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
    AiErrorClass, AiMessage, AiProvider, AiResponse, AiTool, ClassifyAiError, ErrorAction,
    LlmSession, SessionRunner, ValidationError,
};
use crate::toolbox::ToolBox;
use crate::worker::stage::ReviewStage;
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

use crate::worker::kernel_workflow::{
    KernelReviewState, build_kernel_review_workflow_with_options, kernel_system_prompt,
};
use crate::workflow::{WorkflowEngine, WorkflowEnv, WorkflowEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

/// System identity prompt - used across all AI interactions
pub const SYSTEM_IDENTITY: &str = "";

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
    pub baseline_sha: Option<String>,
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
    pub base_dir: PathBuf,
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
3. SERIES VALIDATION RULE: If follow-up patches in this series are provided in the context, check if each identified concern is resolved or fixed in the final state of the series. If the problem has been resolved, fixed, or the code was rewritten in a subsequent patch in this series, you MUST discard the concern and NOT report it as a finding. You MUST verify this by checking the actual code at the end of the series using tools; do not trust promises or claims in commit messages.
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

    /// Append the same per-stage guide files that [`Self::get_stage_prompt`]
    /// appends, for pipelines that supply their own instruction text via
    /// `StagePrompt::Override` (which bypasses `get_stage_prompt`).
    pub async fn append_stage_guides(
        &self,
        stage: u8,
        content: &mut String,
        clean: &mut String,
    ) -> Result<()> {
        let mut clean_files = Vec::new();
        match stage {
            3 => {
                self.append_file(content, &mut clean_files, "callstack.md")
                    .await?;
                self.append_file(content, &mut clean_files, "technical-patterns.md")
                    .await?;
            }
            5 => {
                self.append_file(content, &mut clean_files, "subsystem/locking.md")
                    .await?;
            }
            10 => {
                self.append_file(content, &mut clean_files, "false-positive-guide.md")
                    .await?;
                self.append_file(content, &mut clean_files, "severity.md")
                    .await?;
            }
            11 => {
                self.append_file(content, &mut clean_files, "inline-template.md")
                    .await?;
            }
            _ => {}
        }
        if !clean_files.is_empty() {
            clean.push_str(&clean_files.join(", "));
            clean.push_str("\n\n");
        }
        Ok(())
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
    baseline_sha: Option<String>,
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
            baseline_sha: config.baseline_sha,
            context_tag: None,
            stages: config.stages,
        }
    }

    pub async fn run(
        &mut self,
        patchset: Value,
        progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
    ) -> Result<WorkerResult> {
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

        let baseline_sha = self
            .baseline_sha
            .clone()
            .or_else(|| {
                patchset
                    .get("baseline")
                    .and_then(|b| b.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                self.series_range.as_ref().and_then(|range| {
                    let parts: Vec<&str> = range.split("..").collect();
                    if !parts.is_empty() && !parts[0].is_empty() {
                        Some(parts[0].to_string())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| "unknown".to_string());

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
            let target_patches: Vec<&Value> = if let Some(idx) = patchset["patch_index"].as_i64() {
                let filtered: Vec<&Value> = patches
                    .iter()
                    .filter(|p| p["index"].as_i64() == Some(idx))
                    .collect();
                if filtered.is_empty() {
                    patches.iter().collect()
                } else {
                    filtered
                }
            } else {
                patches.iter().collect()
            };

            for p in target_patches {
                let diff_body = p["diff"].as_str().unwrap_or("");
                let changelog_opt = crate::patch::extract_changelog_from_body(diff_body);

                if let Some(show) = p["git_show"].as_str() {
                    if let Some(ref changelog) = changelog_opt {
                        let enriched_show =
                            crate::patch::inject_changelog_into_git_show(show, changelog);
                        target_commit_diff.push_str(&enriched_show);
                    } else {
                        target_commit_diff.push_str(show);
                    }
                    target_commit_diff.push('\n');
                } else {
                    target_commit_diff.push_str(diff_body);
                    target_commit_diff.push('\n');
                }

                if let Some(diff) = p["diff"].as_str() {
                    target_commit_diff_only.push_str(diff);
                    target_commit_diff_only.push('\n');
                }
            }
        }

        let worktree_path = self.tools.get_worktree_path();
        let prefetched_context =
            crate::worker::prefetch::prefetch_context(worktree_path, &target_commit_diff)
                .await
                .unwrap_or_default();

        let follow_up_series_context = build_follow_up_series_context(
            self.series_range.as_deref(),
            &patchset,
            &target_commit_sha,
        );

        let mut state = KernelReviewState {
            ps_id,
            p_id,
            target_commit_sha,
            baseline_sha,
            target_commit_diff,
            target_commit_diff_only,
            prefetched_context,
            series_range: self.series_range.clone(),
            follow_up_series_context,
            selected_guides: Vec::new(),
            manual_stages: self.stages.clone(),
            planned_stages: Vec::new(),
            all_concerns: Vec::new(),
            all_dismissed_concerns: Vec::new(),
            deduplicated_concerns: Vec::new(),
            deduplicated_dismissed_concerns: Vec::new(),
            conflict_resolved_concerns: Vec::new(),
            findings: Vec::new(),
            review_inline: String::new(),
            fixes: String::new(),
        };

        if self.global_history.is_empty() {
            let sys_template = kernel_system_prompt(true);
            let rendered_sys = sys_template.render_for_log(&state);
            self.global_history.push(AiMessage {
                role: crate::ai::AiRole::System,
                content: Some(rendered_sys),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        let workflow =
            build_kernel_review_workflow_with_options(self.max_interactions, self.temperature);
        let env = WorkflowEnv {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            base_dir: &self.prompts.base_dir,
            context_tag: self.context_tag.clone(),
        };

        let event_cb = move |event: WorkflowEvent| {
            if let Some(progress_cb) = progress {
                match event {
                    WorkflowEvent::StageStarted { stage_name } => {
                        if stage_name == "stage_0_prescreen" {
                            progress_cb(WorkerProgressEvent::PreScreenStarted);
                        } else if stage_name == "stage_planning" {
                            progress_cb(WorkerProgressEvent::PlanningStarted);
                        } else if let Some(num) = parse_stage_number(stage_name) {
                            progress_cb(WorkerProgressEvent::StageStarted { stage: num });
                        }
                    }
                    WorkflowEvent::ParallelResolved { stage_names } => {
                        progress_cb(WorkerProgressEvent::ReviewStarted {
                            planned_stages: planned_stages_from(&stage_names),
                        });
                    }
                    WorkflowEvent::StageFinished { stage_name, .. } => {
                        if let Some(num) = parse_stage_number(stage_name) {
                            progress_cb(WorkerProgressEvent::StageFinished { stage: num });
                        }
                    }
                    WorkflowEvent::StageTurn {
                        stage_name,
                        turn,
                        max_turns,
                    } => {
                        if let Some(num) = parse_stage_number(stage_name) {
                            progress_cb(WorkerProgressEvent::StageTurn {
                                stage: num,
                                turn,
                                max_turns,
                            });
                        }
                    }
                    _ => {}
                }
            }
        };

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, Some(&event_cb)).await?;
        self.global_history.extend(outcome.history.clone());

        let concerns_count = state.all_concerns.len();
        let dismissed_concerns = if !state.deduplicated_dismissed_concerns.is_empty() {
            state.deduplicated_dismissed_concerns.clone()
        } else {
            state.all_dismissed_concerns.clone()
        };
        let dismissed_concerns_count = dismissed_concerns.len();

        let review_inline = if state.review_inline.is_empty() {
            "No issues found.".to_string()
        } else {
            state.review_inline
        };

        let final_output = json!({
            "findings": state.findings,
            "dismissed_concerns": dismissed_concerns,
            "review_inline": review_inline,
            "fixes": state.fixes,
            "concerns_count": concerns_count,
            "dismissed_concerns_count": dismissed_concerns_count,
        });

        Ok(WorkerResult {
            output: Some(final_output),
            error: None,
            input_context: "Multi-stage execution completed".to_string(),
            history: self.global_history.clone(),
            history_before_pruning: self.global_history.clone(),
            history_after_pruning: self.global_history.clone(),
            tokens_in: outcome.tokens_in,
            tokens_out: outcome.tokens_out,
            tokens_cached: outcome.tokens_cached,
        })
    }
}

/// The stages a review will run: the analysis stages the fan-out resolved, then
/// the four that always follow them. Nothing resolved means nothing planned,
/// not a bare tail.
fn planned_stages_from(stage_names: &[&'static str]) -> Vec<u8> {
    let mut planned: Vec<u8> = stage_names
        .iter()
        .filter_map(|n| parse_stage_number(n))
        .collect();
    if !planned.is_empty() {
        planned.extend([8, 9, 10, 11]);
    }
    planned
}

fn parse_stage_number(name: &str) -> Option<u8> {
    if let Some(rest) = name.strip_prefix("stage_")
        && let Some(num_str) = rest.split('_').next()
    {
        return num_str.parse().ok();
    }
    None
}

/// Run a single review stage through the shared `SessionRunner` machinery.
///
/// Additive helper used by the general pipeline executor (`crate::pipelines`).
/// It constructs the module-private `ReviewStageSession` exactly like
/// the legacy stage runner, so alternate pipelines reuse the identical tool
/// loop, validation, and recitation handling.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_review_stage(
    provider: &dyn AiProvider,
    tools: std::sync::Arc<ToolBox>,
    temperature: f32,
    max_interactions: usize,
    context_tag: Option<&str>,
    stage: Box<dyn ReviewStage>,
    system_prompt: String,
    user_prompt: String,
    clean_user_prompt: String,
    progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
) -> Result<crate::ai::session::SessionResult<serde_json::Value>> {
    let stage_num = stage.number();
    let mut session = ReviewStageSession::new(
        stage,
        system_prompt,
        user_prompt,
        clean_user_prompt,
        tools,
        temperature,
        context_tag,
    );
    let runner = SessionRunner::new(provider)
        .with_max_validation_attempts(3)
        .with_max_turns(max_interactions)
        .with_turn_callback(move |turn, max_turns| {
            if let Some(cb) = progress {
                cb(WorkerProgressEvent::StageTurn {
                    stage: stage_num,
                    turn,
                    max_turns,
                });
            }
        });
    runner.run(&mut session).await
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

pub fn build_follow_up_series_context(
    series_range: Option<&str>,
    patchset: &Value,
    target_commit_sha: &str,
) -> Option<String> {
    let range = series_range?;
    let end_sha = range.split("..").nth(1)?;
    if end_sha.is_empty() {
        return None;
    }

    let current_idx = patchset["patch_index"].as_i64().unwrap_or(1);
    let patches = patchset["patches"].as_array()?;
    let total_patches = patches.len();

    let current_subject = patches
        .iter()
        .find(|p| p["index"].as_i64() == Some(current_idx))
        .and_then(|p| p["subject"].as_str())
        .unwrap_or("unknown");

    let mut follow_ups = Vec::new();
    for p in patches {
        let idx = p["index"].as_i64().unwrap_or(0);
        if idx > current_idx {
            let subj = p["subject"].as_str().unwrap_or("");
            let commit_id = p["commit_id"].as_str();
            follow_ups.push((idx, commit_id, subj));
        }
    }

    if follow_ups.is_empty() {
        return None;
    }

    follow_ups.sort_by_key(|(idx, _, _)| *idx);

    let mut block = String::new();
    block.push_str("\n\n=== Follow-Up Patches in Series ===\n");
    block.push_str(&format!(
        "Current Patch Under Review: [Patch {} of {}] - {}\n",
        current_idx, total_patches, current_subject
    ));
    block.push_str(&format!("Series End Commit (Final State): {}\n\n", end_sha));
    block.push_str("Subsequent patches in this series:\n");

    for (idx, commit_id, subj) in follow_ups {
        if let Some(sha) = commit_id {
            block.push_str(&format!(
                "- [Patch {} of {}] (commit {}): {}\n",
                idx, total_patches, sha, subj
            ));
        } else {
            block.push_str(&format!(
                "- [Patch {} of {}]: {}\n",
                idx, total_patches, subj
            ));
        }
    }

    let diff_directive = if target_commit_sha != "unknown" && !target_commit_sha.is_empty() {
        format!(
            "Use tools (e.g., git_diff with base_revision=\"{}\", target_revision=\"{}\", or git_read_files with revision=\"{}\") to inspect the final code state at the end of the series.",
            target_commit_sha, end_sha, end_sha
        )
    } else {
        format!(
            "Use tools (e.g., git_diff with target_revision=\"{}\", or git_read_files with revision=\"{}\") to inspect the final code state at the end of the series.",
            end_sha, end_sha
        )
    };

    block.push_str("\nSERIES VERIFICATION DIRECTIVE:\n");
    block.push_str(&format!(
        "Verify if any candidate concern raised against this patch is fixed, refactored, or resolved in the subsequent patches listed above. {} If a concern is resolved by follow-up patches in this series, discard it as a false positive.\n",
        diff_directive
    ));
    block.push_str("===================================\n");

    Some(block)
}

#[cfg(test)]
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

#[cfg(test)]
fn append_stage_dismissed_concerns(target: &mut Vec<Value>, items: &[Value], stage: u8) {
    append_stage_items(target, items, stage, "General", "description");
}

#[cfg(test)]
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
    use crate::ai::AiRole;
    use crate::worker::stage::create_stage;

    #[test]
    fn test_planned_stages_follow_the_resolved_fan_out() {
        assert_eq!(
            planned_stages_from(&["stage_1", "stage_2", "stage_5"]),
            vec![1, 2, 5, 8, 9, 10, 11]
        );
        assert_eq!(planned_stages_from(&[]), Vec::<u8>::new());
        assert_eq!(planned_stages_from(&["stage_planning"]), Vec::<u8>::new());
    }

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

    #[test]
    fn test_build_follow_up_series_context_none_when_no_range() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [{
                "index": 1,
                "subject": "Single patch",
                "commit_id": "sha1"
            }]
        });
        assert_eq!(
            build_follow_up_series_context(None, &patchset, "sha1"),
            None
        );
    }

    #[test]
    fn test_build_follow_up_series_context_none_when_last_patch() {
        let patchset = serde_json::json!({
            "patch_index": 2,
            "patches": [
                { "index": 1, "subject": "Patch 1", "commit_id": "sha1" },
                { "index": 2, "subject": "Patch 2", "commit_id": "sha2" }
            ]
        });
        assert_eq!(
            build_follow_up_series_context(Some("base..sha2"), &patchset, "sha2"),
            None
        );
    }

    #[test]
    fn test_build_follow_up_series_context_intermediate_patch() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [
                { "index": 1, "subject": "net: add foo API", "commit_id": "sha1" },
                { "index": 2, "subject": "net: add caller for foo", "commit_id": "sha2" },
                { "index": 3, "subject": "net: add docs for foo", "commit_id": "sha3" }
            ]
        });
        let ctx = build_follow_up_series_context(Some("base..sha3"), &patchset, "sha1");
        assert!(ctx.is_some());
        let content = ctx.unwrap();
        assert!(content.contains("Current Patch Under Review: [Patch 1 of 3] - net: add foo API"));
        assert!(content.contains("Series End Commit (Final State): sha3"));
        assert!(content.contains("- [Patch 2 of 3] (commit sha2): net: add caller for foo"));
        assert!(content.contains("- [Patch 3 of 3] (commit sha3): net: add docs for foo"));
        assert!(!content.contains("- [Patch 1 of 3]"));
        assert!(content.contains("SERIES VERIFICATION DIRECTIVE:"));
        assert!(content.contains("base_revision=\"sha1\""));
        assert!(content.contains("target_revision=\"sha3\""));
        assert!(content.contains("git_read_files with revision=\"sha3\""));
    }

    #[test]
    fn test_build_follow_up_series_context_unordered_patches() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [
                { "index": 3, "subject": "Patch 3", "commit_id": "sha3" },
                { "index": 1, "subject": "Patch 1", "commit_id": "sha1" },
                { "index": 2, "subject": "Patch 2", "commit_id": "sha2" }
            ]
        });
        let ctx = build_follow_up_series_context(Some("base..sha3"), &patchset, "sha1");
        assert!(ctx.is_some());
        let content = ctx.unwrap();
        let p2_pos = content.find("Patch 2 of 3").unwrap();
        let p3_pos = content.find("Patch 3 of 3").unwrap();
        assert!(
            p2_pos < p3_pos,
            "Patch 2 should appear before Patch 3 in follow-up list"
        );
    }

    #[test]
    fn test_build_follow_up_series_context_unknown_target_sha() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [
                { "index": 1, "subject": "Patch 1" },
                { "index": 2, "subject": "Patch 2", "commit_id": "sha2" }
            ]
        });
        let ctx = build_follow_up_series_context(Some("base..sha2"), &patchset, "unknown");
        assert!(ctx.is_some());
        let content = ctx.unwrap();
        assert!(!content.contains("base_revision=\"unknown\""));
        assert!(content.contains("target_revision=\"sha2\""));
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
            baseline_sha: None,
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
            baseline_sha: None,
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

    #[tokio::test]
    async fn test_baseline_sha_in_worker_context_when_single_patch() {
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
            baseline_sha: Some("explicit_baseline_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec![1]),
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;", "commit_id": "target_sha"}]
        });

        let res = worker.run(patchset, None).await;
        assert!(res.is_ok());
        let worker_res = res.unwrap();
        assert!(!worker_res.history.is_empty());
        // System prompt contains the Active Git Metadata:
        let sys_content = worker_res.history[0].content.as_deref().unwrap_or_default();
        assert!(sys_content.contains("Baseline SHA: explicit_baseline_sha"));
        assert!(sys_content.contains("Target Commit SHA: target_sha"));
    }

    #[tokio::test]
    async fn test_multi_patch_worker_context_only_contains_target_patch_diff() {
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
            series_range: Some("base_sha..sha2".to_string()),
            baseline_sha: Some("base_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec![1]),
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 100,
            "patch_index": 1,
            "patches": [
                {
                    "index": 1,
                    "subject": "Patch 1 Subject",
                    "diff": "diff --git a/file1.c b/file1.c\n+int patch1_unique_symbol;",
                    "commit_id": "sha1"
                },
                {
                    "index": 2,
                    "subject": "Patch 2 Subject",
                    "diff": "diff --git a/file2.c b/file2.c\n+int patch2_unique_symbol;",
                    "commit_id": "sha2"
                }
            ]
        });

        let res = worker.run(patchset, None).await;
        assert!(res.is_ok());
        let worker_res = res.unwrap();
        assert!(!worker_res.history.is_empty());
        let sys_content = worker_res.history[0].content.as_deref().unwrap_or_default();
        assert!(sys_content.contains("Target Commit SHA: sha1"));
        assert!(sys_content.contains("patch1_unique_symbol"));
        assert!(!sys_content.contains("patch2_unique_symbol"));
    }

    struct MockMultiStageSeriesProvider;

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockMultiStageSeriesProvider {
        async fn generate_content(
            &self,
            request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let last_user = request
                .messages
                .iter()
                .rfind(|m| m.role == crate::ai::AiRole::User)
                .and_then(|m| m.content.as_deref())
                .unwrap_or_default();

            let content = if last_user.contains("# Stage 1.") || last_user.contains("# Stage 8.") {
                r#"{"concerns": [{"type": "Bug", "description": "some issue", "reasoning": "reason", "preexisting": false, "locations": []}], "dismissed_concerns": []}"#
            } else if last_user.contains("# Stage 9.") {
                r#"{"concerns": [{"type": "Bug", "description": "some issue", "reasoning": "reason", "preexisting": false, "locations": []}]}"#
            } else if last_user.contains("# Stage 10.") {
                r#"{"findings": []}"#
            } else {
                r#"{"concerns": [], "dismissed_concerns": []}"#
            };

            Ok(crate::ai::AiResponse {
                content: Some(content.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
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
    async fn test_stage_10_log_history_contains_follow_up_series_context() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockMultiStageSeriesProvider);
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: Some("base_sha..sha2".to_string()),
            baseline_sha: Some("base_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec![1]),
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 200,
            "patch_index": 1,
            "patches": [
                {
                    "index": 1,
                    "subject": "Patch 1 Subject",
                    "diff": "diff --git a/file1.c b/file1.c\n+int patch1;",
                    "commit_id": "sha1"
                },
                {
                    "index": 2,
                    "subject": "Patch 2 Subject",
                    "diff": "diff --git a/file2.c b/file2.c\n+int patch2;",
                    "commit_id": "sha2"
                }
            ]
        });

        let res = worker.run(patchset, None).await;
        assert!(res.is_ok());
        let worker_res = res.unwrap();
        assert!(!worker_res.history.is_empty());

        let stage10_user_msg = worker_res
            .history
            .iter()
            .find(|m| {
                m.role == crate::ai::AiRole::User
                    && m.content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("# Stage 10.")
            })
            .expect("Stage 10 user message should be in history");

        let content = stage10_user_msg.content.as_deref().unwrap();
        assert!(content.contains("=== Follow-Up Patches in Series ==="));
        assert!(content.contains("Series End Commit (Final State): sha2"));
        assert!(content.contains("- [Patch 2 of 2] (commit sha2): Patch 2 Subject"));
        assert!(content.contains("SERIES VERIFICATION DIRECTIVE:"));
    }
}
