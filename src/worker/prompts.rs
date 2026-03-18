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

use crate::ai::{AiMessage, AiProvider, AiRequest, AiResponseFormat, AiRole};
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

        content.push_str("You are an expert Linux kernel maintainer. Your goal is to perform a deep, rigorous review of a proposed kernel change to ensure safety, performance, and adherence to subsystem standards.\n\n");
        content.push_str("TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.\n\n");
        content.push_str("<global_review_guidelines>\n");
        content.push_str("The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.\n\n");
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

You are a hardware engineer reviewing device driver changes. If this patch touches driver or hardware-specific code, rigorously review register accesses, IRQ handling, DMA mapping/unmapping, memory barriers, and timing/delays. Look for missing dma_wmb()/dma_rmb() barriers, incorrect endianness conversions (cpu_to_le32), and unsafe DMA buffer allocations. Ensure the hardware state machine is handled correctly, especially during suspend/resume or device reset. Evaluate the physical state machine constraints: verify that clocks and power domains are enabled before registers are accessed, and that hardware rings/queues are actually initialized in the current hardware state before being unconditionally accessed. If the patch is purely generic software logic (e.g., VFS, core networking), output an empty concerns list."
            }
            8 => {
                "# Stage 8. Verification and severity estimation

You are the lead reviewer consolidating feedback from multiple specialized analysts. You will be given a list of concerns generated by different review stages.
1. Deduplicate identical or overlapping concerns.
2. Validate each concern and prove the provided reasoning. Report all valid concerns as findings. If necessarily, use tools to gather additional material. Discard all false positives
3. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning. If you cannot find definitive proof that the concern is a false positive, it must be reported as a finding. If you're not sure about something and it's critical in the reasoning validation, make it obvious: if X is possible, then problem Y can occur. Always try to validate if X is possible yourself.
4. If context from subsequent patches in the series is provided, check if the concern is fixed later in the series. If so, discard it. But don't trust any promises in the commit message if they can't be verified (e.g. something will be fixed by subsequent patches in the series - if you can't prove that it's indeed fixed, report it as a bug).
5. When referring to other patches within this series in your explanation, DO NOT use git hashes (they are ephemeral/unstable). Instead, refer to them by their patch subject (e.g., 'commit \"mm: fix allocation\"'). Existing historical commits in the tree should still be referenced by their standard hash.
6. Assign a severity (low, medium, high, critical) to each remaining valid finding and explain the reasoning. Be rigorous in filtering out verifiable noise, but accurately report real logic flaws and edge cases.
7. If the problem did exist in the code before the patch was applied, say it explicitly: 'This problem wasn't introduced by this patch, but...'. Discard low- and medium-severity pre-existing problems, report only high- and critical severity issues."
            }
            9 => {
                "# Stage 9. LKML-friendly report generation

You are an automated review bot generating a report for the Linux Kernel Mailing List (LKML). Convert the provided JSON findings into a polite, standard, inline-commented LKML email reply. Follow the formatting rules strictly. Do not use markdown headers or ALL CAPS shouting. Ensure the tone is constructive and professional. Do not use backticks to quote any names or expressions."
            }
            10 => {
                "# Stage 10. Fix generation

You are an expert kernel developer writing patches to fix bugs found during review. Generate git-formatted patches to address the provided findings. Ensure the code conforms to kernel style guidelines and compiles cleanly mentally. Double-check that your fixes do not introduce new regressions."
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
            8 => {
                self.append_file(&mut content, &mut clean_files, "false-positive-guide.md")
                    .await?;
                self.append_file(&mut content, &mut clean_files, "severity.md")
                    .await?;
            }
            9 => {
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
    tools: ToolBox,
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
        tools: ToolBox,
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

    pub async fn run(&mut self, patchset: Value) -> Result<WorkerResult> {
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
        let mut total_tokens_in = 0;
        let mut total_tokens_out = 0;
        let mut total_tokens_cached = 0;

        // Phase 0: Pre-screen relevant prompts
        let subsystem_md_path = self.prompts.base_dir.join("subsystem/subsystem.md");
        let selected_prompts = if subsystem_md_path.exists() {
            match tokio::fs::read_to_string(&subsystem_md_path).await {
                Ok(subsystem_md) => {
                    info!("Executing Phase 0: Pre-screening relevant subsystem guides.");
                    let phase0_system = "You are an AI assistant preparing a Linux kernel patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.";
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

                    match self.provider.generate_content(req).await {
                        Ok(resp) => {
                            if let Some(usage) = &resp.usage {
                                total_tokens_in += usage.prompt_tokens as u32;
                                total_tokens_out += usage.completion_tokens as u32;
                                total_tokens_cached += usage.cached_tokens.unwrap_or(0) as u32;
                            }
                            if let Some(content) = resp.content {
                                match serde_json::from_str::<Value>(&content) {
                                    Ok(val) => {
                                        if let Some(arr) =
                                            val.get("selected_prompts").and_then(|v| v.as_array())
                                        {
                                            let prompts: Vec<String> = arr
                                                .iter()
                                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                                .collect();
                                            info!("Phase 0 selected prompts: {:?}", prompts);
                                            Some(prompts)
                                        } else {
                                            warn!(
                                                "Phase 0 JSON did not contain 'selected_prompts' array"
                                            );
                                            None
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Phase 0 JSON parse error: {}", e);
                                        None
                                    }
                                }
                            } else {
                                warn!("Phase 0 returned no content");
                                None
                            }
                        }
                        Err(e) => {
                            warn!("Phase 0 completion failed: {}", e);
                            None
                        }
                    }
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

        let mut dynamic_context = String::new();
        dynamic_context.push_str("\n\nTarget Commit:\n");
        dynamic_context.push_str(&target_commit_diff);
        let mut clean_dynamic_context = dynamic_context.clone();

        let mut dynamic_context_no_log = String::new();
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

Return ONLY a JSON object containing an array of integers representing the relevant stages (e.g., [4, 5, 6, 7]).
CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo fix, you may omit some stages. Stages 1, 2, and 3 are always run and should not be included in your answer."#;

            let req = AiRequest {
                system: None,
                messages: vec![AiMessage {
                    role: crate::ai::AiRole::User,
                    content: Some(format!("{}\n\n{}", shared_context, planning_prompt)),
                    thought: None,
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
            match self.provider.generate_content(req).await {
                Ok(resp) => {
                    if let Some(usage) = &resp.usage {
                        total_tokens_in += usage.prompt_tokens as u32;
                        total_tokens_out += usage.completion_tokens as u32;
                        total_tokens_cached += usage.cached_tokens.unwrap_or(0) as u32;
                    }
                    if let Some(content) = resp.content {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                            if let Some(arr) = val.get("relevant_stages").and_then(|v| v.as_array())
                            {
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
                            } else {
                                warn!(
                                    "Planning phase JSON did not contain 'relevant_stages' array"
                                );
                            }
                        } else {
                            warn!("Planning phase JSON parse error");
                        }
                    } else {
                        warn!("Planning phase returned no content");
                    }
                }
                Err(e) => {
                    warn!("Planning phase failed: {}", e);
                }
            }
        }

        // Stages 1-7
        for stage in 1..=7 {
            if let Some(ref selected_stages) = self.stages {
                if !selected_stages.contains(&stage) {
                    continue;
                }
            } else if let Some(ref planned_stages) = planning_selected_stages
                && !planned_stages.contains(&stage)
            {
                info!("Skipping stage {} based on planning phase", stage);
                continue;
            }

            info!("Running Stage {}", stage);
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = if (3..=6).contains(&stage) {
                shared_context_no_log.clone()
            } else {
                shared_context.clone()
            };
            let clean_system_prompt = if (3..=6).contains(&stage) {
                clean_shared_context_no_log.clone()
            } else {
                clean_shared_context.clone()
            };

            let format_guidance = r#"Once you have gathered sufficient information, return ONLY a JSON object with a "concerns" array.
If you find no concerns, return `{"concerns": []}`.
If you find concerns, each must be an object with:
- "type": A short category string.
- "description": A clear description of the problem.
- "reasoning": A step-by-step explanation.

CRITICAL REVIEW DIRECTIVE: Do NOT dismiss concerns just because you assume the surrounding system or caller handles it perfectly. Do not be overly charitable to the existing code. If there is a missing initialization, an unhandled edge case, or a brittle logic flow, report it as a concern immediately. Assume the worst-case scenario where external inputs and caller states are malformed.

Example:
```json
{
  "concerns": [
    {
      "type": "Issue Category",
      "description": "What is wrong.",
      "reasoning": "Why it is wrong."
    }
  ]
}
```"#;
            let user_prompt = format!("{}\n\n{}", stage_prompt, format_guidance);
            let clean_user_prompt = format!("{}\n\n{}", clean_stage_prompt, format_guidance);

            let mut outer_attempts = 0;
            let max_outer_attempts = 3;
            let mut success = false;

            while outer_attempts < max_outer_attempts && !success {
                outer_attempts += 1;

                let mut inner_attempts = 0;
                let max_inner_attempts = 3;
                let mut active_user_prompt = user_prompt.clone();
                let mut active_clean_user_prompt = clean_user_prompt.clone();

                while inner_attempts < max_inner_attempts && !success {
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
                            total_tokens_in += t_in;
                            total_tokens_out += t_out;
                            total_tokens_cached += t_cached;

                            if let Some(concerns) =
                                result_json.get("concerns").and_then(|c| c.as_array())
                            {
                                for c in concerns {
                                    if c.is_object() {
                                        all_concerns.push(c.clone());
                                    } else if let Some(s) = c.as_str() {
                                        all_concerns.push(serde_json::json!({
                                            "type": "General",
                                            "description": s
                                        }));
                                    }
                                }
                                success = true;
                            } else {
                                let violation =
                                    "JSON output is missing the required 'concerns' array";
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
                                active_clean_user_prompt =
                                    format!("{}{}", clean_user_prompt, reminder);
                            }
                        }
                        Err(e) => {
                            // Fail fast for non-retryable errors — retrying would
                            // likely just hit the same limit again.
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

                if !success {
                    warn!(
                        "Stage {} outer attempt {}/{} failed to produce valid output.",
                        stage, outer_attempts, max_outer_attempts
                    );
                }
            }
            if !success {
                warn!(
                    "Stage {} failed after {} outer attempts.",
                    stage, max_outer_attempts
                );
                return Err(anyhow::anyhow!(
                    "Stage {} failed to produce valid 'concerns' array after {} attempts — aborting review",
                    stage,
                    max_outer_attempts
                ));
            }
        }

        if all_concerns.is_empty() {
            tracing::info!("No concerns from stages 1-7, skipping stages 8 and 9");
            let final_output = serde_json::json!({
                "findings": [],
                "review_inline": "No issues found.",
                "fixes": ""
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

        // Stage 8
        info!("Running Stage 8");
        let findings_json;
        {
            let stage = 8;
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();
            let clean_system_prompt = clean_shared_context.clone();

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

            let aggregated_concerns_json =
                serde_json::to_string_pretty(&all_concerns).unwrap_or_default();
            let user_prompt = format!(
                "{}\n\nCRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.\n\nFull Series Context:\n{}\n\nAggregated Concerns:\n{}\n\nReturn ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: \"problem\" (a string containing the vulnerability description), \"severity\" (a string: Low, Medium, High, or Critical), \"severity_explanation\" (a string detailing the reasoning and proof).\n\nExample Output:\n```json\n{{\n  \"findings\": [\n    {{\n      \"problem\": \"Memory leak in function X when condition Y is met.\",\n      \"severity\": \"High\",\n      \"severity_explanation\": \"1. Condition Y is met.\\\n2. The buffer is allocated but not freed before return.\"\n    }}\n  ]\n}}\n```",
                stage_prompt, full_series_context, aggregated_concerns_json
            );
            let clean_user_prompt = format!(
                "{}\n\nCRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.\n\nFull Series Context:\n{{{{series context}}}}\n\nAggregated Concerns:\n{}\n\nReturn ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: \"problem\" (a string containing the vulnerability description), \"severity\" (a string: Low, Medium, High, or Critical), \"severity_explanation\" (a string detailing the reasoning and proof).\n\nExample Output:\n```json\n{{\n  \"findings\": [\n    {{\n      \"problem\": \"Memory leak in function X when condition Y is met.\",\n      \"severity\": \"High\",\n      \"severity_explanation\": \"1. Condition Y is met.\\\n2. The buffer is allocated but not freed before return.\"\n    }}\n  ]\n}}\n```",
                clean_stage_prompt, aggregated_concerns_json
            );
            match self
                .run_ai_stage(
                    stage,
                    system_prompt,
                    clean_system_prompt,
                    user_prompt,
                    clean_user_prompt,
                )
                .await
            {
                Ok((result_json, t_in, t_out, t_cached)) => {
                    total_tokens_in += t_in;
                    total_tokens_out += t_out;
                    total_tokens_cached += t_cached;

                    if let Some(f) = result_json.get("findings") {
                        if f.is_array() {
                            findings_json = f.clone();
                        } else {
                            return Err(anyhow::anyhow!(
                                "Stage 8 output 'findings' is not an array"
                            ));
                        }
                    } else {
                        return Err(anyhow::anyhow!(
                            "Stage 8 failed to produce a valid 'findings' array in output."
                        ));
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Stage 8 AI execution failed: {}", e));
                }
            }
        }

        if let Some(f) = findings_json.as_array()
            && f.is_empty()
        {
            tracing::info!("No findings from Stage 8, skipping Stage 9");
            let final_output = serde_json::json!({
                "findings": findings_json,
                "review_inline": "No issues found.",
                "fixes": ""
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

        // Stage 9
        info!("Running Stage 9");
        let mut review_inline_text = String::new();
        {
            let stage = 9;
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();
            let clean_system_prompt = clean_shared_context.clone();
            let findings_str = serde_json::to_string_pretty(&findings_json).unwrap_or_default();
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
            // On format rejection we augment the prompt rather than repeating
            // it verbatim, so track the active prompt separately.
            let mut active_user_prompt = user_prompt.clone();
            let mut active_clean_user_prompt = clean_user_prompt.clone();
            let mut free_form_mode = false;
            while retries < max_retries {
                match self
                    .run_ai_stage_raw(
                        stage,
                        system_prompt.clone(),
                        clean_system_prompt.clone(),
                        active_user_prompt.clone(),
                        active_clean_user_prompt.clone(),
                    )
                    .await
                {
                    Ok((result_text, t_in, t_out, t_cached)) => {
                        total_tokens_in += t_in;
                        total_tokens_out += t_out;
                        total_tokens_cached += t_cached;
                        if free_form_mode {
                            review_inline_text = result_text;
                            break;
                        } else {
                            match validate_inline_format(&result_text) {
                                Ok(_) => {
                                    review_inline_text = result_text;
                                    break;
                                }
                                Err(violation) => {
                                    tracing::warn!(
                                        "Stage 9 format validation failed (attempt {}/{}): {}. Retrying with augmented prompt.",
                                        retries + 1,
                                        max_retries,
                                        violation
                                    );
                                    let reminder = format!(
                                        "\n\nPrevious attempt was rejected: {violation}. Strictly follow the formatting rules."
                                    );
                                    active_user_prompt = format!("{}{}", user_prompt, reminder);
                                    active_clean_user_prompt =
                                        format!("{}{}", clean_user_prompt, reminder);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        tracing::warn!(
                            "Stage 9 failed (attempt {}/{}): {}",
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
                            // Optionally don't penalize the retry count for the first recitation error
                            if retries + 1 == max_retries {
                                retries -= 1;
                            }
                        }
                    }
                }
                retries += 1;
            }

            if review_inline_text.is_empty() {
                return Err(anyhow::anyhow!(
                    "Stage 9 failed to generate a valid LKML report after {} attempts.",
                    max_retries
                ));
            }
        }

        let fixes_text = String::new();
        /*         // Stage 10
        info!("Running Stage 10");

        {
            let stage = 10;
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();
            let clean_system_prompt = clean_shared_context.clone();
            let findings_str = serde_json::to_string_pretty(&findings_json).unwrap_or_default();
            let user_prompt = format!(
                "{}\n\nFindings:\n{}\n\nReturn raw text containing git-formatted patches.",
                stage_prompt, findings_str
            );
            let clean_user_prompt = format!(
                "{}\n\nFindings:\n{}\n\nReturn raw text containing git-formatted patches.",
                clean_stage_prompt, findings_str
            );
            if let Ok((result_text, t_in, t_out, t_cached)) = self
                .run_ai_stage_raw(stage, system_prompt, clean_system_prompt, user_prompt, clean_user_prompt)
                .await
            {
                total_tokens_in += t_in;
                total_tokens_out += t_out;
                total_tokens_cached += t_cached;
                fixes_text = result_text;
            }
        } */

        let final_output = json!({
            "findings": findings_json,
            "review_inline": review_inline_text,
            "fixes": fixes_text
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
        let cleaned = crate::utils::clean_json_string(&raw_text);
        let parsed: Value = serde_json::from_str(&cleaned).unwrap_or_else(|_| {
            let cands = find_json_candidates(&raw_text);
            cands.into_iter().last().unwrap_or(json!({}))
        });
        Ok((parsed, t_in, t_out, t_cached))
    }

    async fn run_ai_stage_raw(
        &mut self,
        _stage: u8,
        system_prompt: String,
        clean_system_prompt: String,
        user_prompt: String,
        clean_user_prompt: String,
    ) -> Result<(String, u32, u32, u32)> {
        let mut local_history = Vec::new();

        let user_msg = AiMessage {
            role: AiRole::User,
            content: Some(user_prompt.clone()),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        };
        local_history.push(user_msg.clone());

        if self.global_history.is_empty() {
            // Keep a clean version for testing/history, we can just push the user prompt.
            // But we don't have a clean sys_msg anymore as an AiMessage.
            // Let's create an informational System message in global history just to record the context.
            self.global_history.push(AiMessage {
                role: AiRole::System,
                content: Some(clean_system_prompt.clone()),
                thought: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }
        self.global_history.push(AiMessage {
            role: AiRole::User,
            content: Some(clean_user_prompt),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        });

        let mut turns = 0;
        let mut t_in = 0;
        let mut t_out = 0;
        let mut t_cached = 0;

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
                    .map(|prefix| format!("{} s:{}] ", &prefix[..prefix.len() - 2], _stage)),
            };

            let resp = self.provider.generate_content(request).await?;

            if let Some(usage) = &resp.usage {
                t_in += usage.prompt_tokens as u32;
                t_out += usage.completion_tokens as u32;
                t_cached += usage.cached_tokens.unwrap_or(0) as u32;
            }

            let assistant_msg = AiMessage {
                role: AiRole::Assistant,
                content: resp.content.clone(),
                thought: resp.thought.clone(),
                tool_calls: resp.tool_calls.clone(),
                tool_call_id: None,
            };
            local_history.push(assistant_msg.clone());
            self.global_history.push(assistant_msg);

            if let Some(tool_calls) = resp.tool_calls {
                let mut tool_responses = Vec::new();
                for call in tool_calls {
                    let result = match self
                        .tools
                        .call(&call.function_name, call.arguments.clone())
                        .await
                    {
                        Ok(v) => v.to_string(),
                        Err(e) => json!({"error": e.to_string()}).to_string(),
                    };
                    tool_responses.push(AiMessage {
                        role: AiRole::Tool,
                        content: Some(result),
                        thought: None,
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
