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

use crate::ai::token_budget::TokenBudget;
use crate::ai::{
    AiErrorClass, AiMessage, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiTool,
    AiUsage, ClassifyAiError, ToolCall,
};
use crate::review_budget::{is_token_budget_failure_message, local_prompt_preflight_cap};
use crate::worker::stage9::*;
use crate::worker::tools::ToolBox;
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
    /// The provider stopped before a complete response was produced.
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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tracing::{info, warn};

/// System identity prompt - used across all AI interactions
pub const SYSTEM_IDENTITY: &str = "";

/// Subsystem guides that are loaded per-stage in get_stage_prompt() and should
/// be excluded from Phase 0's shared context to avoid double-counting.
const STAGE_EXCLUSIVE_GUIDES: &[&str] = &["locking.md"];
const LOCAL_MAX_SELECTED_GUIDES: usize = 4;
const LOCAL_MAX_GUIDE_CONTEXT_TOKENS: usize = 12_000;
const LOCAL_SKIP_EXPLORATION_PERCENT: usize = 85;
const LOCAL_JSON_CORRECTION_MAX_CONTENT_CHARS: usize = 16_000;
const MAX_REPAIRED_STAGE9_FINDINGS: usize = 10;
const MINIMAL_FALLBACK_MAX_FINDINGS: usize = 3;
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

fn validate_inline_format(content: &str) -> std::result::Result<(), String> {
    if content.lines().any(|l| l.trim_start().starts_with("```")) {
        return Err("The output contains Markdown code blocks ('```'). It must be plain text as per `inline-template.md`.".to_string());
    }
    if !content.lines().any(|l| l.trim_start().starts_with(">")) {
        return Err("The output does not appear to quote any code or context using '>'. Please follow the quoting style in `inline-template.md`.".to_string());
    }
    let has_commit_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("commit "));
    if !has_commit_header {
        return Err("The output is missing the 'commit <hash>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_author_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("author:"));
    if !has_author_header {
        return Err("The output is missing the 'Author: <name>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_comments = content.lines().any(|l| {
        let trimmed = l.trim();
        if trimmed.is_empty() || trimmed.starts_with(">") {
            return false;
        }
        let lower = trimmed.to_lowercase();
        !lower.starts_with("commit ")
            && !lower.starts_with("author:")
            && !lower.starts_with("date:")
            && !lower.starts_with("link:")
    });
    if !has_comments {
        return Err("The output appears to lack any comments or summary. You must include a summary and interspersed comments explaining the findings.".to_string());
    }
    Ok(())
}
pub struct WorkerConfig {
    pub max_input_tokens: usize,
    pub max_interactions: usize,
    pub temperature: f32,
    pub custom_prompt: Option<String>,
    pub series_range: Option<String>,
    pub stages: Option<Vec<u8>>,
    pub stage_protocol: StageProtocol,
    pub enable_static_bug_seeds: bool,
    pub enable_targeted_bug_pattern_prescan: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageProtocol {
    Native,
    BoundedLocalModel,
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

#[derive(Clone)]
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
        content.push_str("<global_review_guidelines>\n");
        content.push_str("The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.\n\n");

        clean.push_str(&date_fact);
        clean.push_str("You are an expert Linux kernel maintainer. Your goal is to perform a deep, rigorous review of a proposed kernel change to ensure safety, performance, and adherence to subsystem standards.\n\n");
        clean.push_str("TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.\n\n");
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
7. Sequence counters: Are stats accumulations directly inside a `u64_stats_fetch_retry` loop leading to double counting? Is it possible for an interrupt to read a sequence counter while the interrupted context is modifying it (deadlock)? If a patch removes local_irq_save/local_irq_restore around a write_seqcount_begin/write_seqcount_end section, do not treat an irq-safe callee as proof that the whole sequence-counter write side is interrupt-safe; prove that no interrupt-context reader can spin or retry on the same seqcount.
8. Lock re-initialization: Does it re-initialize a lock that was already initialized, or destroy a lock on a failure path improperly?
9. Missing locking: Is a port or file exposed to userspace before the driver/TTY linking is complete? Does a worker race with cleanup code leading to dropped/leaked frames?"
            }
            6 => {
                "# Stage 6. Security audit

You are a Red Team security researcher auditing a Linux kernel patch. Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, time-of-check to time-of-use (TOCTOU) races, and information leaks (e.g., copying uninitialized kernel memory to user-space via copy_to_user). Scrutinize all points where untrusted user input reaches sensitive functions without validation. Ensure all length checks and bounds checks are robust against malicious input. Focus heavily on attack surfaces and data boundaries."
            }
            7 => {
                "# Stage 7. Hardware engineer's review

You are a hardware engineer reviewing device driver changes. If this patch touches driver or hardware-specific code, rigorously review register accesses, IRQ handling, DMA mapping/unmapping, memory barriers, and timing/delays. Look for missing dma_wmb()/dma_rmb() barriers, incorrect endianness conversions (cpu_to_le32), and unsafe DMA buffer allocations. Ensure the hardware state machine is handled correctly, especially during suspend/resume or device reset. Evaluate the physical state machine constraints: verify that clocks and power domains are enabled before registers are accessed, and that hardware rings/queues are actually initialized in the current hardware state before being unconditionally accessed. If the patch is purely generic software logic (e.g., VFS, core networking), output an empty concerns list."
            }
            8 => {
                "# Stage 8. Deduplication and Consolidation

You are the lead reviewer consolidating feedback from multiple specialized analysts. You will be given a list of concerns generated by different review stages.
Your task is to deduplicate identical or overlapping concerns.
1. Group concerns that refer to the same root cause or the same line of code.
2. Merge overlapping concerns into a single, comprehensive concern. Combine their reasonings if they complement each other.
3. Ensure the output contains only unique concerns.
4. Preserve the `preexisting` flag. If you merge a pre-existing concern with a newly introduced one, flag it based on the root cause (if the root cause is new, it's not pre-existing).
5. SPECIFICITY REQUIREMENT: When merging concerns, preserve and consolidate the most specific details: exact function names, file paths, line numbers when known, and triggering conditions. Never generalize a specific finding into a vague category.
6. Preserve and merge the `locations` arrays from the input concerns. If multiple items describe the same root cause, keep the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters locations. Do not invent line numbers; keep `line` as null when the exact line is not known."
            }
            9 => {
                "# Stage 9. Verification and severity estimation

You are the lead reviewer validating consolidated concerns. You will be given a list of deduplicated concerns.
1. Validate each concern and prove the provided reasoning. Report all valid concerns as findings. If necessary, use tools to gather additional material. Discard all false positives.
2. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning. If you cannot find definitive proof that the concern is a false positive, it must be reported as a finding. If you're not sure about something and it's critical in the reasoning validation, make it obvious: if X is possible, then problem Y can occur. Always try to validate if X is possible yourself.
3. If context from subsequent patches in the series is provided, check if the concern is fixed later in the series. If so, discard it. But don't trust any promises in the commit message if they can't be verified (e.g. something will be fixed by subsequent patches in the series - if you can't prove that it's indeed fixed, report it as a bug).
4. When referring to other patches within this series in your explanation, DO NOT use git hashes (they are ephemeral/unstable). Instead, refer to them by their patch subject (e.g., 'commit \"mm: fix allocation\"'). Existing historical commits in the tree should still be referenced by their standard hash.
5. Assign a severity (low, medium, high, critical) to each remaining valid finding and explain the reasoning. Be rigorous in filtering out verifiable noise, but accurately report real logic flaws and edge cases.
6. If the problem did exist in the code before the patch was applied, say it explicitly: 'This problem wasn't introduced by this patch, but...'. Discard low- and medium-severity pre-existing problems, report only high- and critical severity issues.
7. SPECIFICITY REQUIREMENT: Every finding MUST cite the exact function name(s), file path(s), line number(s) when known, and triggering conditions where the bug manifests. Vague descriptions like 'potential overflow in ring buffer calculations' are insufficient. State precisely which variable overflows, in which function, and under what input conditions. Do not invent line numbers; use `line: null` when the exact line is not known.
8. Carry forward the `locations` from the validated concern into each finding. If you gather better evidence, replace vague locations with the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters locations you verified."
            }
            10 => {
                "# Stage 10. LKML-friendly report generation

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
            9 => {
                self.append_file(&mut content, &mut clean_files, "false-positive-guide.md")
                    .await?;
                self.append_file(&mut content, &mut clean_files, "severity.md")
                    .await?;
            }
            10 => {
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
        format!("{:x}", hasher.finalize())
    }
}

pub struct Worker {
    provider: Arc<dyn AiProvider>,
    tools: Arc<ToolBox>,
    prompts: PromptRegistry,
    global_history: Vec<AiMessage>,
    max_input_tokens: usize,
    max_interactions: usize,
    temperature: f32,
    series_range: Option<String>,
    context_tag: Option<String>,
    stages: Option<Vec<u8>>,
    stage_protocol: StageProtocol,
    enable_static_bug_seeds: bool,
    enable_targeted_bug_pattern_prescan: bool,
}

#[derive(Clone, Copy, Default)]
struct TokenTotals {
    input: u32,
    output: u32,
    cached: u32,
}

struct StandardStageResult {
    stage: u8,
    concerns: Vec<Value>,
    tokens_in: u32,
    tokens_out: u32,
    tokens_cached: u32,
    history: Vec<AiMessage>,
}

impl TokenTotals {
    fn as_tuple(self) -> (u32, u32, u32) {
        (self.input, self.output, self.cached)
    }

    fn replace_with_tuple(&mut self, tokens: (u32, u32, u32)) {
        self.input = tokens.0;
        self.output = tokens.1;
        self.cached = tokens.2;
    }

    fn add_usage(&mut self, input: u32, output: u32, cached: u32) {
        self.input += input;
        self.output += output;
        self.cached += cached;
    }
}

struct ReviewContexts {
    shared_context: String,
    clean_shared_context: String,
    shared_context_no_log: String,
    clean_shared_context_no_log: String,
    dynamic_context_no_log: String,
    clean_dynamic_context_no_log: String,
    base_dynamic_context_no_log: String,
    base_clean_dynamic_context_no_log: String,
}

struct Stage8Result {
    deduplicated_concerns: Value,
    input_concerns_count: usize,
    output_concerns_count: usize,
    dropped_concerns: Value,
}

struct Stage9Result {
    findings: Value,
    input_concerns_count: usize,
    dropped_candidates: Value,
}

struct DirectResponseContext<'a> {
    stage: u8,
    system_prompt: &'a str,
    progress_tool_calls_seen: usize,
}

struct BoundedToolTurn<'a> {
    stage: u8,
    policy: &'a ExplorationPolicy,
    force_finalize_due_to_context: bool,
}

enum Stage9Run {
    Completed(Stage9Result),
    Fallback(WorkerResult),
}

impl Worker {
    pub fn new(
        provider: Arc<dyn AiProvider>,
        tools: impl Into<Arc<ToolBox>>,
        prompts: PromptRegistry,
        config: WorkerConfig,
    ) -> Self {
        Self {
            provider,
            tools: tools.into(),
            prompts,
            global_history: Vec::new(),
            max_input_tokens: config.max_input_tokens,
            max_interactions: config.max_interactions,
            temperature: config.temperature,
            series_range: config.series_range,
            context_tag: None,
            stages: config.stages,
            stage_protocol: config.stage_protocol,
            enable_static_bug_seeds: config.enable_static_bug_seeds,
            enable_targeted_bug_pattern_prescan: config.enable_targeted_bug_pattern_prescan,
        }
    }

    async fn checked_generate_content(
        &self,
        label: &str,
        request: AiRequest,
    ) -> Result<AiResponse> {
        let estimated = self.provider.estimate_tokens(&request);
        let preflight_cap = self.prompt_preflight_cap();
        if estimated > preflight_cap {
            return Err(ReviewError::BudgetExceeded(format!(
                "{label} prompt estimate {estimated} exceeds preflight cap {preflight_cap} (max_input_tokens {})",
                self.max_input_tokens
            ))
            .into());
        }
        match self.provider.generate_content(request).await {
            Ok(response) if response.truncated => Err(ReviewError::OutputTruncated.into()),
            Ok(response) => Ok(response),
            Err(e) if is_token_budget_failure_message(&e.to_string()) => {
                Err(ReviewError::BudgetExceeded(e.to_string()).into())
            }
            Err(e) => Err(e),
        }
    }

    fn request_estimate(&self, request: &AiRequest) -> usize {
        self.provider.estimate_tokens(request)
    }

    fn local_context_threshold(&self, percent: usize) -> usize {
        self.max_input_tokens.saturating_mul(percent) / 100
    }

    fn prompt_preflight_cap(&self) -> usize {
        if self.stage_protocol == StageProtocol::BoundedLocalModel {
            local_prompt_preflight_cap(self.max_input_tokens)
        } else {
            self.max_input_tokens
        }
    }

    fn validate_prompt_usage(&self, label: &str, prompt_tokens: usize) -> Result<()> {
        if prompt_tokens > self.max_input_tokens {
            return Err(ReviewError::BudgetExceeded(format!(
                "{label} actual prompt_tokens {prompt_tokens} exceeds max_input_tokens {}",
                self.max_input_tokens
            ))
            .into());
        }
        Ok(())
    }

    async fn limit_local_selected_prompts(&self, prompts: Vec<String>) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut limited: Vec<String> = prompts
            .into_iter()
            .filter(|name| !STAGE_EXCLUSIVE_GUIDES.contains(&name.as_str()))
            .filter(|name| seen.insert(name.clone()))
            .take(LOCAL_MAX_SELECTED_GUIDES)
            .collect();

        while !limited.is_empty() {
            match self.prompts.build_context(Some(&limited)).await {
                Ok((context, _)) => {
                    let tokens = TokenBudget::estimate_tokens(&context);
                    if tokens <= LOCAL_MAX_GUIDE_CONTEXT_TOKENS {
                        break;
                    }
                    warn!(
                        "Local Phase 0 guide context is {} tokens, over cap {}; dropping broadest selected guide",
                        tokens, LOCAL_MAX_GUIDE_CONTEXT_TOKENS
                    );
                    limited.pop();
                }
                Err(e) => {
                    warn!("Failed to estimate selected guide context: {}", e);
                    break;
                }
            }
        }

        info!("Local Phase 0 capped prompts: {:?}", limited);
        limited
    }

    fn collect_target_diffs(&mut self, patchset: &Value) -> (String, String) {
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

        (target_commit_diff, target_commit_diff_only)
    }

    async fn select_shared_guides(
        &mut self,
        target_commit_diff: &str,
        totals: &mut TokenTotals,
    ) -> Option<Vec<String>> {
        let subsystem_md_path = self.prompts.base_dir.join("subsystem/subsystem.md");
        let selected_prompts = if subsystem_md_path.exists() {
            match tokio::fs::read_to_string(&subsystem_md_path).await {
                Ok(subsystem_md) => {
                    info!("Executing Phase 0: Pre-screening relevant subsystem guides.");
                    let phase0_system = if self.stage_protocol == StageProtocol::BoundedLocalModel {
                        "You are an AI assistant preparing a Linux kernel patch review.\nReview the provided Patch and select the smallest useful set of subsystem guides from the index below.\nFor bounded local-model runs, select at most 4 guide filenames. Prefer the exact subsystem guide, then the closest parent subsystem guide, then at most one directly relevant cross-cutting guide. Exact guides beat many broad guides. Do not include broad or generic guides unless the patch directly needs them and budget remains.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"networking.md\", \"locking.md\"]}\n```"
                    } else {
                        "You are an AI assistant preparing a Linux kernel patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"networking.md\", \"locking.md\"]}\n```"
                    };
                    let phase0_prompt = format!(
                        "<subsystem_guide_index>\n{}\n</subsystem_guide_index>\n\n<patch>\n{}\n</patch>",
                        subsystem_md, target_commit_diff
                    );
                    let schema = json!({
                        "type": "OBJECT",
                        "properties": {
                            "selected_prompts": {
                                "type": "ARRAY",
                                "items": { "type": "STRING" }
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

                    let mut tokens = totals.as_tuple();
                    let val = self
                        .json_request("s0", req, &mut tokens, |v| {
                            v.get("selected_prompts")
                                .and_then(|v| v.as_array())
                                .ok_or_else(|| "missing 'selected_prompts' array".to_string())
                                .map(|_| ())
                        })
                        .await;
                    totals.replace_with_tuple(tokens);
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

        if self.stage_protocol != StageProtocol::BoundedLocalModel {
            return selected_prompts;
        }

        match selected_prompts {
            Some(prompts) => Some(self.limit_local_selected_prompts(prompts).await),
            None => {
                warn!(
                    "Phase 0 did not return selected guides in local mode; loading no shared subsystem guides"
                );
                Some(Vec::new())
            }
        }
    }

    async fn build_review_contexts(
        &self,
        target_commit_diff: &str,
        target_commit_diff_only: &str,
        selected_prompts: Option<&[String]>,
    ) -> Result<ReviewContexts> {
        let (static_context, clean_static_context) =
            self.prompts.build_context(selected_prompts).await?;

        let mut dynamic_context = String::new();
        dynamic_context.push_str("\n\nTarget Commit:\n");
        dynamic_context.push_str(target_commit_diff);
        let mut clean_dynamic_context = dynamic_context.clone();

        let mut dynamic_context_no_log = String::new();
        dynamic_context_no_log.push_str("\n\nTarget Commit Diff:\n");
        dynamic_context_no_log.push_str(target_commit_diff_only);
        let mut clean_dynamic_context_no_log = dynamic_context_no_log.clone();

        let base_dynamic_context = dynamic_context.clone();
        let base_clean_dynamic_context = clean_dynamic_context.clone();
        let base_dynamic_context_no_log = dynamic_context_no_log.clone();
        let base_clean_dynamic_context_no_log = clean_dynamic_context_no_log.clone();

        let worktree_path = self.tools.get_worktree_path();
        if let Ok(prefetched) =
            crate::worker::prefetch::prefetch_context(worktree_path, target_commit_diff).await
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

        let mut shared_context = format!("{}{}", static_context, dynamic_context);
        let mut clean_shared_context = format!("{}{}", clean_static_context, clean_dynamic_context);
        let mut shared_context_no_log = format!("{}{}", static_context, dynamic_context_no_log);
        let mut clean_shared_context_no_log =
            format!("{}{}", clean_static_context, clean_dynamic_context_no_log);

        if self.stage_protocol == StageProtocol::BoundedLocalModel
            && (TokenBudget::estimate_tokens(&shared_context) > self.max_input_tokens
                || TokenBudget::estimate_tokens(&shared_context_no_log) > self.max_input_tokens)
        {
            warn!(
                "Local context exceeds max_input_tokens {}; dropping shared guide context before model calls",
                self.max_input_tokens
            );
            let (minimal_static_context, minimal_clean_static_context) =
                self.prompts.build_context(Some(&[])).await?;
            shared_context = format!("{}{}", minimal_static_context, dynamic_context);
            clean_shared_context =
                format!("{}{}", minimal_clean_static_context, clean_dynamic_context);
            shared_context_no_log = format!("{}{}", minimal_static_context, dynamic_context_no_log);
            clean_shared_context_no_log = format!(
                "{}{}",
                minimal_clean_static_context, clean_dynamic_context_no_log
            );

            if TokenBudget::estimate_tokens(&shared_context) > self.max_input_tokens
                || TokenBudget::estimate_tokens(&shared_context_no_log) > self.max_input_tokens
            {
                warn!(
                    "Local context still exceeds max_input_tokens {}; dropping prefetched context before model calls",
                    self.max_input_tokens
                );
                shared_context = format!("{}{}", minimal_static_context, base_dynamic_context);
                clean_shared_context = format!(
                    "{}{}",
                    minimal_clean_static_context, base_clean_dynamic_context
                );
                shared_context_no_log =
                    format!("{}{}", minimal_static_context, base_dynamic_context_no_log);
                clean_shared_context_no_log = format!(
                    "{}{}",
                    minimal_clean_static_context, base_clean_dynamic_context_no_log
                );
            }
        }

        Ok(ReviewContexts {
            shared_context,
            clean_shared_context,
            shared_context_no_log,
            clean_shared_context_no_log,
            dynamic_context_no_log,
            clean_dynamic_context_no_log,
            base_dynamic_context_no_log,
            base_clean_dynamic_context_no_log,
        })
    }

    async fn prepare_run(
        &mut self,
        patchset: &Value,
    ) -> Result<(String, ReviewContexts, Option<Vec<u8>>, TokenTotals)> {
        let (target_commit_diff, target_commit_diff_only) = self.collect_target_diffs(patchset);
        let mut totals = TokenTotals::default();
        let selected_prompts = self
            .select_shared_guides(&target_commit_diff, &mut totals)
            .await;
        let contexts = self
            .build_review_contexts(
                &target_commit_diff,
                &target_commit_diff_only,
                selected_prompts.as_deref(),
            )
            .await?;
        let planned_stages = self
            .plan_review_stages(&contexts.shared_context, &mut totals)
            .await;

        Ok((target_commit_diff_only, contexts, planned_stages, totals))
    }

    async fn plan_review_stages(
        &mut self,
        shared_context: &str,
        totals: &mut TokenTotals,
    ) -> Option<Vec<u8>> {
        if self.stages.is_some() {
            return None;
        }

        let schema = serde_json::json!({
            "type": "OBJECT",
            "properties": {
                "relevant_stages": {
                    "type": "ARRAY",
                    "items": { "type": "INTEGER" },
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
        let mut tokens = totals.as_tuple();
        let val = self
            .json_request("sp", req, &mut tokens, |v| {
                v.get("relevant_stages")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| "missing 'relevant_stages' array".to_string())
                    .map(|_| ())
            })
            .await;
        totals.replace_with_tuple(tokens);

        let mut stages = vec![1, 2, 3];
        let arr = val
            .as_ref()
            .and_then(|val| val.get("relevant_stages"))
            .and_then(|stages| stages.as_array())?;
        for v in arr {
            if let Some(n) = v.as_u64()
                && (4..=7).contains(&n)
            {
                stages.push(n as u8);
            }
        }
        info!("Planning phase selected stages: {:?}", stages);
        Some(stages)
    }

    fn seed_static_concerns(&self, target_commit_diff_only: &str, all_concerns: &mut Vec<Value>) {
        if !self.enable_static_bug_seeds {
            return;
        }

        let static_bug_pattern_concerns =
            seed_bug_pattern_concerns_from_diff(target_commit_diff_only);
        if !static_bug_pattern_concerns.is_empty() {
            info!(
                "Seeded {} deterministic bug-pattern concern(s) from the diff",
                static_bug_pattern_concerns.len()
            );
            all_concerns.extend(static_bug_pattern_concerns);
        }

        let static_lifecycle_ordering_concerns =
            seed_lifecycle_ordering_concerns_from_diff(target_commit_diff_only);
        if !static_lifecycle_ordering_concerns.is_empty() {
            info!(
                "Seeded {} deterministic lifecycle-ordering concern(s) from the diff",
                static_lifecycle_ordering_concerns.len()
            );
            all_concerns.extend(static_lifecycle_ordering_concerns);
        }
    }

    async fn run_bug_pattern_prescan(
        &mut self,
        patch_context: &str,
        all_concerns: &mut Vec<Value>,
        totals: &mut TokenTotals,
    ) {
        if !self.enable_targeted_bug_pattern_prescan {
            return;
        }

        info!("Running early bug-pattern diff prescan");
        let bug_pattern_diff_prescan_prompt = r#"Run a narrow Linux kernel bug-pattern scan using only the target diff text below.

This is a diff-first pre-scan. Tools are disabled. Do not require external lookup when the diff already shows the changed function, call site, or file operation. Do not report other bug classes.

Check exactly these classes:

A. cgroup keyed file parsing / missing value
- Look for cgroup file write handlers, cftype entries such as .name = "max", and parsers such as limit_key_write().
- If input can be a bare key/sentinel such as "max" without a following value, flag any path that dereferences, strcmp()s, kstrto*()s, or otherwise parses the value pointer before proving it is non-NULL.
- Treat nested keyed files, limits, and "max" sentinel values as separate parsing cases.

B. RCU list iteration in unregister/teardown paths
- Look for unregister/remove/teardown functions, including names like region_unregister().
- If the function uses list_for_each_rcu(), hlist_for_each_entry_rcu(), list_del_rcu(), or another *_rcu traversal without a visible rcu_read_lock() or explicit lockdep/update-side proof in the same shown function, flag it.
- Teardown paths need concrete locking proof; do not assume normal caller locking.

C. skb fragment capacity / MAX_SKB_FRAGS
- Look for skb fragment append paths such as skb_add_rx_frag(), skb_shinfo(skb)->frags[nr_frags], or direct nr_frags increments.
- If the same shown function can append looped DMA/page fragments and has no visible preceding nr_frags < MAX_SKB_FRAGS or equivalent capacity check before appending, flag it.
- Mention MAX_SKB_FRAGS, nr_frags, and fragment array capacity in the concern.

Return ONLY a JSON object with a "concerns" array.
If none of these three classes is supported by the diff, return {"concerns":[]}.
Each concern must use "type", "description", "reasoning", "preexisting", and "locations"."#;

        let bug_pattern_schema = json!({
            "type": "object",
            "properties": {
                "concerns": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": true
                    }
                }
            },
            "required": ["concerns"],
            "additionalProperties": false
        });
        let bug_pattern_prescan_req = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: crate::ai::AiRole::User,
                content: Some(format!(
                    "{}\n\n{}",
                    patch_context, bug_pattern_diff_prescan_prompt
                )),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: Some(0.0),
            response_format: Some(AiResponseFormat::Json {
                schema: Some(bug_pattern_schema),
            }),
            context_tag: self
                .context_tag
                .as_ref()
                .map(|prefix| format!("{} s:12p] ", &prefix[..prefix.len() - 2])),
        };
        let mut tokens = totals.as_tuple();
        let val = self
            .json_request(
                "bug-pattern diff prescan",
                bug_pattern_prescan_req,
                &mut tokens,
                |v| {
                    v.get("concerns")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| "missing 'concerns' array".to_string())
                        .map(|_| ())
                },
            )
            .await;
        totals.replace_with_tuple(tokens);
        if let Some(val) = val {
            append_bug_pattern_concerns(all_concerns, &val, "bug_pattern_diff_prescan");
        }
    }

    fn worker_result_with_output(&self, output: Value, totals: TokenTotals) -> WorkerResult {
        WorkerResult {
            output: Some(output),
            error: None,
            input_context: "Multi-stage execution completed".to_string(),
            history: self.global_history.clone(),
            history_before_pruning: self.global_history.clone(),
            history_after_pruning: self.global_history.clone(),
            tokens_in: totals.input,
            tokens_out: totals.output,
            tokens_cached: totals.cached,
        }
    }

    async fn collect_concerns(
        &mut self,
        target_commit_diff_only: &str,
        contexts: &ReviewContexts,
        planned_stages: Option<&[u8]>,
        totals: &mut TokenTotals,
    ) -> Result<(Vec<Value>, Option<WorkerResult>)> {
        let mut all_concerns = Vec::new();
        self.seed_static_concerns(target_commit_diff_only, &mut all_concerns);
        self.run_bug_pattern_prescan(
            &contexts.base_dynamic_context_no_log,
            &mut all_concerns,
            totals,
        )
        .await;

        if let Some(result) = self
            .run_standard_review_stages(contexts, planned_stages, &mut all_concerns, totals)
            .await?
        {
            return Ok((all_concerns, Some(result)));
        }

        if let Some(result) = self
            .run_argument_order_checker(contexts, &mut all_concerns, totals)
            .await?
        {
            return Ok((all_concerns, Some(result)));
        }

        Ok((all_concerns, None))
    }

    fn no_concerns_output() -> Value {
        serde_json::json!({
            "findings": [],
            "review_inline": "No issues found.",
            "fixes": "",
            "concerns_count": 0,
            "stage8_input_concerns_count": 0,
            "stage8_output_concerns_count": 0,
            "stage8_dropped_concerns": [],
            "stage9_input_concerns_count": 0,
            "stage9_findings_count": 0,
            "stage9_dropped_candidates": []
        })
    }

    fn no_stage8_concerns_output(all_concerns_count: usize, stage8: Stage8Result) -> Value {
        serde_json::json!({
            "findings": [],
            "review_inline": "No issues found.",
            "fixes": "",
            "concerns_count": all_concerns_count,
            "stage8_input_concerns_count": stage8.input_concerns_count,
            "stage8_output_concerns_count": 0,
            "stage8_dropped_concerns": stage8.dropped_concerns,
            "stage9_input_concerns_count": 0,
            "stage9_findings_count": 0,
            "stage9_dropped_candidates": []
        })
    }

    fn no_stage9_findings_output(
        all_concerns_count: usize,
        stage8: Stage8Result,
        stage9: Stage9Result,
    ) -> Value {
        serde_json::json!({
            "findings": stage9.findings,
            "review_inline": "No issues found.",
            "fixes": "",
            "concerns_count": all_concerns_count,
            "stage8_input_concerns_count": stage8.input_concerns_count,
            "stage8_output_concerns_count": stage8.output_concerns_count,
            "stage8_dropped_concerns": stage8.dropped_concerns,
            "stage9_input_concerns_count": stage9.input_concerns_count,
            "stage9_findings_count": 0,
            "stage9_dropped_candidates": stage9.dropped_candidates
        })
    }

    fn final_review_output(
        all_concerns_count: usize,
        stage8: Stage8Result,
        stage9: Stage9Result,
        review_inline_text: String,
    ) -> Value {
        let stage9_findings_count = stage9.findings.as_array().map(|f| f.len()).unwrap_or(0);
        json!({
            "findings": stage9.findings,
            "review_inline": review_inline_text,
            "fixes": "",
            "concerns_count": all_concerns_count,
            "stage8_input_concerns_count": stage8.input_concerns_count,
            "stage8_output_concerns_count": stage8.output_concerns_count,
            "stage8_dropped_concerns": stage8.dropped_concerns,
            "stage9_input_concerns_count": stage9.input_concerns_count,
            "stage9_findings_count": stage9_findings_count,
            "stage9_dropped_candidates": stage9.dropped_candidates
        })
    }

    fn value_array_is_empty(value: &Value) -> bool {
        value.as_array().is_some_and(|array| array.is_empty())
    }

    fn fork_for_standard_stage(&self) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            tools: Arc::clone(&self.tools),
            prompts: self.prompts.clone(),
            global_history: Vec::new(),
            max_input_tokens: self.max_input_tokens,
            max_interactions: self.max_interactions,
            temperature: self.temperature,
            series_range: self.series_range.clone(),
            context_tag: self.context_tag.clone(),
            stages: self.stages.clone(),
            stage_protocol: self.stage_protocol,
            enable_static_bug_seeds: self.enable_static_bug_seeds,
            enable_targeted_bug_pattern_prescan: self.enable_targeted_bug_pattern_prescan,
        }
    }

    async fn run_standard_review_stages(
        &mut self,
        contexts: &ReviewContexts,
        planned_stages: Option<&[u8]>,
        all_concerns: &mut Vec<Value>,
        totals: &mut TokenTotals,
    ) -> Result<Option<WorkerResult>> {
        let format_guidance = r#"TodoWrite compatibility: vendored prompts may ask you to add tasks or suspected bugs to TodoWrite. Do not call or mention TodoWrite. Treat those instructions as an internal checklist only. If that checklist identifies a concrete suspected bug, carry it forward as a JSON concern with file, function_or_symbol, line when known, triggering condition, and evidence. Do not output generic checklist progress as a concern.

Once you have gathered sufficient information, return ONLY a JSON object with a "concerns" array.
If you find no concerns, return `{"concerns": []}`.
If you find concerns, each must be an object with:
- "type": A short category string.
- "description": A clear description of the problem.
- "reasoning": A step-by-step explanation.
- "preexisting": A boolean value: `true` if this bug/vulnerability already existed in the codebase before these patches were applied, or `false` if the issue was newly introduced by the reviewed patchset.
- "locations": An array of objects, each containing "file", "function_or_symbol", "line", "code_snippet", and "why_this_location_matters". Use `null` for "file", "function_or_symbol", "line", or "code_snippet" when an issue is non-local or the exact value is not known. Do not invent line numbers; use `line: null` when the exact line is not known and explain the triggering condition in "reasoning".

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
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }
      ]
    }
  ]
}
```"#;

        let mut stage_tasks = tokio::task::JoinSet::new();

        for stage in 1..=7 {
            if let Some(ref selected_stages) = self.stages {
                if !selected_stages.contains(&stage) {
                    continue;
                }
            } else if let Some(planned_stages) = planned_stages
                && !planned_stages.contains(&stage)
            {
                info!("Skipping stage {} based on planning phase", stage);
                continue;
            }

            info!("Running Stage {}", stage);
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = if (3..=6).contains(&stage) {
                contexts.shared_context_no_log.clone()
            } else {
                contexts.shared_context.clone()
            };
            let clean_system_prompt = if (3..=6).contains(&stage) {
                contexts.clean_shared_context_no_log.clone()
            } else {
                contexts.clean_shared_context.clone()
            };
            let user_prompt = format!("{}\n\n{}", stage_prompt, format_guidance);
            let clean_user_prompt = format!("{}\n\n{}", clean_stage_prompt, format_guidance);

            let mut stage_worker = self.fork_for_standard_stage();
            stage_tasks.spawn(async move {
                stage_worker
                    .run_standard_review_stage(
                        stage,
                        system_prompt,
                        clean_system_prompt,
                        user_prompt,
                        clean_user_prompt,
                    )
                    .await
            });
        }

        info!("Running {} planned stages concurrently", stage_tasks.len());
        let mut stage_results = Vec::new();
        while let Some(result) = stage_tasks.join_next().await {
            match result {
                Ok(Ok(stage_result)) => stage_results.push(stage_result),
                Ok(Err(e)) => {
                    stage_tasks.abort_all();
                    if let Some(ReviewError::BudgetExceeded(reason)) =
                        e.downcast_ref::<ReviewError>()
                        && self.stage_protocol == StageProtocol::BoundedLocalModel
                    {
                        warn!(
                            "Standard review stage exceeded budget; switching to minimal fallback review: {}",
                            reason
                        );
                        let result = self
                            .run_minimal_fallback_review(
                                &contexts.dynamic_context_no_log,
                                &contexts.clean_dynamic_context_no_log,
                                &contexts.base_dynamic_context_no_log,
                                &contexts.base_clean_dynamic_context_no_log,
                                all_concerns,
                                totals.input,
                                totals.output,
                                totals.cached,
                            )
                            .await?;
                        return Ok(Some(result));
                    }
                    return Err(e);
                }
                Err(e) => {
                    stage_tasks.abort_all();
                    return Err(anyhow::anyhow!("standard review stage task failed: {}", e));
                }
            }
        }

        stage_results.sort_by_key(|result| result.stage);
        for result in stage_results {
            totals.add_usage(result.tokens_in, result.tokens_out, result.tokens_cached);
            all_concerns.extend(result.concerns);
            self.global_history.extend(result.history);
        }

        Ok(None)
    }

    async fn run_standard_review_stage(
        &mut self,
        stage: u8,
        system_prompt: String,
        clean_system_prompt: String,
        user_prompt: String,
        clean_user_prompt: String,
    ) -> Result<StandardStageResult> {
        let mut outer_attempts = 0;
        let max_outer_attempts = 3;

        while outer_attempts < max_outer_attempts {
            outer_attempts += 1;
            let mut inner_attempts = 0;
            let max_inner_attempts = 3;
            let mut active_user_prompt = user_prompt.clone();
            let mut active_clean_user_prompt = clean_user_prompt.clone();

            while inner_attempts < max_inner_attempts {
                inner_attempts += 1;
                match self
                    .run_ai_stage(
                        stage,
                        system_prompt.clone(),
                        clean_system_prompt.clone(),
                        active_user_prompt.clone(),
                        active_clean_user_prompt.clone(),
                    )
                    .await
                {
                    Ok((result_json, t_in, t_out, t_cached)) => {
                        if let Some(concerns) =
                            result_json.get("concerns").and_then(|c| c.as_array())
                        {
                            let mut normalized_concerns = Vec::new();
                            for concern in concerns {
                                if concern.is_object() {
                                    normalized_concerns.push(concern.clone());
                                } else if let Some(description) = concern.as_str() {
                                    normalized_concerns.push(serde_json::json!({
                                        "type": "General",
                                        "description": description
                                    }));
                                }
                            }
                            return Ok(StandardStageResult {
                                stage,
                                concerns: normalized_concerns,
                                tokens_in: t_in,
                                tokens_out: t_out,
                                tokens_cached: t_cached,
                                history: self.global_history.clone(),
                            });
                        }

                        let violation = "JSON output is missing the required 'concerns' array";
                        tracing::warn!(
                            "Stage {} format validation failed (inner attempt {}/{}): {}. Retrying with augmented prompt.",
                            stage,
                            inner_attempts,
                            max_inner_attempts,
                            violation
                        );
                        let reminder = format!(
                            "\n\nPrevious attempt was rejected: {violation}. You MUST return ONLY a JSON object containing a 'concerns' array. If there are no concerns, return `{{\"concerns\": []}}`."
                        );
                        active_user_prompt = format!("{}{}", user_prompt, reminder);
                        active_clean_user_prompt = format!("{}{}", clean_user_prompt, reminder);
                    }
                    Err(e) => {
                        if e.downcast_ref::<ReviewError>().is_some() {
                            warn!("Stage {} hit non-retryable error: {}", stage, e);
                            return Err(e);
                        }
                        warn!(
                            "Stage {} AI execution failed (inner attempt {}/{}): {}",
                            stage, inner_attempts, max_inner_attempts, e
                        );
                    }
                }
            }

            warn!(
                "Stage {} outer attempt {}/{} failed to produce valid output.",
                stage, outer_attempts, max_outer_attempts
            );
        }

        warn!(
            "Stage {} failed after {} outer attempts.",
            stage, max_outer_attempts
        );
        Err(anyhow::anyhow!(
            "Stage {} failed to produce valid 'concerns' array after {} attempts — aborting review",
            stage,
            max_outer_attempts
        ))
    }

    async fn run_argument_order_checker(
        &mut self,
        contexts: &ReviewContexts,
        all_concerns: &mut Vec<Value>,
        totals: &mut TokenTotals,
    ) -> Result<Option<WorkerResult>> {
        info!("Running API argument-order checker");
        let argument_order_prompt = r#"Run a narrow API argument-order review for changed function calls only.

Checklist:
1. Inspect each changed call expression in the diff.
2. For each changed call, find the callee declaration or definition.
3. Compare parameter names and semantic roles against the variable/expression names at the call site.
4. Pay special attention to adjacent same-typed or pointer-typed arguments.
5. Flag the call if argument names or semantics appear swapped, even if the types still compile.

Rules:
- Use at most the callee signature/definition and directly touched call site context.
- Do not perform broad caller/callee traversal.
- Return ONLY a JSON object with a "concerns" array.
- If no argument-order/API-contract issue is supported, return {"concerns":[]}.
- Each concern must use "type", "description", "reasoning", "preexisting", and "locations"."#;

        match self
            .run_ai_stage(
                ARGUMENT_ORDER_STAGE,
                contexts.base_dynamic_context_no_log.clone(),
                contexts.base_clean_dynamic_context_no_log.clone(),
                argument_order_prompt.to_string(),
                argument_order_prompt.to_string(),
            )
            .await
        {
            Ok((result_json, t_in, t_out, t_cached)) => {
                totals.add_usage(t_in, t_out, t_cached);
                if let Some(concerns) = result_json.get("concerns").and_then(|c| c.as_array()) {
                    for c in concerns {
                        if let Value::Object(obj) = c {
                            let mut obj = obj.clone();
                            obj.insert(
                                "origin_stage".to_string(),
                                Value::String("argument_order_checker".to_string()),
                            );
                            obj.insert(
                                "preservation_policy".to_string(),
                                Value::String(
                                    "argument_order_emit_or_subsumed_by_detailed_finding"
                                        .to_string(),
                                ),
                            );
                            all_concerns.push(Value::Object(obj));
                        } else if let Some(s) = c.as_str() {
                            all_concerns.push(serde_json::json!({
                                "type": "API Argument Order",
                                "description": s,
                                "origin_stage": "argument_order_checker",
                                "preservation_policy": "argument_order_emit_or_subsumed_by_detailed_finding"
                            }));
                        }
                    }
                } else {
                    warn!("API argument-order checker did not return a concerns array");
                }
            }
            Err(e) => {
                if let Some(ReviewError::BudgetExceeded(reason)) = e.downcast_ref::<ReviewError>()
                    && self.stage_protocol == StageProtocol::BoundedLocalModel
                {
                    warn!(
                        "API argument-order checker exceeded budget; switching to minimal fallback review: {}",
                        reason
                    );
                    let result = self
                        .run_minimal_fallback_review(
                            &contexts.dynamic_context_no_log,
                            &contexts.clean_dynamic_context_no_log,
                            &contexts.base_dynamic_context_no_log,
                            &contexts.base_clean_dynamic_context_no_log,
                            all_concerns,
                            totals.input,
                            totals.output,
                            totals.cached,
                        )
                        .await?;
                    return Ok(Some(result));
                }
                warn!("API argument-order checker failed: {}", e);
            }
        }

        Ok(None)
    }

    async fn run_stage8_deduplication(
        &mut self,
        contexts: &ReviewContexts,
        all_concerns: &[Value],
        totals: &mut TokenTotals,
    ) -> Result<Stage8Result> {
        info!("Running Stage 8 (Deduplication)");
        let stage = 8;
        let (stage_prompt, _) = self.prompts.get_stage_prompt(stage).await?;
        let aggregated_concerns_json =
            serde_json::to_string_pretty(all_concerns).unwrap_or_default();
        let bounded_local_output_guidance = if self.stage_protocol
            == StageProtocol::BoundedLocalModel
        {
            "\n\nBounded local-model output discipline: keep retained concerns compact. Use one-sentence descriptions and at most two concise reasoning sentences. Do not copy full original reasoning, code blocks, or long snippets. Retain at most 12 unique concerns unless more are required to preserve distinct proof-required seed, argument-order, lifecycle-ordering, or retry resource-leak mechanisms."
        } else {
            ""
        };

        let user_prompt = format!(
            "{}\n\n{}\n\n{}\n\n{}\n\n{}{}\n\nAggregated Concerns:\n{}\n\nReturn ONLY a JSON object with top-level \"concerns\" and \"dropped_concerns\" arrays. Each retained concern must include \"type\", \"description\", \"reasoning\", \"preexisting\", and \"locations\" when location evidence is known. Preserve the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters data from merged concerns. Do not invent line numbers; use null when exact values are unknown. Preserve seed metadata fields such as \"source\", \"pattern\", \"preservation\", \"preservation_policy\", and \"required_evidence\" when merging static or targeted seed concerns into a retained concern. For every input concern discarded or merged away, add one dropped_concerns object with \"description\" and \"reason\". If a concern was merged into a retained concern, name the retained concern that absorbed it. If an argument-order/API-contract concern cannot preserve callee name, expected parameter order/signature, actual call-site argument order, and why the order is wrong inside the retained concern, keep it separate. If a lifecycle-ordering concern cannot preserve destroyed resource, unregister/callback source, re-entry path, bad ordering, and concrete consequence inside the retained concern, keep it separate. If a retry/error-path resource concern cannot preserve operation/helper, resource buffer, cleanup helper, retry/fallback trigger, and leak-before-retry mechanism inside the retained concern, keep it separate. If a proof-required static/targeted seed concern cannot preserve the relevant cgroup value-pointer, RCU traversal/locking, MAX_SKB_FRAGS/nr_frags capacity, or retry response-buffer/free-before-retry details inside the retained concern, keep it separate. No markdown. No prose.",
            stage_prompt,
            ARGUMENT_ORDER_PRESERVATION_RULE,
            LIFECYCLE_ORDERING_PRESERVATION_RULE,
            RETRY_RESOURCE_PRESERVATION_RULE,
            BUG_PATTERN_PRESERVATION_RULE,
            bounded_local_output_guidance,
            aggregated_concerns_json
        );

        let request = AiRequest {
            system: Some(contexts.shared_context.clone()),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some(user_prompt),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: Some(self.temperature),
            response_format: Some(AiResponseFormat::Json {
                schema: stage_response_schema(stage),
            }),
            context_tag: self
                .context_tag
                .as_ref()
                .map(|prefix| format!("{} s:8] ", &prefix[..prefix.len() - 2])),
        };

        let mut tokens = (0, 0, 0);
        let result_json = self
            .json_request("stage 8 deduplication", request, &mut tokens, |v| {
                if !v.get("concerns").is_some_and(|c| c.is_array()) {
                    return Err("missing concerns array".to_string());
                }
                if !v.get("dropped_concerns").is_some_and(|d| d.is_array()) {
                    return Err("missing dropped_concerns array".to_string());
                }
                Ok(())
            })
            .await;
        totals.add_usage(tokens.0, tokens.1, tokens.2);

        let (mut deduplicated_concerns, mut output_concerns_count, dropped_concerns) = if let Some(
            result_json,
        ) =
            result_json
        {
            let concerns = result_json
                .get("concerns")
                .filter(|c| c.is_array())
                .cloned()
                .unwrap_or_else(|| Value::Array(all_concerns.to_vec()));
            let output_count = concerns.as_array().map(|arr| arr.len()).unwrap_or(0);
            let drops = result_json
                .get("dropped_concerns")
                .filter(|d| d.is_array())
                .cloned()
                .unwrap_or_else(|| derive_stage8_drops(all_concerns, &concerns));
            (concerns, output_count, drops)
        } else {
            warn!(
                "Stage 8 did not produce valid schema output; preserving all {} concern(s) without deduplication",
                all_concerns.len()
            );
            (
                Value::Array(all_concerns.to_vec()),
                all_concerns.len(),
                Value::Array(Vec::new()),
            )
        };

        let restored_argument_order = preserve_stage8_argument_order_concerns(
            all_concerns,
            &mut deduplicated_concerns,
            &dropped_concerns,
        );
        if restored_argument_order > 0 {
            info!(
                "Stage 8 restored {} argument-order concern(s) that were merged without preserving order details",
                restored_argument_order
            );
            output_concerns_count = deduplicated_concerns
                .as_array()
                .map(|arr| arr.len())
                .unwrap_or(output_concerns_count);
        }

        let restored_seeded_patterns =
            preserve_stage8_proof_required_seed_concerns(all_concerns, &mut deduplicated_concerns);
        if restored_seeded_patterns > 0 {
            info!(
                "Stage 8 restored {} proof-required seeded concern(s) that were merged without preserving pattern details",
                restored_seeded_patterns
            );
            output_concerns_count = deduplicated_concerns
                .as_array()
                .map(|arr| arr.len())
                .unwrap_or(output_concerns_count);
        }

        Ok(Stage8Result {
            deduplicated_concerns,
            input_concerns_count: all_concerns.len(),
            output_concerns_count,
            dropped_concerns,
        })
    }

    async fn run_stage9_verification(
        &mut self,
        contexts: &ReviewContexts,
        stage8: &Stage8Result,
        all_concerns: &[Value],
        totals: &mut TokenTotals,
    ) -> Result<Stage9Run> {
        info!("Running Stage 9 (Verification)");
        let stage9_concerns = add_stage9_source_ids(&stage8.deduplicated_concerns);
        let stage9_input_concerns_count = stage9_concerns.as_array().map(|c| c.len()).unwrap_or(0);
        let stage = 9;
        let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
        let full_series_context = self.full_series_context();
        let deduplicated_concerns_json =
            serde_json::to_string_pretty(&stage9_concerns).unwrap_or_default();
        let user_prompt = format!(
            "{stage_prompt}\n\nCRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern. Argument-order concerns must not be dropped as false_positive; emit them as findings or mark them subsumed_by a detailed finding. Findings must describe bugs/regressions only; do not emit a finding that says the patch is correct, safe, redundant, or merely improves performance.\n\n{ARGUMENT_ORDER_PRESERVATION_RULE}\n\n{SEQCOUNT_IRQ_PRESERVATION_RULE}\n\n{RESOURCE_CLEANUP_PRESERVATION_RULE}\n\n{RETRY_RESOURCE_PRESERVATION_RULE}\n\n{LIFECYCLE_ORDERING_PRESERVATION_RULE}\n\n{BUG_PATTERN_PRESERVATION_RULE}\n\n{ROOT_CAUSE_COMPACTION_RULE}\n\nEvery consolidated concern has a stable source_concern_id. You MUST account for every source_concern_id exactly once: either convert it into one finding with the same source_concern_id, or place it in dropped_candidates with decision=\"drop\" and a concrete rationale. Prefer one finding per root cause; if a concern is covered by an emitted finding, drop it with drop_reason=\"subsumed_by\" and subsumed_by_finding_id set to that finding's finding_id.\n\nAllowed drop_reason values: duplicate, subsumed_by, insufficient_evidence, not_security_relevant, already_mitigated, false_positive, unclear.\n\nFull Series Context:\n{full_series_context}\n\nConsolidated Concerns:\n{deduplicated_concerns_json}\n\nReturn ONLY a JSON object with top-level 'findings' and 'dropped_candidates' arrays. Each finding MUST include these keys: \"finding_id\", \"source_concern_id\", \"problem\", \"severity\", \"severity_explanation\", \"preexisting\". Include a \"locations\" array when location evidence is known, carrying forward the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters details from the source concern. Do not invent line numbers; use null when exact values are unknown. Each dropped candidate MUST use these keys: \"source_concern_id\", \"decision\", \"drop_reason\", \"rationale\". If drop_reason is \"subsumed_by\", it MUST also include \"subsumed_by_finding_id\".\n\nThe count rule is mandatory: len(findings) + len(dropped_candidates) == len(Consolidated Concerns).\n\nExample Output:\n```json\n{{\n  \"findings\": [\n    {{\n      \"finding_id\": \"finding-1\",\n      \"source_concern_id\": \"stage8-001\",\n      \"problem\": \"Memory leak in function X when condition Y is met.\",\n      \"severity\": \"High\",\n      \"severity_explanation\": \"1. Condition Y is met.\\\n2. The buffer is allocated but not freed before return.\",\n      \"preexisting\": false,\n      \"locations\": [{{\"file\": \"path/to/file.c\", \"function_or_symbol\": \"function_name\", \"line\": 123, \"code_snippet\": \"problematic_code();\", \"why_this_location_matters\": \"This is where the newly allocated resource is dropped on the error path.\"}}]\n    }}\n  ],\n  \"dropped_candidates\": [\n    {{\n      \"source_concern_id\": \"stage8-002\",\n      \"decision\": \"drop\",\n      \"drop_reason\": \"subsumed_by\",\n      \"subsumed_by_finding_id\": \"finding-1\",\n      \"rationale\": \"This concern is the same root cause as finding-1.\"\n    }}\n  ]\n}}\n```"
        );
        let clean_user_prompt = format!(
            "{clean_stage_prompt}\n\nCRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid. If you cannot find concrete proof of safety, you must retain the concern. Argument-order concerns must not be dropped as false_positive; emit them as findings or mark them subsumed_by a detailed finding. Findings must describe bugs/regressions only; do not emit a finding that says the patch is correct, safe, redundant, or merely improves performance.\n\n{ARGUMENT_ORDER_PRESERVATION_RULE}\n\n{SEQCOUNT_IRQ_PRESERVATION_RULE}\n\n{RESOURCE_CLEANUP_PRESERVATION_RULE}\n\n{RETRY_RESOURCE_PRESERVATION_RULE}\n\n{LIFECYCLE_ORDERING_PRESERVATION_RULE}\n\n{BUG_PATTERN_PRESERVATION_RULE}\n\n{ROOT_CAUSE_COMPACTION_RULE}\n\nEvery consolidated concern has a stable source_concern_id. You MUST account for every source_concern_id exactly once: either convert it into one finding with the same source_concern_id, or place it in dropped_candidates with decision=\"drop\" and a concrete rationale. Prefer one finding per root cause; if a concern is covered by an emitted finding, drop it with drop_reason=\"subsumed_by\" and subsumed_by_finding_id set to that finding's finding_id.\n\nAllowed drop_reason values: duplicate, subsumed_by, insufficient_evidence, not_security_relevant, already_mitigated, false_positive, unclear.\n\nFull Series Context:\n{{series context}}\n\nConsolidated Concerns:\n{deduplicated_concerns_json}\n\nReturn ONLY a JSON object with top-level 'findings' and 'dropped_candidates' arrays. Each finding MUST include \"finding_id\", \"source_concern_id\", \"problem\", \"severity\", \"severity_explanation\", and \"preexisting\"; include \"locations\" when known and preserve precise location evidence from the source concern. Each dropped candidate MUST use \"source_concern_id\", \"decision\", \"drop_reason\", and \"rationale\"; include \"subsumed_by_finding_id\" when drop_reason is \"subsumed_by\".\n\nThe count rule is mandatory: len(findings) + len(dropped_candidates) == len(Consolidated Concerns)."
        );

        let mut findings_json;
        let mut stage9_dropped_candidates;
        match self
            .run_ai_stage(
                stage,
                contexts.shared_context.clone(),
                contexts.clean_shared_context.clone(),
                user_prompt,
                clean_user_prompt,
            )
            .await
        {
            Ok((result_json, t_in, t_out, t_cached)) => {
                totals.add_usage(t_in, t_out, t_cached);
                if let Some(f) = result_json.get("findings").filter(|f| f.is_array()) {
                    findings_json = f.clone();
                    ensure_stage9_finding_ids(&mut findings_json);
                } else {
                    warn!(
                        "Stage 9 did not produce a valid findings array; using accountability repair instead of failing review"
                    );
                    findings_json = json!([]);
                }
                stage9_dropped_candidates = result_json
                    .get("dropped_candidates")
                    .filter(|d| d.is_array())
                    .cloned()
                    .unwrap_or_else(|| json!([]));
            }
            Err(e) => {
                if let Some(ReviewError::BudgetExceeded(reason)) = e.downcast_ref::<ReviewError>()
                    && self.stage_protocol == StageProtocol::BoundedLocalModel
                {
                    warn!(
                        "Stage 9 exceeded budget; switching to minimal fallback review: {}",
                        reason
                    );
                    let result = self
                        .run_minimal_fallback_review(
                            &contexts.dynamic_context_no_log,
                            &contexts.clean_dynamic_context_no_log,
                            &contexts.base_dynamic_context_no_log,
                            &contexts.base_clean_dynamic_context_no_log,
                            all_concerns,
                            totals.input,
                            totals.output,
                            totals.cached,
                        )
                        .await?;
                    return Ok(Stage9Run::Fallback(result));
                }
                return Err(anyhow::anyhow!("Stage 9 AI execution failed: {}", e));
            }
        }

        self.repair_stage9_accounting_if_needed(
            contexts,
            &stage9_concerns,
            stage9_input_concerns_count,
            &mut findings_json,
            &mut stage9_dropped_candidates,
            totals,
        )
        .await?;

        let (compacted_findings, compacted_drops) = compact_stage9_related_findings(
            &stage9_concerns,
            &findings_json,
            &stage9_dropped_candidates,
        );
        if compacted_findings != findings_json || compacted_drops != stage9_dropped_candidates {
            if let Err(e) =
                validate_stage9_accounting(&stage9_concerns, &compacted_findings, &compacted_drops)
            {
                warn!(
                    "Compacted Stage 9 accounting was invalid; keeping uncompacted repaired ledger: {}",
                    e
                );
            } else {
                findings_json = compacted_findings;
                stage9_dropped_candidates = compacted_drops;
            }
        }

        Ok(Stage9Run::Completed(Stage9Result {
            findings: findings_json,
            input_concerns_count: stage9_input_concerns_count,
            dropped_candidates: stage9_dropped_candidates,
        }))
    }

    async fn repair_stage9_accounting_if_needed(
        &mut self,
        contexts: &ReviewContexts,
        stage9_concerns: &Value,
        stage9_input_concerns_count: usize,
        findings_json: &mut Value,
        stage9_dropped_candidates: &mut Value,
        totals: &mut TokenTotals,
    ) -> Result<()> {
        let mut accounting_violation =
            validate_stage9_accounting(stage9_concerns, findings_json, stage9_dropped_candidates)
                .err();
        if stage9_input_concerns_count > 0
            && findings_json
                .as_array()
                .is_some_and(|findings| findings.is_empty())
        {
            accounting_violation = Some(format!(
                "Stage 9 emitted zero findings despite {stage9_input_concerns_count} retained Stage 8 concern(s)"
            ));
        }

        let Some(reason) = accounting_violation else {
            return Ok(());
        };

        warn!(
            "Stage 9 accountability check requires a second finalization pass: {}",
            reason
        );
        let mut retry_error = match self
            .run_stage9_accountability_pass(
                &reason,
                &contexts.base_dynamic_context_no_log,
                &contexts.base_clean_dynamic_context_no_log,
                stage9_concerns,
            )
            .await
        {
            Ok((retry_json, t_in, t_out, t_cached)) => {
                totals.add_usage(t_in, t_out, t_cached);
                let mut retry_error = None;
                if let Some(retry_findings) = retry_json.get("findings").filter(|f| f.is_array()) {
                    *findings_json = retry_findings.clone();
                    ensure_stage9_finding_ids(findings_json);
                } else {
                    retry_error = Some(
                        "Stage 9 accountability pass did not return findings array".to_string(),
                    );
                }

                if let Some(retry_drops) = retry_json
                    .get("dropped_candidates")
                    .filter(|d| d.is_array())
                {
                    *stage9_dropped_candidates = retry_drops.clone();
                } else {
                    retry_error.get_or_insert_with(|| {
                        "Stage 9 accountability pass did not return dropped_candidates array"
                            .to_string()
                    });
                }

                retry_error
            }
            Err(e) => Some(format!("Stage 9 accountability pass failed: {e}")),
        };

        if retry_error.is_none() {
            retry_error = validate_stage9_accounting(
                stage9_concerns,
                findings_json,
                stage9_dropped_candidates,
            )
            .err();
        }

        if let Some(e) = retry_error {
            warn!(
                "Stage 9 accountability pass was invalid; repairing ledger instead of failing review: {}",
                e
            );
            let (repaired_findings, repaired_drops) =
                repair_stage9_accounting(stage9_concerns, findings_json, stage9_dropped_candidates);
            let (repaired_findings, repaired_drops) = cap_repaired_stage9_findings(
                stage9_concerns,
                repaired_findings,
                repaired_drops,
                MAX_REPAIRED_STAGE9_FINDINGS,
            );
            *findings_json = repaired_findings;
            *stage9_dropped_candidates = repaired_drops;
        }

        let finding_count = findings_json.as_array().map(|f| f.len()).unwrap_or(0);
        if finding_count > MAX_REPAIRED_STAGE9_FINDINGS {
            warn!(
                "Stage 9 accountability pass emitted {} findings; compacting to at most {}",
                finding_count, MAX_REPAIRED_STAGE9_FINDINGS
            );
            let (capped_findings, capped_drops) = cap_repaired_stage9_findings(
                stage9_concerns,
                findings_json.clone(),
                stage9_dropped_candidates.clone(),
                MAX_REPAIRED_STAGE9_FINDINGS,
            );
            *findings_json = capped_findings;
            *stage9_dropped_candidates = capped_drops;
        }

        if let Err(e) =
            validate_stage9_accounting(stage9_concerns, findings_json, stage9_dropped_candidates)
        {
            warn!(
                "Stage 9 accounting repair still failed; attempting final deterministic repair: {}",
                e
            );
            let (repaired_findings, repaired_drops) =
                repair_stage9_accounting(stage9_concerns, findings_json, stage9_dropped_candidates);
            let (repaired_findings, repaired_drops) = cap_repaired_stage9_findings(
                stage9_concerns,
                repaired_findings,
                repaired_drops,
                MAX_REPAIRED_STAGE9_FINDINGS,
            );
            *findings_json = repaired_findings;
            *stage9_dropped_candidates = repaired_drops;
            if let Err(e) = validate_stage9_accounting(
                stage9_concerns,
                findings_json,
                stage9_dropped_candidates,
            ) {
                return Err(ReviewError::FormatRejection(format!(
                    "Stage 9 accounting repair failed after deterministic repair: {e}"
                ))
                .into());
            }
        }

        Ok(())
    }

    fn full_series_context(&self) -> String {
        let Some(range) = &self.series_range else {
            return "Not applicable (single patch or last patch in series).".to_string();
        };

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
    }

    async fn run_stage10_report(
        &mut self,
        contexts: &ReviewContexts,
        findings_json: &Value,
        totals: &mut TokenTotals,
    ) -> Result<String> {
        info!("Running Stage 10");
        let stage = 10;
        let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
        let findings_str = serde_json::to_string_pretty(findings_json).unwrap_or_default();
        let user_prompt = format!(
            "{}\n\nFindings:\n{}\n\nReturn raw text output, not JSON.",
            stage_prompt, findings_str
        );
        let clean_user_prompt = format!(
            "{}\n\nFindings:\n{}\n\nReturn raw text output, not JSON.",
            clean_stage_prompt, findings_str
        );
        let max_retries = 3;
        let mut retries = 0;
        let mut active_user_prompt = user_prompt.clone();
        let mut active_clean_user_prompt = clean_user_prompt.clone();
        let mut free_form_mode = false;
        let mut review_inline_text = String::new();

        while retries < max_retries {
            match self
                .run_ai_stage_raw(
                    stage,
                    contexts.shared_context.clone(),
                    contexts.clean_shared_context.clone(),
                    active_user_prompt.clone(),
                    active_clean_user_prompt.clone(),
                )
                .await
            {
                Ok((result_text, t_in, t_out, t_cached)) => {
                    totals.add_usage(t_in, t_out, t_cached);
                    if free_form_mode {
                        review_inline_text = result_text;
                        break;
                    }
                    match validate_inline_format(&result_text) {
                        Ok(_) => {
                            review_inline_text = result_text;
                            break;
                        }
                        Err(violation) => {
                            tracing::warn!(
                                "Stage 10 format validation failed (attempt {}/{}): {}. Retrying with augmented prompt.",
                                retries + 1,
                                max_retries,
                                violation
                            );
                            let reminder = format!(
                                "\n\nPrevious attempt was rejected: {violation}. Strictly follow the formatting rules."
                            );
                            active_user_prompt = format!("{}{}", user_prompt, reminder);
                            active_clean_user_prompt = format!("{}{}", clean_user_prompt, reminder);
                        }
                    }
                }
                Err(e) => {
                    let err_str = e.to_string();
                    tracing::warn!(
                        "Stage 10 failed (attempt {}/{}): {}",
                        retries + 1,
                        max_retries,
                        err_str
                    );
                    if err_str.contains("RECITATION") && !free_form_mode {
                        tracing::warn!(
                            "Recitation error detected. Falling back to free-form mode."
                        );
                        free_form_mode = true;
                        let fallback_reminder = "\n\nCRITICAL: The previous attempt failed due to a RECITATION policy violation. Do NOT quote the original patch code at all. Instead, provide a free-form summary of the findings. Start your report with a note explaining that the format is altered due to recitation restrictions. Do not use the inline quoting style `>`.";
                        active_user_prompt = format!("{}{}", user_prompt, fallback_reminder);
                        active_clean_user_prompt =
                            format!("{}{}", clean_user_prompt, fallback_reminder);
                        if retries + 1 == max_retries {
                            retries -= 1;
                        }
                    }
                }
            }
            retries += 1;
        }

        if review_inline_text.is_empty() {
            warn!(
                "Stage 10 failed to generate a valid LKML report after {} attempts; using synthesized fallback report",
                max_retries
            );
            review_inline_text = fallback_inline_review(findings_json);
        }

        Ok(review_inline_text)
    }

    pub async fn run(&mut self, patchset: Value) -> Result<WorkerResult> {
        let (target_commit_diff_only, contexts, planned_stages, mut totals) =
            self.prepare_run(&patchset).await?;

        let (all_concerns, fallback_result) = self
            .collect_concerns(
                &target_commit_diff_only,
                &contexts,
                planned_stages.as_deref(),
                &mut totals,
            )
            .await?;
        if let Some(result) = fallback_result {
            return Ok(result);
        }

        if all_concerns.is_empty() {
            tracing::info!(
                "No concerns from early checker or stages 1-7, skipping stages 8, 9 and 10"
            );
            return Ok(self.worker_result_with_output(Self::no_concerns_output(), totals));
        }

        let stage8 = self
            .run_stage8_deduplication(&contexts, &all_concerns, &mut totals)
            .await?;
        if Self::value_array_is_empty(&stage8.deduplicated_concerns) {
            tracing::info!(
                "No concerns remaining after Stage 8 deduplication, skipping stages 9 and 10"
            );
            let final_output = Self::no_stage8_concerns_output(all_concerns.len(), stage8);
            return Ok(self.worker_result_with_output(final_output, totals));
        }

        let stage9 = match self
            .run_stage9_verification(&contexts, &stage8, &all_concerns, &mut totals)
            .await?
        {
            Stage9Run::Completed(result) => result,
            Stage9Run::Fallback(result) => return Ok(result),
        };

        if Self::value_array_is_empty(&stage9.findings) {
            tracing::info!("No findings from Stage 9, skipping Stage 10");
            let final_output = Self::no_stage9_findings_output(all_concerns.len(), stage8, stage9);
            return Ok(self.worker_result_with_output(final_output, totals));
        }

        let review_inline_text = self
            .run_stage10_report(&contexts, &stage9.findings, &mut totals)
            .await?;
        let final_output =
            Self::final_review_output(all_concerns.len(), stage8, stage9, review_inline_text);

        Ok(self.worker_result_with_output(final_output, totals))
    }

    async fn run_stage9_accountability_pass(
        &mut self,
        reason: &str,
        patch_context: &str,
        clean_patch_context: &str,
        stage9_concerns: &Value,
    ) -> Result<(Value, u32, u32, u32)> {
        let concerns_json = serde_json::to_string_pretty(stage9_concerns).unwrap_or_default();
        let user_prompt = format!(
            "{patch_context}\n\nStage 9 accountability retry is required: {reason}\n\nYou are about to emit zero or unaccounted findings despite retained Stage 8 concerns. For each retained concern below, either convert it into a finding or give a concrete reason it is invalid. No tools. Schema only. Emit at most {MAX_REPAIRED_STAGE9_FINDINGS} findings; use dropped_candidates with duplicate, subsumed_by, insufficient_evidence, not_security_relevant, already_mitigated, false_positive, or unclear for the rest. Argument-order concerns must not be dropped as false_positive; emit them as findings or mark them subsumed_by a detailed finding. Findings must describe bugs/regressions only; do not emit a finding that says the patch is correct, safe, redundant, or merely improves performance.\n\n{ARGUMENT_ORDER_PRESERVATION_RULE}\n\n{SEQCOUNT_IRQ_PRESERVATION_RULE}\n\n{RESOURCE_CLEANUP_PRESERVATION_RULE}\n\n{RETRY_RESOURCE_PRESERVATION_RULE}\n\n{LIFECYCLE_ORDERING_PRESERVATION_RULE}\n\n{BUG_PATTERN_PRESERVATION_RULE}\n\n{ROOT_CAUSE_COMPACTION_RULE}\n\nRetained Stage 8 concerns:\n{concerns_json}\n\nReturn ONLY a JSON object with top-level 'findings' and 'dropped_candidates' arrays. Each finding MUST include finding_id, source_concern_id, problem, severity, severity_explanation, and preexisting. Each dropped candidate MUST include source_concern_id, decision=\"drop\", drop_reason, and rationale. If drop_reason is subsumed_by, include subsumed_by_finding_id. Allowed drop_reason values: duplicate, subsumed_by, insufficient_evidence, not_security_relevant, already_mitigated, false_positive, unclear. Account for every source_concern_id exactly once."
        );
        let clean_user_prompt = format!(
            "{clean_patch_context}\n\nStage 9 accountability retry is required: {reason}\n\nFor each retained Stage 8 concern below, either convert it into a finding or give a concrete reason it is invalid. No tools. Schema only. Emit at most {MAX_REPAIRED_STAGE9_FINDINGS} findings; account for the rest in dropped_candidates. Argument-order concerns must not be dropped as false_positive; emit them as findings or mark them subsumed_by a detailed finding. Findings must describe bugs/regressions only; do not emit a finding that says the patch is correct, safe, redundant, or merely improves performance.\n\n{ARGUMENT_ORDER_PRESERVATION_RULE}\n\n{SEQCOUNT_IRQ_PRESERVATION_RULE}\n\n{RESOURCE_CLEANUP_PRESERVATION_RULE}\n\n{RETRY_RESOURCE_PRESERVATION_RULE}\n\n{LIFECYCLE_ORDERING_PRESERVATION_RULE}\n\n{BUG_PATTERN_PRESERVATION_RULE}\n\n{ROOT_CAUSE_COMPACTION_RULE}\n\nRetained Stage 8 concerns:\n{concerns_json}\n\nReturn ONLY a JSON object with top-level 'findings' and 'dropped_candidates' arrays. Findings need finding_id values. dropped_candidates with drop_reason=subsumed_by need subsumed_by_finding_id. Account for every source_concern_id exactly once."
        );

        let clean_msg = AiMessage {
            role: AiRole::User,
            content: Some(clean_user_prompt),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };
        self.global_history.push(clean_msg);

        let request = AiRequest {
            system: Some(
                "You are a conservative Linux kernel review finalizer. Preserve plausible retained concerns unless concrete evidence invalidates them."
                    .to_string(),
            ),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some(user_prompt),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: Some(self.temperature),
            response_format: Some(AiResponseFormat::Json {
                schema: stage_response_schema(9),
            }),
            context_tag: self.context_tag.as_ref().map(|prefix| {
                format!("{} s:9:account] ", &prefix[..prefix.len() - 2])
            }),
        };

        let estimated = self.request_estimate(&request);
        if estimated > self.prompt_preflight_cap() {
            return Err(ReviewError::BudgetExceeded(format!(
                "stage 9 accountability prompt estimate {} exceeds preflight cap {} (max_input_tokens {})",
                estimated,
                self.prompt_preflight_cap(),
                self.max_input_tokens
            ))
            .into());
        }

        let response = self
            .checked_generate_content("stage 9 accountability finalization", request)
            .await?;
        let mut t_in = 0;
        let mut t_out = 0;
        let mut t_cached = 0;
        if let Some(usage) = &response.usage {
            self.validate_prompt_usage("stage 9 accountability finalization", usage.prompt_tokens)?;
            t_in += usage.prompt_tokens as u32;
            t_out += usage.completion_tokens as u32;
            t_cached += usage.cached_tokens.unwrap_or(0) as u32;
        }

        let raw = response.content.unwrap_or_default();
        let parsed = parse_stage_json(&raw);
        self.global_history.push(AiMessage {
            role: AiRole::Assistant,
            content: Some(raw),
            thought: response.thought,
            thought_signature: response.thought_signature,
            tool_calls: response.tool_calls,
            tool_call_id: None,
        });

        Ok((parsed, t_in, t_out, t_cached))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_minimal_fallback_review(
        &mut self,
        preferred_context: &str,
        clean_preferred_context: &str,
        diff_only_context: &str,
        clean_diff_only_context: &str,
        existing_concerns: &[Value],
        mut total_tokens_in: u32,
        mut total_tokens_out: u32,
        mut total_tokens_cached: u32,
    ) -> Result<WorkerResult> {
        let stage9_concerns = add_stage9_source_ids(&Value::Array(existing_concerns.to_vec()));
        let stage9_concerns_vec = stage9_concerns.as_array().cloned().unwrap_or_default();
        let existing_concerns_json =
            serde_json::to_string_pretty(&stage9_concerns).unwrap_or_default();
        let task = format!(
            r#"Minimal budget fallback review mode is active.

The normal staged review exceeded the prompt budget. Do a narrow diff-focused Linux kernel bug review using only the context below and any existing concerns already gathered.

Rules:
- Do not request tools.
- Focus on concrete regressions visible in the diff or changed-function context.
- Prefer local reasoning over broad subsystem archaeology.
- Report classic local bugs: swapped arguments, missing free/unwind on error path, missing bounds checks, RCU/list misuse, workqueue lifetime ordering, locking imbalance, NULL dereference, and use-after-free.
- If existing concerns are listed, validate them from the available context and keep any concern that cannot be proven false.
- Existing concerns from the targeted bug-pattern scan need concrete proof before dropping: prove the non-NULL value check for cgroup keyed writes, the RCU read/update-side lock for teardown traversal, the MAX_SKB_FRAGS/nr_frags guard before skb fragment append, or the exact free path that runs before retry/fallback for response buffers. For the cgroup keyed-write seed, a valid finding must explicitly preserve the limit.max/"max" key, the missing value or bare-key write, and the NULL dereference or invalid parse site; a generic non-"max" value parsing concern is not enough. For retry response-buffer seeds, a valid finding must preserve retry_open or equivalent, retry_iov.iov_base/response buffer, free_response_buf or equivalent, failed operation followed by retry/fallback, and missing free before retry/overwrite.
- Existing lifecycle-ordering concerns need concrete proof before dropping: prove the exact unregister/destroy order, callback path, and synchronization/barrier/cancel operation that prevents a callback such as nci_close_device from observing a destroyed workqueue or freed state.
- Return only JSON with a top-level "findings" array.
- If nothing concrete is supported, return {{"findings":[]}}.

Existing concerns from the aborted staged review:
{existing_concerns_json}

Each finding must use exactly these keys: "source_concern_id", "problem", "severity", "severity_explanation", "preexisting". Copy the source_concern_id from the existing concern this finding validates."#
        );

        let make_request = |context: &str, clean_context: &str| {
            let user_prompt = format!("{context}\n\n{task}");
            let clean_user_prompt = format!("{clean_context}\n\n{task}");
            let user_msg = AiMessage {
                role: AiRole::User,
                content: Some(user_prompt),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            };
            let clean_msg = AiMessage {
                role: AiRole::User,
                content: Some(clean_user_prompt),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            };
            let request = AiRequest {
                system: Some(
                    "You are a conservative Linux kernel reviewer running a minimal fallback pass."
                        .to_string(),
                ),
                messages: vec![user_msg],
                tools: None,
                temperature: Some(self.temperature),
                response_format: Some(AiResponseFormat::Json {
                    schema: minimal_fallback_response_schema(),
                }),
                context_tag: self
                    .context_tag
                    .as_ref()
                    .map(|prefix| format!("{} s:minimal] ", &prefix[..prefix.len() - 2])),
            };
            (request, clean_msg)
        };

        let (mut request, mut clean_msg) = make_request(preferred_context, clean_preferred_context);
        if self.request_estimate(&request) > self.prompt_preflight_cap() {
            warn!(
                "Minimal fallback preferred context is over budget; retrying with diff-only context"
            );
            (request, clean_msg) = make_request(diff_only_context, clean_diff_only_context);
        }

        let estimated = self.request_estimate(&request);
        if estimated > self.prompt_preflight_cap() {
            return Err(ReviewError::BudgetExceeded(format!(
                "minimal fallback prompt estimate {} exceeds preflight cap {} (max_input_tokens {})",
                estimated,
                self.prompt_preflight_cap(),
                self.max_input_tokens
            ))
            .into());
        }

        info!("Running minimal budget fallback review");
        self.global_history.push(clean_msg);
        let response = self
            .checked_generate_content("minimal fallback review", request)
            .await?;
        if let Some(usage) = &response.usage {
            self.validate_prompt_usage("minimal fallback review", usage.prompt_tokens)?;
            total_tokens_in += usage.prompt_tokens as u32;
            total_tokens_out += usage.completion_tokens as u32;
            total_tokens_cached += usage.cached_tokens.unwrap_or(0) as u32;
        }

        let raw = response.content.unwrap_or_default();
        let parsed = parse_stage_json(&raw);
        let mut findings = parsed
            .get("findings")
            .filter(|findings| findings.is_array())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Minimal fallback failed to produce findings array"))?;
        preserve_static_bug_pattern_findings(&stage9_concerns_vec, &mut findings);
        preserve_static_lifecycle_ordering_findings(&stage9_concerns_vec, &mut findings);
        assign_minimal_fallback_source_ids(&stage9_concerns_vec, &mut findings);
        cap_minimal_fallback_findings(
            &stage9_concerns,
            &mut findings,
            MINIMAL_FALLBACK_MAX_FINDINGS,
        );

        let finding_source_ids: HashSet<&str> = findings
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|finding| {
                finding
                    .get("source_concern_id")
                    .and_then(|value| value.as_str())
            })
            .collect();
        let mut stage9_dropped_candidates = Vec::new();
        for source_concern_id in stage9_concerns_vec
            .iter()
            .filter_map(|concern| concern.get("source_concern_id"))
            .filter_map(|value| value.as_str())
            .filter(|source_concern_id| !finding_source_ids.contains(source_concern_id))
        {
            stage9_dropped_candidates.push(json!({
                "source_concern_id": source_concern_id,
                "decision": "drop",
                "drop_reason": "insufficient_evidence",
                "rationale": "Minimal-budget fallback did not generate a finding for this retained Stage 9 concern."
            }));
        }

        let final_output = json!({
            "findings": findings,
            "review_inline": fallback_inline_review(&findings),
            "fixes": "",
            "concerns_count": stage9_concerns_vec.len(),
            "fallback_mode": "minimal_budget",
            "stage8_input_concerns_count": stage9_concerns_vec.len(),
            "stage8_output_concerns_count": stage9_concerns_vec.len(),
            "stage8_dropped_concerns": [],
            "stage9_input_concerns_count": stage9_concerns_vec.len(),
            "stage9_findings_count": findings.as_array().map(|f| f.len()).unwrap_or(0),
            "stage9_dropped_candidates": stage9_dropped_candidates
        });

        Ok(WorkerResult {
            output: Some(final_output),
            error: None,
            input_context: "Minimal budget fallback completed".to_string(),
            history: self.global_history.clone(),
            history_before_pruning: self.global_history.clone(),
            history_after_pruning: self.global_history.clone(),
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            tokens_cached: total_tokens_cached,
        })
    }

    async fn run_ai_stage(
        &mut self,
        stage: u8,
        system_prompt: String,
        clean_system_prompt: String,
        user_prompt: String,
        clean_user_prompt: String,
    ) -> Result<(Value, u32, u32, u32)> {
        let (raw_text, t_in, t_out, t_cached) = self
            .run_ai_stage_raw(
                stage,
                system_prompt,
                clean_system_prompt,
                user_prompt,
                clean_user_prompt,
            )
            .await?;
        let parsed = parse_stage_json(&raw_text);
        Ok((parsed, t_in, t_out, t_cached))
    }

    fn initialize_stage_history(
        &mut self,
        user_prompt: String,
        clean_system_prompt: String,
        clean_user_prompt: String,
    ) -> Vec<AiMessage> {
        let user_msg = AiMessage {
            role: AiRole::User,
            content: Some(user_prompt),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };

        if self.global_history.is_empty() {
            self.global_history.push(AiMessage {
                role: AiRole::System,
                content: Some(clean_system_prompt),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }
        self.global_history.push(AiMessage {
            role: AiRole::User,
            content: Some(clean_user_prompt),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        });

        vec![user_msg]
    }

    fn record_stage_usage(
        &self,
        stage: u8,
        label: &str,
        usage: &AiUsage,
        t_in: &mut u32,
        t_out: &mut u32,
        t_cached: &mut u32,
    ) -> Result<bool> {
        self.validate_prompt_usage(label, usage.prompt_tokens)?;
        *t_in += usage.prompt_tokens as u32;
        *t_out += usage.completion_tokens as u32;
        *t_cached += usage.cached_tokens.unwrap_or(0) as u32;

        if usage.prompt_tokens > self.local_context_threshold(LOCAL_SKIP_EXPLORATION_PERCENT) {
            warn!(
                "Stage {} actual prompt_tokens {} crossed the local {}% context guard; blocking further tool expansion",
                stage, usage.prompt_tokens, LOCAL_SKIP_EXPLORATION_PERCENT
            );
            return Ok(true);
        }
        Ok(false)
    }

    fn push_assistant_response(&mut self, resp: &AiResponse, local_history: &mut Vec<AiMessage>) {
        let assistant_msg = AiMessage {
            role: AiRole::Assistant,
            content: resp.content.clone(),
            thought: resp.thought.clone(),
            thought_signature: resp.thought_signature.clone(),
            tool_calls: resp.tool_calls.clone(),
            tool_call_id: None,
        };
        local_history.push(assistant_msg.clone());
        self.global_history.push(assistant_msg);
    }

    async fn handle_bounded_direct_response(
        &mut self,
        context: DirectResponseContext<'_>,
        resp: AiResponse,
        local_history: &mut Vec<AiMessage>,
        empty_no_tool_reprompt_used: &mut bool,
        token_counts: (&mut u32, &mut u32, &mut u32),
    ) -> Result<Option<(String, u32, u32, u32)>> {
        if stage_response_schema(context.stage).is_none() {
            return Ok(Some((
                resp.content.unwrap_or_default(),
                *token_counts.0,
                *token_counts.1,
                *token_counts.2,
            )));
        }

        let final_content = self
            .finalize_stage_output(
                context.stage,
                context.system_prompt,
                local_history,
                token_counts.0,
                token_counts.1,
                token_counts.2,
            )
            .await?;

        if (1..=7).contains(&context.stage)
            && context.progress_tool_calls_seen == 0
            && !*empty_no_tool_reprompt_used
            && stage_has_empty_concerns(&final_content.0)
        {
            *empty_no_tool_reprompt_used = true;
            let reprompt = "You returned an empty stage result without inspecting any available context.\nBefore emitting an empty concerns list, use the available tools to inspect the target diff and relevant surrounding code.";
            let reprompt_msg = AiMessage {
                role: AiRole::User,
                content: Some(reprompt.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            };
            local_history.push(reprompt_msg.clone());
            self.global_history.push(reprompt_msg);
            return Ok(None);
        }

        Ok(Some(final_content))
    }

    fn bounded_exploration_request(
        &self,
        stage: u8,
        system_prompt: &str,
        local_history: &[AiMessage],
        tool_declarations: &[AiTool],
    ) -> AiRequest {
        AiRequest {
            system: Some(system_prompt.to_string()),
            messages: local_history.to_vec(),
            tools: Some(tool_declarations.to_vec()),
            temperature: Some(self.temperature),
            response_format: None,
            context_tag: self
                .context_tag
                .as_ref()
                .map(|prefix| format!("{} s:{}] ", &prefix[..prefix.len() - 2], stage)),
        }
    }

    async fn execute_bounded_tool_calls(
        &mut self,
        turn: BoundedToolTurn<'_>,
        progress_tool_calls_seen: &mut usize,
        tool_cache: &mut HashMap<String, CachedToolCall>,
        tool_calls: Vec<ToolCall>,
        local_history: &mut Vec<AiMessage>,
    ) -> bool {
        let mut tool_responses = Vec::new();
        let mut force_finalize = turn.force_finalize_due_to_context;
        let remaining_stage_budget = turn
            .policy
            .max_tool_calls
            .saturating_sub(*progress_tool_calls_seen);
        let allowed_this_turn = if turn.force_finalize_due_to_context {
            0
        } else {
            per_turn_tool_call_cap(turn.stage).min(remaining_stage_budget)
        };

        for (idx, call) in tool_calls.into_iter().enumerate() {
            let tool_name = call.function_name.trim().to_ascii_lowercase();
            let result = if idx >= allowed_this_turn {
                force_finalize = true;
                json!({
                    "error": "Tool call blocked: per-turn or context tool budget exceeded. Finalize using the code/context evidence gathered so far."
                })
                .to_string()
            } else if (1..=9).contains(&turn.stage)
                && matches!(tool_name.as_str(), "todowrite" | "todoread")
            {
                force_finalize = true;
                json!({
                    "error": "Todo planning tools are disabled during review-stage exploration. Finalize using the code/context evidence gathered so far."
                })
                .to_string()
            } else if (1..=9).contains(&turn.stage) && tool_name == "git_blame" {
                force_finalize = true;
                json!({
                    "error": "git_blame is disabled during bounded local review because blame output is too large for the context budget. Use the diff and directly relevant source context already gathered."
                })
                .to_string()
            } else {
                *progress_tool_calls_seen += 1;
                let key = tool_cache_key(&call.function_name, &call.arguments);
                if let Some(cached) = tool_cache.get_mut(&key) {
                    cached.requests += 1;
                    if cached.requests == 2 {
                        json!({
                            "cached": true,
                            "content": cached.result
                        })
                        .to_string()
                    } else {
                        force_finalize = true;
                        json!({
                            "error": "Repeated identical tool call blocked. Finalize the stage using the evidence already gathered."
                        })
                        .to_string()
                    }
                } else {
                    let result = match self
                        .tools
                        .call(&call.function_name, call.arguments.clone())
                        .await
                    {
                        Ok(v) => v.to_string(),
                        Err(e) => json!({"error": e.to_string()}).to_string(),
                    };
                    tool_cache.insert(
                        key,
                        CachedToolCall {
                            requests: 1,
                            result: result.clone(),
                        },
                    );
                    result
                }
            };
            tool_responses.push(AiMessage {
                role: AiRole::Tool,
                content: Some(result),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: Some(call.id.clone()),
            });
        }

        local_history.extend(tool_responses.clone());
        self.global_history.extend(tool_responses);
        force_finalize || *progress_tool_calls_seen >= turn.policy.max_tool_calls
    }

    async fn run_ai_stage_raw(
        &mut self,
        stage: u8,
        system_prompt: String,
        clean_system_prompt: String,
        user_prompt: String,
        clean_user_prompt: String,
    ) -> Result<(String, u32, u32, u32)> {
        let local_history =
            self.initialize_stage_history(user_prompt, clean_system_prompt, clean_user_prompt);

        let t_in = 0;
        let t_out = 0;
        let t_cached = 0;
        if self.stage_protocol == StageProtocol::Native {
            return self
                .run_ai_stage_raw_native(stage, system_prompt, local_history, t_in, t_out, t_cached)
                .await;
        }

        self.run_ai_stage_raw_bounded(stage, system_prompt, local_history, t_in, t_out, t_cached)
            .await
    }

    async fn run_ai_stage_raw_bounded(
        &mut self,
        stage: u8,
        system_prompt: String,
        mut local_history: Vec<AiMessage>,
        mut t_in: u32,
        mut t_out: u32,
        mut t_cached: u32,
    ) -> Result<(String, u32, u32, u32)> {
        let policy = exploration_policy(stage, self.max_interactions);
        let tool_declarations = stage_tool_declarations(&self.tools, stage);
        let mut exploration_turns = 0usize;
        let mut progress_tool_calls_seen = 0usize;
        let mut empty_no_tool_reprompt_used = false;
        let mut tool_cache: HashMap<String, CachedToolCall> = HashMap::new();

        loop {
            if exploration_turns >= policy.max_interactions {
                return self
                    .finalize_stage_output(
                        stage,
                        &system_prompt,
                        &mut local_history,
                        &mut t_in,
                        &mut t_out,
                        &mut t_cached,
                    )
                    .await;
            }
            exploration_turns += 1;

            let request = self.bounded_exploration_request(
                stage,
                &system_prompt,
                &local_history,
                &tool_declarations,
            );

            let estimated = self.request_estimate(&request);
            if estimated > self.local_context_threshold(LOCAL_SKIP_EXPLORATION_PERCENT) {
                warn!(
                    "Stage {} prompt estimate {} is over the local {}% context guard (max_input_tokens {}); skipping exploration",
                    stage, estimated, LOCAL_SKIP_EXPLORATION_PERCENT, self.max_input_tokens
                );
                return self
                    .finalize_stage_output(
                        stage,
                        &system_prompt,
                        &mut local_history,
                        &mut t_in,
                        &mut t_out,
                        &mut t_cached,
                    )
                    .await;
            }

            let label = format!("stage {} exploration", stage);
            let resp = self.checked_generate_content(&label, request).await?;

            let mut force_finalize_due_to_context = false;
            if let Some(usage) = &resp.usage {
                force_finalize_due_to_context = self.record_stage_usage(
                    stage,
                    &label,
                    usage,
                    &mut t_in,
                    &mut t_out,
                    &mut t_cached,
                )?;
            }

            self.push_assistant_response(&resp, &mut local_history);

            let tool_calls = resp.tool_calls.clone().unwrap_or_default();
            if !tool_calls.is_empty() {
                let force_finalize = self
                    .execute_bounded_tool_calls(
                        BoundedToolTurn {
                            stage,
                            policy: &policy,
                            force_finalize_due_to_context,
                        },
                        &mut progress_tool_calls_seen,
                        &mut tool_cache,
                        tool_calls,
                        &mut local_history,
                    )
                    .await;
                if force_finalize || progress_tool_calls_seen >= policy.max_tool_calls {
                    return self
                        .finalize_stage_output(
                            stage,
                            &system_prompt,
                            &mut local_history,
                            &mut t_in,
                            &mut t_out,
                            &mut t_cached,
                        )
                        .await;
                }
            } else if resp.content.is_some() || resp.thought.is_some() {
                if let Some(final_content) = self
                    .handle_bounded_direct_response(
                        DirectResponseContext {
                            stage,
                            system_prompt: &system_prompt,
                            progress_tool_calls_seen,
                        },
                        resp,
                        &mut local_history,
                        &mut empty_no_tool_reprompt_used,
                        (&mut t_in, &mut t_out, &mut t_cached),
                    )
                    .await?
                {
                    return Ok(final_content);
                }
                continue;
            } else {
                return Err(anyhow::anyhow!("No content or tool calls from AI"));
            }
        }
    }

    async fn run_ai_stage_raw_native(
        &mut self,
        stage: u8,
        system_prompt: String,
        mut local_history: Vec<AiMessage>,
        mut t_in: u32,
        mut t_out: u32,
        mut t_cached: u32,
    ) -> Result<(String, u32, u32, u32)> {
        let mut turns = 0;
        let mut recitation_retries = 0;
        let mut last_tool_call_key: Option<String> = None;

        loop {
            turns += 1;
            if turns > self.max_interactions {
                break;
            }

            let request = crate::ai::AiRequest {
                system: Some(system_prompt.clone()),
                messages: local_history.clone(),
                tools: Some(self.tools.get_declarations_generic()),
                temperature: Some(self.temperature),
                response_format: None,
                context_tag: self
                    .context_tag
                    .as_ref()
                    .map(|prefix| format!("{} s:{}] ", &prefix[..prefix.len() - 2], stage)),
            };

            let label = format!("stage {} native", stage);
            let resp = match self.checked_generate_content(&label, request).await {
                Ok(response) => response,
                Err(e) => {
                    let err_msg = e.to_string();
                    let lower_err = err_msg.to_ascii_lowercase();
                    if (err_msg.contains("RECITATION") || lower_err.contains("blocked"))
                        && recitation_retries < 3
                    {
                        recitation_retries += 1;
                        warn!(
                            "Stage {} native response blocked by provider; retrying with anti-recitation reminder ({}/3)",
                            stage, recitation_retries
                        );
                        let reminder = AiMessage {
                            role: AiRole::User,
                            content: Some(
                                "IMPORTANT: Your previous response was blocked by a recitation filter. Do not copy large blocks of code verbatim. Describe the relevant logic in prose or short simplified expressions, then continue the review."
                                    .to_string(),
                            ),
                            thought: None,
                            thought_signature: None,
                            tool_calls: None,
                            tool_call_id: None,
                        };
                        local_history.push(reminder.clone());
                        self.global_history.push(reminder);
                        turns = turns.saturating_sub(1);
                        continue;
                    }
                    return Err(e);
                }
            };

            if let Some(usage) = &resp.usage {
                self.validate_prompt_usage(
                    &format!("stage {} native", stage),
                    usage.prompt_tokens,
                )?;
                t_in += usage.prompt_tokens as u32;
                t_out += usage.completion_tokens as u32;
                t_cached += usage.cached_tokens.unwrap_or(0) as u32;
            }

            let assistant_msg = AiMessage {
                role: AiRole::Assistant,
                content: resp.content.clone(),
                thought: resp.thought.clone(),
                thought_signature: resp.thought_signature.clone(),
                tool_calls: resp.tool_calls.clone(),
                tool_call_id: None,
            };
            local_history.push(assistant_msg.clone());
            self.global_history.push(assistant_msg);

            if let Some(tool_calls) = resp.tool_calls {
                let mut tool_responses = Vec::new();
                for call in tool_calls {
                    let key = tool_cache_key(&call.function_name, &call.arguments);
                    let result = if last_tool_call_key.as_deref() == Some(key.as_str()) {
                        json!({
                            "error": "Repeated identical tool call blocked. Finalize the stage using the evidence already gathered."
                        })
                        .to_string()
                    } else {
                        last_tool_call_key = Some(key);
                        match self
                            .tools
                            .call(&call.function_name, call.arguments.clone())
                            .await
                        {
                            Ok(v) => v.to_string(),
                            Err(e) => json!({"error": e.to_string()}).to_string(),
                        }
                    };
                    tool_responses.push(AiMessage {
                        role: AiRole::Tool,
                        content: Some(result),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        tool_call_id: Some(call.id.clone()),
                    });
                }
                local_history.extend(tool_responses.clone());
                self.global_history.extend(tool_responses);
            } else if resp.content.is_some() || resp.thought.is_some() {
                return Ok((resp.content.unwrap_or_default(), t_in, t_out, t_cached));
            } else {
                return Err(anyhow::anyhow!("No content or tool calls from AI"));
            }
        }

        Err(ReviewError::LimitExceeded.into())
    }

    async fn finalize_stage_output(
        &mut self,
        stage: u8,
        system_prompt: &str,
        local_history: &mut Vec<AiMessage>,
        t_in: &mut u32,
        t_out: &mut u32,
        t_cached: &mut u32,
    ) -> Result<(String, u32, u32, u32)> {
        let Some(schema) = stage_response_schema(stage) else {
            return Err(ReviewError::LimitExceeded.into());
        };

        let final_prompt = stage_finalization_prompt(stage);
        let final_user_msg = AiMessage {
            role: AiRole::User,
            content: Some(final_prompt.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };
        local_history.push(final_user_msg.clone());
        self.global_history.push(final_user_msg);

        let final_request = crate::ai::AiRequest {
            system: Some(system_prompt.to_string()),
            messages: local_history.clone(),
            tools: None,
            temperature: Some(self.temperature),
            response_format: Some(AiResponseFormat::Json {
                schema: Some(schema),
            }),
            context_tag: self
                .context_tag
                .as_ref()
                .map(|prefix| format!("{} s:{}:final] ", &prefix[..prefix.len() - 2], stage)),
        };

        let estimated = self.request_estimate(&final_request);
        if estimated > self.prompt_preflight_cap() {
            warn!(
                "Stage {} finalization prompt estimate {} exceeds preflight cap {} (max_input_tokens {}); returning typed budget error",
                stage,
                estimated,
                self.prompt_preflight_cap(),
                self.max_input_tokens
            );
            return Err(ReviewError::BudgetExceeded(format!(
                "stage {stage} finalization prompt estimate {estimated} exceeds preflight cap {} (max_input_tokens {})",
                self.prompt_preflight_cap(),
                self.max_input_tokens
            ))
            .into());
        }

        let label = format!("stage {} finalization", stage);
        let final_resp = self.checked_generate_content(&label, final_request).await?;

        if let Some(usage) = &final_resp.usage {
            self.validate_prompt_usage(
                &format!("stage {} finalization", stage),
                usage.prompt_tokens,
            )?;
            *t_in += usage.prompt_tokens as u32;
            *t_out += usage.completion_tokens as u32;
            *t_cached += usage.cached_tokens.unwrap_or(0) as u32;
        }

        let final_content = final_resp.content.unwrap_or_default();
        let final_assistant_msg = AiMessage {
            role: AiRole::Assistant,
            content: Some(final_content.clone()),
            thought: final_resp.thought,
            thought_signature: final_resp.thought_signature,
            tool_calls: final_resp.tool_calls,
            tool_call_id: None,
        };
        local_history.push(final_assistant_msg.clone());
        self.global_history.push(final_assistant_msg);

        Ok((final_content, *t_in, *t_out, *t_cached))
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
        let resp = match self.checked_generate_content(label, req).await {
            Ok(r) => r,
            Err(e) => {
                warn!("{} completion failed: {}", label, e);
                return None;
            }
        };
        if let Some(usage) = &resp.usage {
            if let Err(e) = self.validate_prompt_usage(label, usage.prompt_tokens) {
                warn!("{} completion exceeded input cap: {}", label, e);
                return None;
            }
            accumulate(tokens, usage);
        }
        let content = resp.content.as_deref().unwrap_or("");
        match try_parse(content, &validate) {
            Ok(v) => return Some(v),
            Err(e) => {
                if !should_retry_json_correction(&e, content, self.stage_protocol) {
                    warn!(
                        "{}: {}; response appears truncated after {} bytes, skipping correction retry",
                        label,
                        e,
                        content.len()
                    );
                    return None;
                }
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
                match self.checked_generate_content(label, retry_req).await {
                    Ok(resp2) => {
                        if let Some(usage) = &resp2.usage {
                            if let Err(e) = self.validate_prompt_usage(label, usage.prompt_tokens) {
                                warn!("{} retry exceeded input cap: {}", label, e);
                                return None;
                            }
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
}

fn assign_minimal_fallback_source_ids(concerns: &[Value], findings: &mut Value) {
    let Some(findings) = findings.as_array_mut() else {
        return;
    };

    let mut used_source_ids: HashSet<String> = findings
        .iter()
        .filter_map(|finding| finding.get("source_concern_id"))
        .filter_map(|source_id| source_id.as_str())
        .filter(|source_id| !source_id.trim().is_empty())
        .map(str::to_string)
        .collect();

    for finding in findings {
        if finding
            .get("source_concern_id")
            .and_then(|source_id| source_id.as_str())
            .is_some_and(|source_id| !source_id.trim().is_empty())
        {
            continue;
        }

        let finding_text = minimal_fallback_match_text(finding);
        let mut best_id = None;
        let mut best_score = 0usize;
        let mut tied = false;

        for concern in concerns {
            let Some(source_id) = concern.get("source_concern_id").and_then(|id| id.as_str())
            else {
                continue;
            };
            if used_source_ids.contains(source_id) {
                continue;
            }

            let score = minimal_fallback_match_score(concern, &finding_text);
            if score > best_score {
                best_score = score;
                best_id = Some(source_id.to_string());
                tied = false;
            } else if score == best_score && score > 0 {
                tied = true;
            }
        }

        if best_score >= 4
            && !tied
            && let Some(source_id) = best_id
            && let Some(finding_obj) = finding.as_object_mut()
        {
            finding_obj.insert(
                "source_concern_id".to_string(),
                Value::String(source_id.clone()),
            );
            used_source_ids.insert(source_id);
        }
    }
}

fn minimal_fallback_match_score(concern: &Value, finding_text: &str) -> usize {
    if finding_text.trim().is_empty() {
        return 0;
    }

    if let Some(source_id) = concern.get("source_concern_id").and_then(|id| id.as_str())
        && !source_id.trim().is_empty()
        && finding_text.contains(&source_id.to_ascii_lowercase())
    {
        return 1000;
    }

    let concern_text = minimal_fallback_match_text(concern);
    if concern_text.trim().is_empty() {
        return 0;
    }

    if concern_text.len() >= 32 && finding_text.contains(&concern_text) {
        return 200;
    }

    let concern_terms = minimal_fallback_match_terms(&concern_text);
    let finding_terms = minimal_fallback_match_terms(finding_text);
    concern_terms.intersection(&finding_terms).count()
}

fn minimal_fallback_match_text(value: &Value) -> String {
    fn collect_strings(value: &Value, output: &mut Vec<String>) {
        match value {
            Value::String(text) => output.push(text.to_ascii_lowercase()),
            Value::Array(items) => {
                for item in items {
                    collect_strings(item, output);
                }
            }
            Value::Object(map) => {
                for value in map.values() {
                    collect_strings(value, output);
                }
            }
            _ => {}
        }
    }

    let mut parts = Vec::new();
    collect_strings(value, &mut parts);
    parts.join(" ")
}

fn minimal_fallback_match_terms(text: &str) -> HashSet<String> {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|term| term.len() >= 4)
        .filter(|term| {
            !matches!(
                *term,
                "this"
                    | "that"
                    | "with"
                    | "from"
                    | "into"
                    | "stage"
                    | "stage8"
                    | "stage9"
                    | "concern"
                    | "finding"
                    | "source"
                    | "preexisting"
                    | "severity"
                    | "explanation"
                    | "insufficient"
                    | "evidence"
            )
        })
        .map(str::to_string)
        .collect()
}

fn parse_stage_json(raw_text: &str) -> Value {
    let cleaned = crate::utils::clean_json_string(raw_text);
    serde_json::from_str(&cleaned).unwrap_or_else(|_| {
        let cands = find_json_candidates(raw_text);
        cands.into_iter().last().unwrap_or(json!({}))
    })
}

fn stage_has_empty_concerns(raw_text: &str) -> bool {
    parse_stage_json(raw_text)
        .get("concerns")
        .and_then(|c| c.as_array())
        .is_some_and(|concerns| concerns.is_empty())
}

fn should_retry_json_correction(error: &str, content: &str, stage_protocol: StageProtocol) -> bool {
    if stage_protocol != StageProtocol::BoundedLocalModel {
        return true;
    }

    if content.len() <= LOCAL_JSON_CORRECTION_MAX_CONTENT_CHARS {
        return true;
    }

    !json_error_looks_truncated(error)
}

fn json_error_looks_truncated(error: &str) -> bool {
    error.to_ascii_lowercase().contains("eof while parsing")
}

fn stage_finalization_prompt(stage: u8) -> &'static str {
    match stage {
        8 => {
            "Finalize this stage now. Tools are disabled. Based only on the aggregated concerns and context above, emit ONLY a JSON object with top-level \"concerns\" and \"dropped_concerns\" arrays. Preserve every unique concern still supported by the gathered evidence. For each input concern discarded or merged away, include a dropped_concerns item with \"description\" and \"reason\". If no concerns remain, return {\"concerns\":[],\"dropped_concerns\":[]}. Do not include markdown, prose, TODOs, or tool calls."
        }
        9 => {
            "Finalize this stage now. Tools are disabled. Based only on the stage instructions, target patch, and context/tool results above, emit ONLY a JSON object with top-level \"findings\" and \"dropped_candidates\" arrays. Every source_concern_id from the consolidated concerns must appear exactly once, either in a finding or a dropped candidate. Preserve every plausible finding still supported by the gathered evidence. Prefer one finding per root cause. Findings must describe bugs/regressions only; do not emit a finding saying the patch is correct, safe, redundant, or improves performance. For argument-order/API-contract concerns involving swapped, reversed, or wrong-order arguments or parameters, emit one finding that explicitly preserves the callee name, expected parameter order/signature, actual call-site argument order, and why the order is wrong; mark duplicate argument-order concerns as drop_reason=\"subsumed_by\" with subsumed_by_finding_id pointing to that finding. Do not drop argument-order concerns as false_positive. For concerns about removing local_irq_save/local_irq_restore around write_seqcount_begin/write_seqcount_end, do not drop as false_positive merely because a callee is irq-safe; either emit a finding about the sequence-counter writer interruptibility or prove no interrupt-context reader can spin/retry on that seqcount. For concerns about newly added allocations/resources leaking on error or cleanup paths, do not drop with generic helper wording; either emit a finding or name the exact resource and concrete deallocation expression/path for that same resource. For teardown lifecycle-ordering concerns involving workqueues, timers, rfkill, unregister callbacks, close/remove paths, or callbacks that can re-enter teardown, either emit a finding or prove the exact unregister/destroy order, callback path, and synchronization/barrier/cancel operation that prevents a callback such as nci_close_device from observing freed state. For proof-required static or targeted seed concerns, either emit a finding, mark it subsumed_by a finding that preserves the exact seeded bug mechanism, or drop it as false_positive with concrete proof. Do not drop proof-required seeds as duplicate, unclear, insufficient_evidence, too speculative, not_security_relevant, or generic cleanup/API concern. Drop non-preserved concerns only with a concrete rationale and one of these drop_reason values: duplicate, subsumed_by, insufficient_evidence, not_security_relevant, already_mitigated, false_positive, unclear. Do not include markdown, prose, TODOs, or tool calls."
        }
        _ => {
            "Finalize this stage now. Tools are disabled. Based only on the stage instructions, target patch, and context/tool results above, emit ONLY a JSON object with a top-level \"concerns\" array. If the exploration above produced a draft concerns object or plausible concern, preserve every item still supported by the gathered evidence. If there are no concerns, return {\"concerns\": []}. Do not include markdown, prose, TODOs, or tool calls."
        }
    }
}

#[derive(Debug, Clone)]
struct CachedToolCall {
    requests: usize,
    result: String,
}

#[derive(Debug, Clone, Copy)]
struct ExplorationPolicy {
    max_tool_calls: usize,
    max_interactions: usize,
}

fn exploration_policy(stage: u8, configured_max_interactions: usize) -> ExplorationPolicy {
    let (max_tool_calls, max_interactions) = match stage {
        1 | 2 => (10, 16),
        3 => (18, 24),
        4..=8 => (12, 18),
        9 => (18, 24),
        BUG_PATTERN_STAGE => (12, 12),
        ARGUMENT_ORDER_STAGE => (5, 8),
        _ => (usize::MAX, configured_max_interactions),
    };

    ExplorationPolicy {
        max_tool_calls,
        max_interactions: max_interactions.min(configured_max_interactions),
    }
}

fn per_turn_tool_call_cap(stage: u8) -> usize {
    match stage {
        3 => 6,
        BUG_PATTERN_STAGE => 3,
        ARGUMENT_ORDER_STAGE => 2,
        _ => 4,
    }
}

fn stage_tool_declarations(tools: &ToolBox, stage: u8) -> Vec<crate::ai::AiTool> {
    if stage == 8 {
        return Vec::new();
    }

    let mut declarations = tools.get_declarations_generic();
    if (1..=9).contains(&stage) || stage == ARGUMENT_ORDER_STAGE || stage == BUG_PATTERN_STAGE {
        declarations.retain(|tool| {
            !matches!(
                tool.name.to_ascii_lowercase().as_str(),
                "todowrite" | "todoread"
            )
        });
    }
    declarations
}

fn tool_cache_key(function_name: &str, arguments: &Value) -> String {
    format!(
        "{}:{}",
        function_name.trim().to_ascii_lowercase(),
        serde_json::to_string(arguments).unwrap_or_else(|_| arguments.to_string())
    )
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

fn find_json_candidates(text: &str) -> Vec<Value> {
    let mut candidates = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '{'
            && let Some(end) = find_matching_brace(&chars, i)
        {
            let candidate: String = chars[i..=end].iter().collect();
            let clean_candidate = crate::utils::clean_json_string(&candidate);
            if let Ok(v) =
                serde_json::from_str(&clean_candidate).or_else(|_| serde_json::from_str(&candidate))
            {
                candidates.push(v);
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    candidates
}

fn find_matching_brace(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;

    for (i, c) in chars.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if *c == '\\' {
                escape = true;
            } else if *c == '"' {
                in_string = false;
            }
        } else if *c == '"' {
            in_string = true;
        } else if *c == '{' {
            depth += 1;
        } else if *c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_local_skips_large_truncated_json_correction() {
        let content = format!(
            "{{\"concerns\":[{{\"description\":\"{}",
            "x".repeat(LOCAL_JSON_CORRECTION_MAX_CONTENT_CHARS)
        );

        assert!(!should_retry_json_correction(
            "JSON parse error: EOF while parsing a string at line 256 column 318",
            &content,
            StageProtocol::BoundedLocalModel
        ));
    }

    #[test]
    fn bounded_local_still_retries_short_invalid_json() {
        assert!(should_retry_json_correction(
            "JSON parse error: EOF while parsing a string at line 1 column 20",
            "{\"concerns\":[{\"description\":\"unterminated",
            StageProtocol::BoundedLocalModel
        ));
    }

    #[test]
    fn native_protocol_keeps_json_correction_retry() {
        let content = format!(
            "{{\"concerns\":[{{\"description\":\"{}",
            "x".repeat(LOCAL_JSON_CORRECTION_MAX_CONTENT_CHARS)
        );

        assert!(should_retry_json_correction(
            "JSON parse error: EOF while parsing a string at line 256 column 318",
            &content,
            StageProtocol::Native
        ));
    }

    #[test]
    fn test_exploration_policy_caps_verification_stage() {
        let policy = exploration_policy(9, 50);

        assert_eq!(policy.max_tool_calls, 18);
        assert_eq!(policy.max_interactions, 24);
    }

    #[test]
    fn test_minimal_fallback_schema_matches_findings_only_prompt() {
        let schema = minimal_fallback_response_schema().unwrap();

        assert_eq!(
            schema
                .get("required")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap(),
            vec![serde_json::json!("findings")]
        );
        assert!(schema.pointer("/properties/dropped_candidates").is_none());
        assert_eq!(
            stage_response_schema(9)
                .unwrap()
                .get("required")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap(),
            vec![
                serde_json::json!("findings"),
                serde_json::json!("dropped_candidates")
            ]
        );
    }

    #[test]
    fn test_static_bug_pattern_seed_catches_dmem_classes() {
        let diff = r#"
+static int limit_key_write(char *options)
+{
+    char *name = strsep(&options, " ");
+    if (!strcmp(options, "max"))
+        return 0;
+    memparse(options, NULL);
+}
+static ssize_t limit_region_max_write(...)
+{
+    return limit_key_write(options);
+}
+static void region_unregister(...)
+{
+    list_del_rcu(&region->region_node);
+    list_for_each_rcu(entry, &region->pools)
+        cleanup(entry);
+}
+.name = "max",
"#;
        let seeded = seed_bug_pattern_concerns_from_diff(diff);
        let text = seeded
            .iter()
            .map(concern_review_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("limit.max keyed write parsing"));
        assert!(text.contains("region_unregister() uses RCU"));
        assert!(seeded.iter().all(is_proof_required_seed_concern));
        assert!(seeded.iter().any(|concern| {
            concern.get("pattern").and_then(|value| value.as_str())
                == Some("cgroup_keyed_parse_missing_value")
        }));
        assert!(seeded.iter().any(|concern| {
            concern.get("pattern").and_then(|value| value.as_str())
                == Some("rcu_teardown_iteration_without_read_lock")
        }));
    }

    #[test]
    fn test_static_bug_pattern_seed_catches_t7xx_skb_frag_capacity() {
        let diff = r#"
+static int t7xx_dpmaif_set_frag_to_skb(struct sk_buff *skb)
+{
+    skb_add_rx_frag(skb, skb_shinfo(skb)->nr_frags, page, offset, len, truesize);
+    return 0;
+}
"#;
        let seeded = seed_bug_pattern_concerns_from_diff(diff);
        let text = seeded
            .iter()
            .map(concern_review_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("MAX_SKB_FRAGS"));
        assert!(text.contains("nr_frags"));
        assert!(seeded.iter().all(is_proof_required_seed_concern));
        assert_eq!(
            seeded[0].get("pattern").and_then(|value| value.as_str()),
            Some("skb_fragment_capacity_max_skb_frags")
        );
    }

    #[test]
    fn test_static_bug_pattern_seed_catches_retry_response_buffer_leak() {
        let diff = r#"
 int retry_open_file(...)
 {
     struct kvec retry_iov = {};
     int err_buftype = CIFS_NO_BUFFER;
     bool retry_without_read_attributes = false;
     rc = retry_open(xid, oparms, smb2_path, &smb2_oplock, smb2_data, NULL,
                    &retry_iov, &err_buftype);
     if (rc == -EACCES && retry_without_read_attributes) {
         oparms->desired_access &= ~FILE_READ_ATTRIBUTES;
         rc = retry_open(xid, oparms, smb2_path, &smb2_oplock, smb2_data, NULL,
                        &retry_iov, &err_buftype);
     }
 out:
     free_response_buf(err_buftype, retry_iov.iov_base);
 }
"#;
        let seeded = seed_bug_pattern_concerns_from_diff(diff);
        let text = seeded
            .iter()
            .map(concern_review_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("retry_open"));
        assert!(text.contains("retry_iov.iov_base"));
        assert!(text.contains("free_response_buf"));
        assert!(seeded.iter().all(is_proof_required_seed_concern));
        assert!(seeded.iter().any(|concern| {
            concern.get("pattern").and_then(|value| value.as_str())
                == Some("retry_error_path_resource_leak")
        }));
    }

    #[test]
    fn test_static_lifecycle_seed_catches_nci_unregister_ordering() {
        let diff = r#"
+static int nci_close_device(struct nci_dev *ndev)
+{
+    flush_workqueue(ndev->cmd_wq);
+    ndev->ops->close(ndev);
+    return 0;
+}
+void nci_unregister_device(struct nci_dev *ndev)
+{
+    nci_close_device(ndev);
+    destroy_workqueue(ndev->cmd_wq);
+    destroy_workqueue(ndev->rx_wq);
+    destroy_workqueue(ndev->tx_wq);
+    nfc_unregister_device(ndev->nfc_dev);
+}
"#;
        let seeded = seed_lifecycle_ordering_concerns_from_diff(diff);
        let text = seeded
            .iter()
            .map(concern_review_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("nci_unregister_device"));
        assert!(text.contains("rfkill"));
        assert!(text.contains("destroyed"));
    }

    #[test]
    fn test_minimal_fallback_does_not_promote_static_lifecycle_seed() {
        let concerns = seed_lifecycle_ordering_concerns_from_diff(
            r#"
+static int nci_close_device(struct nci_dev *ndev) { return 0; }
+void nci_unregister_device(struct nci_dev *ndev)
+{
+    nci_close_device(ndev);
+    destroy_workqueue(ndev->cmd_wq);
+    nfc_unregister_device(ndev->nfc_dev);
+}
"#,
        );
        let mut findings = serde_json::json!([]);

        preserve_static_lifecycle_ordering_findings(&concerns, &mut findings);

        assert!(findings.as_array().unwrap().is_empty());

        let mut unrelated_findings = serde_json::json!([
            {
                "problem": "An unrelated cleanup path has incomplete logging.",
                "severity": "Low",
                "severity_explanation": "This does not mention the NFC lifecycle path.",
                "preexisting": false
            }
        ]);
        preserve_static_lifecycle_ordering_findings(&concerns, &mut unrelated_findings);

        let unrelated_findings = unrelated_findings.as_array().unwrap();
        assert_eq!(unrelated_findings.len(), 1);
        assert!(!finding_review_text(&unrelated_findings[0]).contains("rfkill"));
    }

    #[test]
    fn test_minimal_fallback_upgrades_matching_static_lifecycle_finding() {
        let concerns = seed_lifecycle_ordering_concerns_from_diff(
            r#"
+static int nci_close_device(struct nci_dev *ndev) { return 0; }
+void nci_unregister_device(struct nci_dev *ndev)
+{
+    nci_close_device(ndev);
+    destroy_workqueue(ndev->cmd_wq);
+    nfc_unregister_device(ndev->nfc_dev);
+}
"#,
        );
        let mut findings = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "problem": "nci_close_device() can re-enter teardown through an unregister callback after cmd_wq has been destroyed.",
                "severity": "High",
                "severity_explanation": "The nci_unregister_device() path destroys the command workqueue before nfc_unregister_device() can trigger callbacks.",
                "preexisting": false
            }
        ]);

        preserve_static_lifecycle_ordering_findings(&concerns, &mut findings);
        let findings = findings.as_array().unwrap();
        assert_eq!(findings.len(), 1);
        let text = finding_review_text(&findings[0]);

        assert!(text.contains("nci_close_device"));
        assert!(text.contains("rfkill"));
        assert!(text.contains("destroyed"));
    }

    #[test]
    fn test_stage9_source_ids_replace_duplicate_model_ids() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "duplicate-model-id",
                "description": "first concern"
            },
            {
                "source_concern_id": "duplicate-model-id",
                "description": "second concern"
            }
        ]);

        let with_source_ids = add_stage9_source_ids(&concerns);
        let array = with_source_ids.as_array().unwrap();

        assert_eq!(
            array[0].get("source_concern_id").and_then(|id| id.as_str()),
            Some("stage8-001")
        );
        assert_eq!(
            array[1].get("source_concern_id").and_then(|id| id.as_str()),
            Some("stage8-002")
        );
        assert_eq!(
            array[0]
                .get("model_source_concern_id")
                .and_then(|id| id.as_str()),
            Some("duplicate-model-id")
        );
        assert_eq!(
            array[1]
                .get("model_source_concern_id")
                .and_then(|id| id.as_str()),
            Some("duplicate-model-id")
        );
    }

    #[test]
    fn test_stage9_rejects_argument_order_duplicate_drop() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "API Argument Order",
                "description": "Incorrect argument order in call to release_sgls: sg_list and sge are swapped",
                "reasoning": "The callee signature expects sge before sg_list, but the call site passes sg_list before sge.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "duplicate",
                "rationale": "Merged into a generic API contract concern."
            }
        ]);

        let err = validate_stage9_accounting(&concerns, &findings, &dropped).unwrap_err();
        assert!(err.contains("argument-order concern stage8-001"));
    }

    #[test]
    fn test_stage9_rejects_argument_order_false_positive_drop() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "API Argument Order",
                "description": "Incorrect argument order in call to release_sgls: sg_list and sge are swapped",
                "reasoning": "The callee signature expects sge before sg_list, but the call site passes sg_list before sge.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "The signature is release_sgls(req, sge, sg_list) and the call is release_sgls(req, sg_list, sge)."
            }
        ]);

        let err = validate_stage9_accounting(&concerns, &findings, &dropped).unwrap_err();
        assert!(err.contains("dropped as false_positive"));
    }

    #[test]
    fn test_stage9_allows_vague_argument_order_drop() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "API Argument Order",
                "description": "Potential argument-order/API contract issue in a changed call",
                "reasoning": "The concern does not identify the callee signature, expected parameter order, actual call-site order, or concrete swapped roles.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "insufficient_evidence",
                "rationale": "The retained concern only states a possible API-contract issue and does not preserve a concrete expected-vs-actual argument order."
            }
        ]);

        validate_stage9_accounting(&concerns, &findings, &dropped).unwrap();
    }

    #[test]
    fn test_stage9_repair_does_not_synthesize_vague_argument_order_finding() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "API Argument Order",
                "description": "Potential argument-order/API contract issue in a changed call",
                "reasoning": "The concern does not identify the callee signature, expected parameter order, actual call-site order, or concrete swapped roles.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "duplicate",
                "rationale": "Stage 9 treated this as duplicate of a generic API-contract concern."
            }
        ]);

        let (repaired_findings, repaired_drops) =
            repair_stage9_accounting(&concerns, &findings, &dropped);

        validate_stage9_accounting(&concerns, &repaired_findings, &repaired_drops).unwrap();
        assert_eq!(repaired_findings.as_array().unwrap().len(), 0);
        assert_eq!(repaired_drops.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_stage9_rejects_argument_order_finding_without_role_names() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "API Argument Order",
                "description": "Incorrect argument order in call to release_sgls",
                "reasoning": "The function release_sgls takes parameters `req`, `sge`, and `sg_list`, but the call site passes descriptor as the second argument and list as the third.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([
            {
                "finding_id": "finding-1",
                "source_concern_id": "stage8-001",
                "problem": "The release_sgls function has incorrect argument order in its call site, passing descriptor as the second argument and list as the third, which reverses the intended parameter order.",
                "severity": "High",
                "severity_explanation": "Incorrect argument order can lead to incorrect function behavior, potentially causing memory corruption or incorrect DMA unmap operations.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([]);

        let err = validate_stage9_accounting(&concerns, &findings, &dropped).unwrap_err();
        assert!(err.contains("argument-order concern stage8-001"));
    }

    #[test]
    fn test_stage9_rejects_seqcount_irq_drop_based_only_on_percpu_counter() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Deadlock Prevention Regression",
                "description": "Removal of local_irq_save/restore in fprop_new_period may reintroduce a deadlock",
                "reasoning": "The patch removes local_irq_save around write_seqcount_begin/write_seqcount_end. A hardirq reader can retry on the same sequence counter while the writer is interrupted.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "percpu_counter functions are irq-safe due to raw_spin_lock_irqsave, so the outer local_irq_save is redundant."
            }
        ]);

        let err = validate_stage9_accounting(&concerns, &findings, &dropped).unwrap_err();
        assert!(err.contains("sequence-counter writer"));
    }

    #[test]
    fn test_stage9_rejects_resource_cleanup_drop_without_exact_free() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Memory Leak",
                "description": "stripe_uptodate_bitmap allocated by bitmap_zalloc in alloc_rbio may leak on the ERR_PTR(-ENOMEM) path",
                "reasoning": "The error path calls free_raid_bio_pointers(rbio), but that helper frees error_bitmap and does not show bitmap_free(rbio->stripe_uptodate_bitmap).",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "free_raid_bio_pointers() frees all rbio resources including stripe_uptodate_bitmap."
            }
        ]);

        let err = validate_stage9_accounting(&concerns, &findings, &dropped).unwrap_err();
        assert!(err.contains("resource-cleanup concern stage8-001"));
    }

    #[test]
    fn test_stage9_does_not_treat_generic_resource_cleanup_as_preserved_class() {
        let concern = serde_json::json!({
            "source_concern_id": "stage8-001",
            "type": "Memory Leak",
            "description": "Newly allocated resource may leak on the error cleanup path: the newly added allocation/resource",
            "reasoning": "Stage 9 did not prove a concrete deallocation expression for the same newly allocated resource.",
            "preexisting": false
        });

        assert!(!is_resource_cleanup_concern(&concern));
    }

    #[test]
    fn test_stage9_allows_resource_cleanup_drop_with_exact_free() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Memory Leak",
                "description": "stripe_uptodate_bitmap allocated by bitmap_zalloc in alloc_rbio may leak on the ERR_PTR(-ENOMEM) path",
                "reasoning": "The error path must free stripe_uptodate_bitmap.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "The cleanup path calls bitmap_free(rbio->stripe_uptodate_bitmap) before returning ERR_PTR(-ENOMEM)."
            }
        ]);

        validate_stage9_accounting(&concerns, &findings, &dropped).unwrap();
    }

    #[test]
    fn test_stage9_rejects_retry_resource_drop_without_before_retry_proof() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Resource Management",
                "description": "Potential leak of retry_open response buffer when the -EACCES fallback retries without read attributes",
                "reasoning": "The first retry_open can populate retry_iov.iov_base, then the code retries the open without proving free_response_buf() runs before the second retry_open overwrites or loses the response buffer.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "The retry_iov buffer is freed in the free_response_buf() call in the out label, which is executed regardless of whether the second retry_open call succeeds or fails. The buffer is properly cleaned up before any potential reuse."
            }
        ]);

        let err = validate_stage9_accounting(&concerns, &findings, &dropped).unwrap_err();
        assert!(err.contains("retry-resource concern stage8-001"));
    }

    #[test]
    fn test_stage9_allows_retry_resource_drop_with_before_retry_proof() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Resource Management",
                "description": "Potential leak of retry_open response buffer when the -EACCES fallback retries without read attributes",
                "reasoning": "The first retry_open can populate retry_iov.iov_base, then the code retries the open without proving free_response_buf() runs before the second retry_open overwrites or loses the response buffer.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "In retry_open_file(), the retry path first branches to the retry_cleanup label; that cleanup path calls free_response_buf(resp_buftype, retry_iov.iov_base) before the second retry_open fallback is issued. Because retry_iov.iov_base is freed before retry and before reuse/overwrite, the response buffer cannot leak or be overwritten."
            }
        ]);

        validate_stage9_accounting(&concerns, &findings, &dropped).unwrap();
    }

    #[test]
    fn test_stage9_repair_converts_retry_resource_drop_to_finding() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Resource Management",
                "description": "Potential leak of retry_open response buffer when the -EACCES fallback retries without read attributes",
                "reasoning": "The first retry_open can populate retry_iov.iov_base, then the code retries the open without proving free_response_buf() runs before the second retry_open overwrites or loses the response buffer.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "free_response_buf runs at the out label."
            }
        ]);

        let (repaired_findings, repaired_drops) =
            repair_stage9_accounting(&concerns, &findings, &dropped);
        validate_stage9_accounting(&concerns, &repaired_findings, &repaired_drops).unwrap();
        assert_eq!(repaired_findings.as_array().unwrap().len(), 1);
        assert_eq!(repaired_drops.as_array().unwrap().len(), 0);
        let text = finding_review_text(&repaired_findings[0]);
        assert!(text.contains("retry_open"));
        assert!(text.contains("retry_iov.iov_base"));
        assert!(text.contains("free_response_buf"));
        assert!(text.contains("retry"));
        assert!(text.contains("leak"));
    }

    #[test]
    fn test_stage9_rejects_lifecycle_drop_without_ordering_proof() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Lifecycle Ordering",
                "description": "The command workqueue is destroyed before rfkill unregister can stop callbacks",
                "reasoning": "rfkill can call nci_close_device through the close callback after destroy_workqueue freed the command workqueue, so teardown can race into freed state.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "The code properly handles cleanup order."
            }
        ]);

        let err = validate_stage9_accounting(&concerns, &findings, &dropped).unwrap_err();
        assert!(err.contains("lifecycle-ordering concern stage8-001"));
    }

    #[test]
    fn test_stage8_propagates_proof_required_seed_metadata() {
        let input = vec![serde_json::json!({
            "source": "static_bug_pattern_seed",
            "pattern": "cgroup_keyed_parse_missing_value",
            "preservation": "proof_required_drop",
            "preservation_policy": "proof_required_drop",
            "required_evidence": ["file", "function", "key or option name", "missing value path", "dereference/parse site"],
            "type": "Cgroup keyed file parsing / missing value",
            "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
            "reasoning": "kernel/cgroup/limit.c limit_key_write handles limit.max writes where a missing value can reach strcmp/memparse before a non-NULL value pointer is proven.",
            "preexisting": false
        })];
        let mut retained = serde_json::json!([
            {
                "type": "Cgroup keyed file parsing / missing value",
                "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
                "reasoning": "kernel/cgroup/limit.c limit_key_write handles limit.max cgroup file writes where a missing value or absent option can reach strcmp/memparse parsing before proving the value pointer is non-NULL.",
                "preexisting": false
            }
        ]);

        let restored = preserve_stage8_proof_required_seed_concerns(&input, &mut retained);

        assert_eq!(restored, 0);
        assert_eq!(
            retained[0]
                .get("preservation")
                .and_then(|value| value.as_str()),
            Some("proof_required_drop")
        );
        assert_eq!(
            retained[0].get("pattern").and_then(|value| value.as_str()),
            Some("cgroup_keyed_parse_missing_value")
        );
    }

    #[test]
    fn test_stage9_rejects_proof_required_seed_generic_drop() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "source": "static_bug_pattern_seed",
                "pattern": "cgroup_keyed_parse_missing_value",
                "preservation": "proof_required_drop",
                "type": "Cgroup keyed file parsing / missing value",
                "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
                "reasoning": "kernel/cgroup/limit.c limit_key_write handles limit.max writes where a missing value can reach strcmp/memparse before a non-NULL value pointer is proven.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "The code properly handles cleanup and checks for NULL values."
            }
        ]);

        let err = validate_stage9_accounting(&concerns, &findings, &dropped).unwrap_err();
        assert!(err.contains("proof-required seed concern stage8-001"));
    }

    #[test]
    fn test_stage9_allows_proof_required_seed_drop_with_concrete_cgroup_proof() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "source": "static_bug_pattern_seed",
                "pattern": "cgroup_keyed_parse_missing_value",
                "preservation": "proof_required_drop",
                "type": "Cgroup keyed file parsing / missing value",
                "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
                "reasoning": "kernel/cgroup/limit.c limit_key_write handles limit.max writes where a missing value can reach strcmp/memparse before a non-NULL value pointer is proven.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "In kernel/cgroup/limit.c, limit_key_write for limit.max rejects a missing value before strcmp/memparse parse access: the handler checks the options value pointer for non-NULL before parsing the \"max\" limit option, so a bare key without a value cannot reach the dereference/parse site."
            }
        ]);

        validate_stage9_accounting(&concerns, &findings, &dropped).unwrap();
    }

    #[test]
    fn test_cgroup_seed_detail_rejects_generic_non_max_value_finding() {
        let concern = serde_json::json!({
            "source_concern_id": "stage8-001",
            "source": "static_bug_pattern_seed",
            "pattern": "cgroup_keyed_parse_missing_value",
            "preservation": "proof_required_drop",
            "type": "Cgroup keyed file parsing / missing value",
            "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
            "reasoning": "kernel/cgroup/limit.c limit_key_write handles limit.max writes where a bare max key can reach strcmp/memparse before a non-NULL value pointer is proven.",
            "preexisting": false
        });
        let generic_finding = serde_json::json!({
            "source_concern_id": "stage8-001",
            "problem": "limit_key_write() does not validate that the value string is non-NULL before calling memparse() for non-'max' values, which could lead to incorrect parsing or NULL dereference.",
            "severity": "High",
            "severity_explanation": "The function parses input strings for limit values but does not check if the value part is NULL before attempting to parse it with memparse().",
            "preexisting": false
        });

        assert!(!finding_preserves_seed_pattern_detail(
            &concern,
            &generic_finding
        ));
    }

    #[test]
    fn test_minimal_fallback_preserves_exact_cgroup_seed_over_generic_finding() {
        let existing_concerns = vec![serde_json::json!({
            "source_concern_id": "stage8-001",
            "source": "static_bug_pattern_seed",
            "pattern": "cgroup_keyed_parse_missing_value",
            "preservation": "proof_required_drop",
            "origin_stage": "bug_pattern_static_seed",
            "type": "Cgroup keyed file parsing / missing value",
            "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
            "reasoning": "kernel/cgroup/limit.c limit_region_max_write()/limit_key_write() handles limit.max writes where a bare max key can reach strcmp/memparse before a non-NULL value pointer is proven.",
            "preexisting": false
        })];
        let mut findings = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "problem": "The limit_key_write() function does not properly check for NULL values when parsing 'max' sentinel values, potentially leading to a NULL dereference or incorrect parsing.",
                "severity": "High",
                "severity_explanation": "This handles cgroup keyed writes where a key like 'max' can be provided without a value. If the value pointer is NULL after strsep(), the subsequent strcmp() or memparse() calls will cause a NULL dereference or incorrect parsing.",
                "preexisting": false
            }
        ]);

        preserve_static_bug_pattern_findings(&existing_concerns, &mut findings);

        let findings = findings.as_array().unwrap();
        assert_eq!(findings.len(), 1);
        let upgraded_text = finding_review_text(&findings[0]);
        assert!(upgraded_text.contains("max"));
        assert!(upgraded_text.contains("missing") || upgraded_text.contains("without a value"));
        assert!(upgraded_text.contains("cgroup"));
        assert!(finding_preserves_seed_pattern_detail(
            &existing_concerns[0],
            &findings[0]
        ));
    }

    #[test]
    fn test_minimal_fallback_does_not_promote_static_bug_seed() {
        let existing_concerns = vec![serde_json::json!({
            "source_concern_id": "stage8-001",
            "source": "static_bug_pattern_seed",
            "pattern": "cgroup_keyed_parse_missing_value",
            "preservation": "proof_required_drop",
            "origin_stage": "bug_pattern_static_seed",
            "type": "Cgroup keyed file parsing / missing value",
            "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
            "reasoning": "kernel/cgroup/limit.c limit_region_max_write()/limit_key_write() handles limit.max writes where a bare max key can reach strcmp/memparse before a non-NULL value pointer is proven.",
            "preexisting": false
        })];
        let mut findings = serde_json::json!([]);

        preserve_static_bug_pattern_findings(&existing_concerns, &mut findings);

        assert!(findings.as_array().unwrap().is_empty());

        let mut unrelated_findings = serde_json::json!([
            {
                "problem": "An unrelated allocation path may leak a temporary object.",
                "severity": "Medium",
                "severity_explanation": "This does not mention dmem keyed parsing.",
                "preexisting": false
            }
        ]);
        preserve_static_bug_pattern_findings(&existing_concerns, &mut unrelated_findings);

        let unrelated_findings = unrelated_findings.as_array().unwrap();
        assert_eq!(unrelated_findings.len(), 1);
        assert!(!finding_review_text(&unrelated_findings[0]).contains("limit.max"));
    }

    #[test]
    fn test_minimal_fallback_caps_unprotected_dmem_noise() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "source": "static_bug_pattern_seed",
                "pattern": "cgroup_keyed_parse_missing_value",
                "preservation": "proof_required_drop",
                "origin_stage": "bug_pattern_static_seed",
                "type": "Cgroup keyed file parsing / missing value",
                "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
                "reasoning": "kernel/cgroup/limit.c limit_region_max_write()/limit_key_write() handles limit.max writes where a bare max key can reach strcmp/memparse before a non-NULL value pointer is proven.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-002",
                "source": "static_bug_pattern_seed",
                "pattern": "rcu_teardown_iteration_without_read_lock",
                "preservation": "proof_required_drop",
                "origin_stage": "bug_pattern_static_seed",
                "type": "RCU list iteration in unregister/teardown path",
                "description": "region_unregister() uses list_for_each_rcu in unregister teardown without visible rcu_read_lock.",
                "reasoning": "region_unregister() is an unregister path using list_for_each_rcu without rcu_read_lock or update-side lockdep proof.",
                "preexisting": false
            }
        ]);
        let mut findings = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "problem": "limit.max accepts a keyed write without proving the value is present.",
                "severity": "High",
                "severity_explanation": "kernel/cgroup/limit.c limit_region_max_write()/limit_key_write() handles limit.max. A bare max key or missing value can leave the value pointer absent before strcmp(), memparse(), or other parsing, causing NULL dereference or invalid parse from the cgroup write path.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-002",
                "problem": "region_unregister() traverses an RCU list during unregister without read-side protection.",
                "severity": "High",
                "severity_explanation": "region_unregister() runs in unregister/teardown context and uses list_for_each_rcu. Stage 9 did not prove rcu_read_lock(), a documented update-side lock, or a lockdep condition covers the traversal, so teardown can trigger a suspicious RCU warning or race.",
                "preexisting": false
            },
            {
                "problem": "dmem_cgroup_try_charge() may mishandle ret_limit_pool.",
                "severity": "Medium",
                "severity_explanation": "A local error path can return without setting the optional pool pointer.",
                "preexisting": false
            },
            {
                "problem": "dmem_cgroup_try_charge() may miss an allocation error check.",
                "severity": "Medium",
                "severity_explanation": "get_cg_pool_unlocked() could fail and the path may continue.",
                "preexisting": false
            },
            {
                "problem": "dmem_cgroup_try_charge() has a speculative lock-order concern.",
                "severity": "Medium",
                "severity_explanation": "The evidence is less direct than the retained dmem seed findings.",
                "preexisting": false
            }
        ]);

        cap_minimal_fallback_findings(&concerns, &mut findings, MINIMAL_FALLBACK_MAX_FINDINGS);

        let findings_array = findings.as_array().unwrap();
        assert_eq!(findings_array.len(), 3);
        let kept = serde_json::to_string(&findings).unwrap();
        assert!(kept.contains("limit.max"));
        assert!(kept.contains("region_unregister"));
    }

    #[test]
    fn test_stage9_repair_converts_proof_required_seed_drop_to_finding() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "source": "static_bug_pattern_seed",
                "pattern": "rcu_teardown_iteration_without_read_lock",
                "preservation": "proof_required_drop",
                "type": "RCU list iteration in unregister/teardown path",
                "description": "region_unregister() uses list_for_each_rcu in unregister teardown without visible rcu_read_lock.",
                "reasoning": "region_unregister() is an unregister path using list_for_each_rcu without rcu_read_lock or update-side lockdep proof, which can trigger a suspicious RCU warning or race.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "unclear",
                "rationale": "This looks speculative."
            }
        ]);

        let (repaired_findings, repaired_drops) =
            repair_stage9_accounting(&concerns, &findings, &dropped);
        validate_stage9_accounting(&concerns, &repaired_findings, &repaired_drops).unwrap();
        assert_eq!(repaired_findings.as_array().unwrap().len(), 1);
        assert_eq!(repaired_drops.as_array().unwrap().len(), 0);
        let text = finding_review_text(&repaired_findings[0]);
        assert!(text.contains("region_unregister"));
        assert!(text.contains("list_for_each_rcu"));
        assert!(text.contains("rcu_read_lock"));
    }

    #[test]
    fn test_stage9_allows_lifecycle_drop_with_concrete_barrier_proof() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Lifecycle Ordering",
                "description": "The command workqueue is destroyed before rfkill unregister can stop callbacks",
                "reasoning": "rfkill can call nci_close_device through the close callback after destroy_workqueue freed the command workqueue.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "rfkill_unregister() runs before destroy_workqueue() in the teardown order, and rfkill_unregister is the callback barrier for the rfkill close callback path into nci_close_device. It waits until the callback is unregistered, so nci_close_device cannot run after the workqueue is destroyed or observe freed state."
            }
        ]);

        validate_stage9_accounting(&concerns, &findings, &dropped).unwrap();
    }

    #[test]
    fn test_stage9_rejects_non_problem_finding() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Locking correctness",
                "description": "The patch correctly removes redundant local_irq_save/local_irq_restore calls.",
                "reasoning": "The callee is irq-safe.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([
            {
                "finding_id": "finding-1",
                "source_concern_id": "stage8-001",
                "problem": "The patch correctly removes redundant local_irq_save/restore calls from fprop_new_period because percpu_counter functions are now irq-safe.",
                "severity": "Low",
                "severity_explanation": "This improves performance by removing unnecessary locking.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([]);

        let err = validate_stage9_accounting(&concerns, &findings, &dropped).unwrap_err();
        assert!(err.contains("safe/correct behavior"));
    }

    #[test]
    fn test_stage9_repair_converts_argument_order_drop_to_finding() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "API Argument Order",
                "description": "Incorrect argument order in call to release_sgls: sg_list and sge are swapped",
                "reasoning": "The callee signature expects sge before sg_list, but the call site passes sg_list before sge.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "duplicate",
                "rationale": "Merged into a generic API contract concern."
            }
        ]);

        let (repaired_findings, repaired_drops) =
            repair_stage9_accounting(&concerns, &findings, &dropped);
        validate_stage9_accounting(&concerns, &repaired_findings, &repaired_drops).unwrap();
        assert_eq!(repaired_findings.as_array().unwrap().len(), 1);
        assert_eq!(repaired_drops.as_array().unwrap().len(), 0);
        let problem = repaired_findings[0]
            .get("problem")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(problem.contains("release_sgls"));
    }

    #[test]
    fn test_stage9_repair_converts_seqcount_irq_drop_to_finding() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Deadlock Prevention Regression",
                "description": "Removal of local_irq_save/restore in fprop_new_period may reintroduce a deadlock",
                "reasoning": "The patch removes local_irq_save around write_seqcount_begin/write_seqcount_end. A hardirq reader can retry on the same sequence counter while the writer is interrupted.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "percpu_counter functions are irq-safe due to raw_spin_lock_irqsave, so the outer local_irq_save is redundant."
            }
        ]);

        let (repaired_findings, repaired_drops) =
            repair_stage9_accounting(&concerns, &findings, &dropped);
        validate_stage9_accounting(&concerns, &repaired_findings, &repaired_drops).unwrap();
        assert_eq!(repaired_findings.as_array().unwrap().len(), 1);
        assert_eq!(repaired_drops.as_array().unwrap().len(), 0);
        let text = finding_review_text(&repaired_findings[0]);
        assert!(text.contains("write_seqcount_begin"));
        assert!(text.contains("hardirq"));
    }

    #[test]
    fn test_stage9_repair_converts_resource_cleanup_drop_to_finding() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Memory Leak",
                "description": "stripe_uptodate_bitmap allocated by bitmap_zalloc in alloc_rbio may leak on the ERR_PTR(-ENOMEM) path",
                "reasoning": "The error path calls free_raid_bio_pointers(rbio), but that helper does not show bitmap_free(rbio->stripe_uptodate_bitmap).",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "free_raid_bio_pointers() frees all rbio resources including stripe_uptodate_bitmap."
            }
        ]);

        let (repaired_findings, repaired_drops) =
            repair_stage9_accounting(&concerns, &findings, &dropped);
        validate_stage9_accounting(&concerns, &repaired_findings, &repaired_drops).unwrap();
        assert_eq!(repaired_findings.as_array().unwrap().len(), 1);
        assert_eq!(repaired_drops.as_array().unwrap().len(), 0);
        let text = finding_review_text(&repaired_findings[0]);
        assert!(text.contains("stripe_uptodate_bitmap"));
        assert!(text.contains("error"));
    }

    #[test]
    fn test_stage9_repair_converts_lifecycle_drop_to_finding() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Lifecycle Ordering",
                "description": "The command workqueue is destroyed before rfkill unregister can stop callbacks",
                "reasoning": "rfkill can call nci_close_device through the close callback after destroy_workqueue freed the command workqueue, so teardown can race into freed state.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "decision": "drop",
                "drop_reason": "false_positive",
                "rationale": "The code properly handles cleanup order."
            }
        ]);

        let (repaired_findings, repaired_drops) =
            repair_stage9_accounting(&concerns, &findings, &dropped);
        validate_stage9_accounting(&concerns, &repaired_findings, &repaired_drops).unwrap();
        assert_eq!(repaired_findings.as_array().unwrap().len(), 1);
        assert_eq!(repaired_drops.as_array().unwrap().len(), 0);
        let text = finding_review_text(&repaired_findings[0]);
        assert!(text.contains("nci_close_device"));
        assert!(text.contains("use-after-free"));
    }

    #[test]
    fn test_stage9_repair_cap_limits_non_special_finding_explosion() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Logic",
                "description": "Potential NULL pointer dereference",
                "reasoning": "A helper can return NULL.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-002",
                "type": "Logic",
                "description": "Potential RCU misuse",
                "reasoning": "An RCU list traversal may run without a read lock.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-003",
                "type": "Logic",
                "description": "Speculative state mismatch",
                "reasoning": "The model was unsure.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([
            {
                "finding_id": "finding-1",
                "source_concern_id": "stage8-001",
                "problem": "Potential NULL pointer dereference.",
                "severity": "High",
                "severity_explanation": "A helper can return NULL and the caller dereferences it.",
                "preexisting": false
            },
            {
                "finding_id": "finding-2",
                "source_concern_id": "stage8-002",
                "problem": "RCU traversal may happen without a read lock.",
                "severity": "High",
                "severity_explanation": "The list traversal can race with unregister.",
                "preexisting": false
            },
            {
                "finding_id": "finding-3",
                "source_concern_id": "stage8-003",
                "problem": "Speculative state mismatch.",
                "severity": "Low",
                "severity_explanation": "The evidence is unclear.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([]);

        let (capped_findings, capped_drops) =
            cap_repaired_stage9_findings(&concerns, findings, dropped, 2);
        validate_stage9_accounting(&concerns, &capped_findings, &capped_drops).unwrap();
        assert_eq!(capped_findings.as_array().unwrap().len(), 2);
        assert_eq!(capped_drops.as_array().unwrap().len(), 1);
        let kept = serde_json::to_string(&capped_findings).unwrap();
        assert!(kept.contains("NULL pointer"));
        assert!(kept.contains("RCU"));
    }

    #[test]
    fn test_stage9_allows_argument_order_subsumed_by_detailed_finding() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "API Argument Order",
                "description": "Incorrect argument order in call to release_sgls: sg_list and sge are swapped",
                "reasoning": "The callee signature expects sge before sg_list, but the call site passes sg_list before sge.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-002",
                "type": "API Argument Order",
                "description": "Duplicate concern for release_sgls argument order",
                "reasoning": "The callee signature expects sge before sg_list, but the actual call site passes sg_list before sge, so the order is wrong.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([
            {
                "finding_id": "finding-1",
                "source_concern_id": "stage8-001",
                "problem": "release_sgls is called with the wrong argument order.",
                "severity": "High",
                "severity_explanation": "The callee signature expects (req, sge, sg_list), but the actual call site passes (req, sg_list, sge). That swaps the SGL descriptor and list pointer, so DMA unmapping uses the wrong object.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([
            {
                "source_concern_id": "stage8-002",
                "decision": "drop",
                "drop_reason": "subsumed_by",
                "subsumed_by_finding_id": "finding-1",
                "rationale": "finding-1 preserves the same release_sgls argument-order issue."
            }
        ]);

        validate_stage9_accounting(&concerns, &findings, &dropped).unwrap();
    }

    #[test]
    fn test_stage9_compacts_argument_order_related_findings() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "API Argument Order",
                "description": "Incorrect argument order in call to release_sgls(req, sge, sg_list)",
                "reasoning": "The callee signature expects (req, sge, sg_list), but the actual call site passes (req, sg_list, sge), so the order is wrong.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-002",
                "type": "API Contract",
                "description": "release_sgls may dereference the wrong SGL pointer",
                "reasoning": "The same release_sgls call passes the SGL descriptor and SGL list in the wrong order.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([
            {
                "finding_id": "finding-1",
                "source_concern_id": "stage8-001",
                "problem": "release_sgls is called with the wrong argument order.",
                "severity": "High",
                "severity_explanation": "The callee signature expects (req, sge, sg_list), but the actual call site passes (req, sg_list, sge). That swaps the SGL descriptor and list pointer, so DMA unmapping uses the wrong object.",
                "preexisting": false
            },
            {
                "finding_id": "finding-2",
                "source_concern_id": "stage8-002",
                "problem": "release_sgls may dereference the wrong SGL pointer.",
                "severity": "High",
                "severity_explanation": "The release_sgls helper receives mismatched SGL arguments.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([]);

        let (compacted_findings, compacted_drops) =
            compact_argument_order_related_findings(&concerns, &findings, &dropped);

        validate_stage9_accounting(&concerns, &compacted_findings, &compacted_drops).unwrap();
        assert_eq!(compacted_findings.as_array().unwrap().len(), 1);
        assert_eq!(compacted_drops.as_array().unwrap().len(), 1);
        assert_eq!(
            compacted_drops[0]
                .get("drop_reason")
                .and_then(|value| value.as_str()),
            Some("subsumed_by")
        );
        assert_eq!(
            compacted_drops[0]
                .get("subsumed_by_finding_id")
                .and_then(|value| value.as_str()),
            Some("finding-1")
        );
    }

    #[test]
    fn test_stage9_compacts_lifecycle_root_cause_findings() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Lifecycle Ordering",
                "description": "rfkill unregister occurs after command workqueue destruction",
                "reasoning": "nci_close_device can re-enter the close path from the rfkill callback after destroy_workqueue has freed the command workqueue, causing use-after-free.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-002",
                "type": "Lifecycle Ordering",
                "description": "nci_close_device callback can use the destroyed workqueue during unregister",
                "reasoning": "The rfkill callback source remains registered while teardown has already destroyed the workqueue, so callback re-entry races with freed workqueue state.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([
            {
                "finding_id": "finding-1",
                "source_concern_id": "stage8-001",
                "problem": "rfkill unregister runs after workqueue destruction.",
                "severity": "High",
                "severity_explanation": "The rfkill callback source remains registered after the command workqueue is destroyed. nci_close_device can re-enter the close path after destroy_workqueue, use the destroyed workqueue/freed state, and race with teardown.",
                "preexisting": false
            },
            {
                "finding_id": "finding-2",
                "source_concern_id": "stage8-002",
                "problem": "nci_close_device can use a destroyed workqueue.",
                "severity": "High",
                "severity_explanation": "The callback path can run after workqueue destruction and race with unregister, causing use-after-free.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([]);

        let (compacted_findings, compacted_drops) =
            compact_stage9_related_findings(&concerns, &findings, &dropped);

        validate_stage9_accounting(&concerns, &compacted_findings, &compacted_drops).unwrap();
        assert_eq!(compacted_findings.as_array().unwrap().len(), 1);
        assert_eq!(compacted_drops.as_array().unwrap().len(), 1);
        assert_eq!(
            compacted_drops[0]
                .get("drop_reason")
                .and_then(|value| value.as_str()),
            Some("subsumed_by")
        );
    }

    #[test]
    fn test_stage9_compacts_response_buffer_cleanup_findings() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "type": "Resource Cleanup",
                "description": "SendReceive can leak the response buffer on the cifs_send_recv failure path",
                "reasoning": "The response buffer in resp_iov.iov_base is allocated before cifs_send_recv returns an error, and the error return can miss free_response_buf.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-002",
                "type": "Resource Cleanup",
                "description": "The SendReceive error path may return without freeing resp_iov.iov_base",
                "reasoning": "The same response buffer cleanup path relies on free_response_buf but can exit early after the failing cifs_send_recv call.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([
            {
                "finding_id": "finding-1",
                "source_concern_id": "stage8-001",
                "problem": "SendReceive can leak the response buffer after cifs_send_recv fails.",
                "severity": "High",
                "severity_explanation": "The response buffer stored in resp_iov.iov_base is allocated before cifs_send_recv returns an error, and the failing path can return without the expected free_response_buf cleanup.",
                "preexisting": false
            },
            {
                "finding_id": "finding-2",
                "source_concern_id": "stage8-002",
                "problem": "SendReceive error cleanup may miss free_response_buf for resp_iov.iov_base.",
                "severity": "High",
                "severity_explanation": "The same response buffer can leak when the cifs_send_recv error path exits before free_response_buf releases resp_iov.iov_base.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([]);

        let (compacted_findings, compacted_drops) =
            compact_stage9_related_findings(&concerns, &findings, &dropped);

        validate_stage9_accounting(&concerns, &compacted_findings, &compacted_drops).unwrap();
        assert_eq!(compacted_findings.as_array().unwrap().len(), 1);
        assert_eq!(compacted_drops.as_array().unwrap().len(), 1);
        assert_eq!(
            compacted_drops[0]
                .get("subsumed_by_finding_id")
                .and_then(|value| value.as_str()),
            Some("finding-1")
        );
    }

    #[test]
    fn test_stage9_compacts_dmem_max_missing_value_findings() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "source": "static_bug_pattern_seed",
                "pattern": "cgroup_keyed_parse_missing_value",
                "preservation": "proof_required_drop",
                "type": "Cgroup keyed file parsing / missing value",
                "description": "limit.max keyed write parsing may dereference or parse a missing value pointer.",
                "reasoning": "kernel/cgroup/limit.c limit_region_max_write()/limit_key_write() handles limit.max writes where a bare max key can reach strcmp/memparse before a non-NULL value pointer is proven.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-002",
                "type": "Cgroup parser",
                "description": "limit_key_write can parse a bare limit.max key without an accompanying value.",
                "reasoning": "The same limit.max write path can receive an absent value and reach parsing before proving the value pointer exists.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([
            {
                "finding_id": "finding-1",
                "source_concern_id": "stage8-001",
                "problem": "limit.max accepts a keyed write without proving the value is present.",
                "severity": "High",
                "severity_explanation": "kernel/cgroup/limit.c limit_region_max_write()/limit_key_write() handles limit.max. A bare max key or missing value can leave the value pointer absent before strcmp(), memparse(), or other parsing, causing NULL dereference or invalid parse from the cgroup write path.",
                "preexisting": false
            },
            {
                "finding_id": "finding-2",
                "source_concern_id": "stage8-002",
                "problem": "limit_key_write may parse an absent limit.max value.",
                "severity": "High",
                "severity_explanation": "The limit.max parser can see a missing value and attempt to parse it.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([]);

        let (compacted_findings, compacted_drops) =
            compact_stage9_related_findings(&concerns, &findings, &dropped);

        validate_stage9_accounting(&concerns, &compacted_findings, &compacted_drops).unwrap();
        assert_eq!(compacted_findings.as_array().unwrap().len(), 1);
        assert_eq!(compacted_drops.as_array().unwrap().len(), 1);
        assert!(finding_preserves_seed_pattern_detail(
            &concerns[0],
            &compacted_findings[0]
        ));
        assert_eq!(
            compacted_drops[0]
                .get("drop_reason")
                .and_then(|value| value.as_str()),
            Some("subsumed_by")
        );
    }

    #[test]
    fn test_stage9_compacts_dmem_unregister_rcu_findings() {
        let concerns = serde_json::json!([
            {
                "source_concern_id": "stage8-001",
                "source": "static_bug_pattern_seed",
                "pattern": "rcu_teardown_iteration_without_read_lock",
                "preservation": "proof_required_drop",
                "type": "RCU list iteration in unregister/teardown path",
                "description": "region_unregister() uses list_for_each_rcu in unregister teardown without visible rcu_read_lock.",
                "reasoning": "region_unregister() is an unregister path using list_for_each_rcu without rcu_read_lock or update-side lockdep proof, which can trigger a suspicious RCU warning or race.",
                "preexisting": false
            },
            {
                "source_concern_id": "stage8-002",
                "type": "RCU teardown",
                "description": "region_unregister() traverses an RCU list during unregister without visible read-side protection.",
                "reasoning": "The same teardown path uses list_for_each_rcu and should prove rcu_read_lock or an equivalent update-side lock.",
                "preexisting": false
            }
        ]);
        let findings = serde_json::json!([
            {
                "finding_id": "finding-1",
                "source_concern_id": "stage8-001",
                "problem": "region_unregister() traverses an RCU list during unregister without read-side protection.",
                "severity": "High",
                "severity_explanation": "region_unregister() runs in unregister/teardown context and uses list_for_each_rcu. Stage 9 did not prove rcu_read_lock(), a documented update-side lock, or a lockdep condition covers the traversal, so teardown can trigger a suspicious RCU warning or race.",
                "preexisting": false
            },
            {
                "finding_id": "finding-2",
                "source_concern_id": "stage8-002",
                "problem": "dmem unregister path is missing RCU read-side protection.",
                "severity": "High",
                "severity_explanation": "The unregister path uses list_for_each_rcu without rcu_read_lock.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([]);

        let (compacted_findings, compacted_drops) =
            compact_stage9_related_findings(&concerns, &findings, &dropped);

        validate_stage9_accounting(&concerns, &compacted_findings, &compacted_drops).unwrap();
        assert_eq!(compacted_findings.as_array().unwrap().len(), 1);
        assert_eq!(compacted_drops.as_array().unwrap().len(), 1);
        assert!(finding_preserves_seed_pattern_detail(
            &concerns[0],
            &compacted_findings[0]
        ));
        assert_eq!(
            compacted_drops[0]
                .get("subsumed_by_finding_id")
                .and_then(|value| value.as_str()),
            Some("finding-1")
        );
    }

    #[test]
    fn test_stage8_restores_argument_order_when_merge_loses_details() {
        let input = vec![serde_json::json!({
            "type": "API Argument Order",
            "description": "Incorrect argument order in call to release_sgls",
            "reasoning": "The callee signature expects (req, sge, sg_list), but the actual call site passes (req, sg_list, sge), so the order is wrong.",
            "preexisting": false
        })];
        let mut retained = serde_json::json!([
            {
                "type": "API Contract Violation",
                "description": "Generic release_sgls API contract issue",
                "reasoning": "The function signature changed.",
                "preexisting": false
            }
        ]);
        let dropped = serde_json::json!([
            {
                "description": "Incorrect argument order in call to release_sgls",
                "reason": "Merged into retained concern: API Contract Violation"
            }
        ]);

        let restored = preserve_stage8_argument_order_concerns(&input, &mut retained, &dropped);

        assert_eq!(restored, 1);
        assert_eq!(retained.as_array().unwrap().len(), 2);
        assert!(
            retained
                .as_array()
                .unwrap()
                .iter()
                .any(is_argument_order_concern)
        );
    }

    #[test]
    fn test_exploration_policy_respects_configured_interaction_limit() {
        let policy = exploration_policy(3, 12);

        assert_eq!(policy.max_tool_calls, 18);
        assert_eq!(policy.max_interactions, 12);
    }

    #[test]
    fn test_per_turn_tool_call_cap_allows_more_for_execution_flow() {
        assert_eq!(per_turn_tool_call_cap(3), 6);
        assert_eq!(per_turn_tool_call_cap(1), 4);
        assert_eq!(per_turn_tool_call_cap(8), 4);
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
        let tools = crate::worker::tools::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            custom_prompt: None,
            stages: None,
            stage_protocol: StageProtocol::Native,
            enable_static_bug_seeds: false,
            enable_targeted_bug_pattern_prescan: false,
        };
        let mut worker = Worker::new(provider, tools, prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        match worker.run(patchset).await {
            Ok(_) => panic!("Expected stage failure error, got Ok"),
            Err(e) => assert!(
                e.to_string().contains("failed to produce valid"),
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
}
