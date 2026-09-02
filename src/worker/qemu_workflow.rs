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

//! Declarative review workflow definition for QEMU virtualization and device emulation.
//!
//! This module specifies the multi-stage review pipeline as a declarative [`Workflow`]
//! operating over [`ReviewState`].

use std::path::PathBuf;

use serde_json::json;

use crate::worker::kernel_workflow::{
    Phase0Output, PlanningOutput, ReviewState, STAGE_8_INSTRUCTION, STAGE_9_INSTRUCTION,
    STAGE_JSON_SCHEMA_EXAMPLE, Stage9Output, Stage10Output, StageConcernsOutput,
    append_stage_dismissed_concerns, append_stage_items, format_inline_feedback,
    format_stages_1_to_8_feedback, stage_uses_commit_log, validate_inline_format,
    validate_stages_1_to_8,
};
use crate::workflow::graph::Workflow;
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{ParallelPolicy, RecitationPolicy, StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};

/// Subsystem guides that are loaded per-stage and should be excluded
/// from Phase 0 shared context to avoid redundant token usage.
pub const QEMU_STAGE_EXCLUSIVE_GUIDES: &[&str] = &["concurrency.md", "qom.md"];

/// QEMU system prompt template.
pub fn qemu_system_prompt(use_log: bool) -> PromptTemplate<ReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<ReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are an expert QEMU maintainer and virtualization security researcher. Your goal is to perform a deep, rigorous review of a proposed QEMU change to ensure hypervisor safety, virtual hardware correctness, memory virtualization integrity, and adherence to QEMU subsystem standards.

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.
@includes
</global_review_guidelines>

=== Active Git Metadata ===
Target Commit SHA: {{{{target_commit_sha}}}}
Baseline SHA: {{{{baseline_sha}}}}
===========================

Target Commit:
{diff_var}
{{{{prefetched_block}}}}{{{{custom_prompt_block}}}}"#
    ))
    .with_var("target_commit_sha", |s: &ReviewState| s.target_commit_sha.clone())
    .with_var("baseline_sha", |s: &ReviewState| s.baseline_sha.clone())
    .with_var("target_commit_diff", |s: &ReviewState| s.target_commit_diff.clone())
    .with_var("target_commit_diff_only", |s: &ReviewState| s.target_commit_diff_only.clone())
    .with_var("prefetched_block", |s: &ReviewState| {
        if s.prefetched_context.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\nIf it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n{}\n</pre_fetched_context>",
                s.prefetched_context
            )
        }
    })
    .with_var("custom_prompt_block", |s: &ReviewState| {
        s.custom_prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()).map_or_else(String::new, |p| {
            format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
        })
    })
    .include_files_from_state(|s: &ReviewState| {
        let mut paths = Vec::new();
        if !s.selected_guides.is_empty() {
            for guide in &s.selected_guides {
                let p = std::path::Path::new(guide);
                let name = p.file_name().unwrap_or(p.as_os_str());
                paths.push(PathBuf::from("subsystem").join(name));
            }
        }
        paths
    })
}

// ---------------------------------------------------------------------------
// Stage Instructions
// ---------------------------------------------------------------------------

const STAGE_1_INSTRUCTION: &str = r#"# Stage 1. Analyze commit main goal

You are a senior QEMU maintainer evaluating the high-level virtualization and device architecture intent of a proposed commit. Analyze the commit message and the conceptual change. Focus on the big picture: Are there architectural flaws, QOM hierarchy anti-patterns, QAPI schema breakages, command-line/machine compatibility issues, or migration protocol regressions? Consider the long-term maintainability and system-wide hypervisor implications of this design. If the core idea is dangerous, incorrect, or violates established QEMU design principles, raise a concern. Be open-minded but thorough; question assumptions made by the author and consider alternative, cleaner designs."#;

const STAGE_2_INSTRUCTION: &str = r#"# Stage 2. High-level implementation verification

You are verifying if the provided code changes actually implement what the commit message claims. Look for undocumented side-effects, missing pieces (e.g., adding a new device property without updating VMState, or changing a device model without wiring reset or unrealize handlers), and unhandled corner cases related to the feature's logic. Explicitly check for missing callbacks in MemoryRegionOps (read/write), DeviceClass (realize, unrealize, reset), and VMStateDescription. Verify that all claims in the commit message are fully realized in the code. Identify any incomplete implementations, implicit behavioral changes, or API contract violations. Verify that bounds, arithmetic, and endianness logic are semantically sound. Check for off-by-one errors in memory region sizes, incorrect bitwise operations on hardware registers, and verify that all arguments passed to QEMU core APIs (like qemu_find_file, memory_region_init_io) are valid and semantically correct. Don't trust the commit message without verifying each claim. Assume that the message might be incorrect or even intentionally malicious."#;

const STAGE_3_INSTRUCTION: &str = r#"# Stage 3. Execution flow and error handling verification

You are a static analysis engine tracing execution flow in QEMU C code. Carefully trace the control flow of the provided patch. Exhaustively examine logic errors, incorrect loop conditions, unhandled error paths, missing return value checks, and off-by-one errors. Check every branch, switch statement, and conditional. Specifically look for NULL pointer dereferences and unchecked pointers. Explore every error handling path (goto out; / goto err;) to ensure it unwinds correctly under failure conditions. Scrutinize QEMU error handling rules: verify proper use of Error **errp, ensure ERRP_GUARD() is used when dereferencing or checking *errp, and ensure error_setg() is not called on an already set error. Verify that static/inline declarations or symbol linkages won't cause build or linking issues."#;

const STAGE_4_INSTRUCTION: &str = r#"# Stage 4. QOM lifecycle and resource management

You are an expert in QEMU Object Model (QOM) and resource management. Analyze the patch for memory leaks, Use-After-Free (UAF), double frees, uninitialized variables, and unbalanced lifecycle operations. Pay special attention to:
1. QOM reference counting: Ensure object_ref() and object_unref() are strictly balanced. Verify that parent-child relationships (object_property_add_child) do not create reference cycles or dangling pointers. Verify that TypeInfo.class_size is specified when subclassing or adding class callbacks to prevent heap corruption.
2. Allocation safety: Audit g_malloc vs g_try_malloc. Ensure guest-controlled or dynamic sizes use g_try_malloc / g_try_new to prevent guest-triggerable host aborts/crashes.
3. Device realize / unrealize symmetry: If resources (memory regions, timers, BHs, IRQs, backends) are allocated or initialized in instance_init or realize, verify they are cleanly freed or torn down in unrealize or instance_finalize.
4. Timers and Bottom Halves (BH): Verify that QEMUTimer and QEMUBH objects are deleted or canceled before their holding structs are freed."#;

const STAGE_5_INSTRUCTION: &str = r#"# Stage 5. Concurrency, BQL, and coroutines

You are a world-class concurrency and virtualization expert auditing a QEMU patch.
Carefully review the proposed patch for ANY concurrency, locking, or synchronization bugs:
1. Big QEMU Lock (BQL / qemu_mutex_lock_iothread): Are operations that modify global virtual machine state executed under BQL? Are thread-unsafe helpers called from I/O threads without holding BQL?
2. AioContext and IOThreads: Are device callbacks safely executing in the correct AioContext? Are aio_context_acquire / aio_context_release rules respected?
3. Coroutine safety: Are coroutine-only functions (coroutine_fn) safely called within coroutine context? Does yielding across a coroutine (qemu_coroutine_yield, aio_co_wake) introduce TOCTOU races or use-after-free on stack variables or locks?
4. Reentrancy and bottom halves: Does MMIO or DMA callback dispatch re-enter device emulation code while internal state is mid-update? Are bottom halves scheduled with appropriate reentrancy guards?
5. RCU and lockless access: Are RCU-protected pointers dereferenced via qatomic_rcu_read inside rcu_read_lock()? Are lockless accesses correctly using qatomic_* primitives?"#;

const STAGE_6_INSTRUCTION: &str = r#"# Stage 6. Security audit and guest attack surface

You are a Red Team virtualization security researcher auditing a QEMU patch for hypervisor breakout vulnerabilities.
Scrutinize all guest-to-host attack surfaces:
1. Guest MMIO / PIO bounds: Are guest read/write offsets, register indices, and transfer lengths strictly validated against device memory region boundaries and FIFO/buffer capacities?
2. DMA buffer overflows: Does the device perform DMA reads/writes using guest-supplied physical addresses or lengths without validating against maximum transfer sizes or address limits?
3. Packet / request bounds: Are network frame sizes, SCSI/NVMe command blocks, and virtio descriptor lengths checked against maximum buffer sizes (e.g., BUFSZ_MAX, MTU, queue capacity)?
4. Integer overflows: Are buffer lengths or offset calculations subject to integer overflow, sign extension, or truncation before allocation or indexing?
5. Information disclosure: Does the device leak uninitialized host heap/stack data back to the guest through MMIO reads or DMA buffers?
6. Migration deserialization: Does VMState loading (VMSTATE_*, post_load) thoroughly validate all deserialized guest state fields to prevent corrupt or attacker-controlled values from inducing host memory corruption?"#;

const STAGE_7_INSTRUCTION: &str = r#"# Stage 7. Virtual hardware and device model review

You are a virtual hardware and device emulation engineer reviewing QEMU device driver changes. Rigorously review:
1. Hardware register write masks (wmask / rmask / w1c): Are read-only bits preserved? Are write-1-to-clear bits cleared correctly without overwriting other bits?
2. Device reset behavior: Does DeviceReset or the Resettable interface restore all registers, FIFOs, and state machines to power-on or warm-reset state according to the hardware specification?
3. Interrupt line semantics: Are IRQ lines (qemu_irq_raise, qemu_irq_lower) raised and lowered with proper symmetry? Is interrupt status updated atomically with interrupt signal assertion?
4. Endianness conversion: Are register and descriptor accesses converted correctly between host and guest endianness (le16_to_cpu, le32_to_cpu, cpu_to_le32)?
5. State machine integrity: Does the device model handle out-of-order guest commands, unexpected reset, or aborted transactions gracefully without getting stuck in an invalid state?
If the patch is purely generic software logic without device emulation or virtual hardware, return {"concerns": [], "dismissed_concerns": []}."#;

const STAGE_10_INSTRUCTION: &str = r#"# Stage 10. Verification and severity estimation

You are the lead reviewer validating consolidated concerns. You will be given a list of deduplicated concerns after conflict resolution.
1. Validate each concern and prove the provided reasoning against QEMU virtualization semantics. Report all valid concerns as findings. If necessary, use tools to gather additional material. Discard all false positives.
2. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning. If you cannot find definitive proof that the concern is a false positive, it must be reported as a finding.
3. SERIES VALIDATION RULE: If follow-up patches in this series are provided in the context, check if each identified concern is resolved or fixed in the final state of the series. If the problem has been resolved, fixed, or the code was rewritten in a subsequent patch in this series, you MUST discard the concern and NOT report it as a finding.
4. When referring to other patches within this series in your explanation, DO NOT use git hashes. Instead, refer to them by their patch subject.
5. Assign a severity (low, medium, high, critical) following QEMU severity guidelines: reason through consequence (guest breakout, host crash, data loss, resource leak), triggering path, and guest reachability.
6. If the problem did exist in the code before the patch was applied, say it explicitly: 'This problem wasn't introduced by this patch, but...'. Discard low- and medium-severity pre-existing problems, report only high- and critical severity issues.
7. SPECIFICITY REQUIREMENT: Every finding MUST cite the exact function name(s), file path(s), line number(s) when known, and triggering conditions where the bug manifests.
8. Carry forward the locations from the validated concern into each finding."#;

const STAGE_11_INSTRUCTION: &str = r#"# Stage 11. Plain-text inline review report generation

You are an automated review bot generating a report for the QEMU mailing list (qemu-devel). Convert the provided JSON findings into a polite, standard, inline-commented review email reply.

CRITICAL FORMAT REQUIREMENTS:
1. The report MUST be plain text only. Do NOT use markdown code blocks (no ```).
2. The output MUST start with the commit header lines within the first few lines:
commit {{target_commit_sha}}
Author: <author_name_and_email>

<commit_subject>
3. Quote relevant portions of the diff or code using '>' prefix lines.
4. Place your review findings inline under the quoted code lines where the issue occurs.
5. If a finding is flagged as pre-existing ("preexisting": true), you MUST explicitly state in your inline comment that this issue is pre-existing and was not introduced by the patch under review. Use phrasing like "This isn't a bug introduced by this patch, but..." or "This is a pre-existing issue, but..." to start the comment.
6. Follow formatting rules strictly. Do not use markdown headers or ALL CAPS shouting. Ensure the tone is constructive and professional. Do not use backticks to quote any names or expressions.
7. SPECIFICITY REQUIREMENT: Each inline comment MUST reference the exact function name, file, line number when known, and specific triggering condition. Prefer the finding's locations field when present."#;

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

pub fn prescreen_stage() -> Stage<ReviewState, Phase0Output> {
    Stage::builder("stage_0_prescreen")
        .system_prompt(PromptTemplate::<ReviewState>::new(
            "You are an AI assistant preparing a QEMU patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"qom.md\", \"memory.md\"]}\n```",
        ))
        .user_prompt(
            PromptTemplate::<ReviewState>::new(
                "<subsystem_guide_index>\n@include(\"subsystem/subsystem.md\")\n</subsystem_guide_index>\n\n<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &ReviewState| s.target_commit_diff.clone())
            .include_file("subsystem/subsystem.md"),
        )
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "selected_prompts": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["selected_prompts"]
        })))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(|state, out: Phase0Output| {
            let prompts: Vec<String> = out
                .selected_prompts
                .into_iter()
                .filter(|name| !QEMU_STAGE_EXCLUSIVE_GUIDES.contains(&name.as_str()))
                .collect();
            state.selected_guides = prompts;
        })
        .build()
}

pub fn planning_stage() -> Stage<ReviewState, PlanningOutput> {
    Stage::builder("stage_planning")
        .system_prompt(qemu_system_prompt(true))
        .user_prompt(PromptTemplate::<ReviewState>::new(
            r#"Analyze the provided patch and determine which of the following review stages are relevant and should be executed:
- Stage 4: QOM lifecycle and resource management
- Stage 5: Concurrency, BQL, and coroutines
- Stage 6: Security audit and guest attack surface
- Stage 7: Virtual hardware and device model review

CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo fix, you may omit some stages. Stages 1, 2, and 3 are always run and should not be included in your answer.

You MUST respond with ONLY a JSON object, no other text. Example:
```json
{"relevant_stages": [4, 5, 6, 7]}
```"#,
        ))
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "relevant_stages": {
                    "type": "array",
                    "items": { "type": "integer" }
                }
            },
            "required": ["relevant_stages"]
        })))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(|state, out: PlanningOutput| {
            let mut stages = vec![1, 2, 3];
            for n in out.relevant_stages {
                if (4..=7).contains(&n) && !stages.contains(&n) {
                    stages.push(n);
                }
            }
            state.planned_stages = stages;
        })
        .build()
}

fn analysis_stage(
    stage_num: u8,
    name: &'static str,
    instruction: &'static str,
    guides: &[&'static str],
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<ReviewState>> {
    let mut user_template = PromptTemplate::<ReviewState>::new(format!(
        "{}\n\n{}",
        instruction, STAGE_JSON_SCHEMA_EXAMPLE
    ));
    for guide in guides {
        user_template = user_template.include_file(*guide);
    }

    Box::new(
        Stage::builder(name)
            .system_prompt(qemu_system_prompt(stage_uses_commit_log(stage_num)))
            .user_prompt(user_template)
            .output_format(
                OutputFormat::json()
                    .with_validator(validate_stages_1_to_8)
                    .with_feedback_formatter(format_stages_1_to_8_feedback),
            )
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                ..Default::default()
            })
            .reduce(move |state: &mut ReviewState, out: StageConcernsOutput| {
                append_stage_items(
                    &mut state.all_concerns,
                    &out.concerns,
                    stage_num,
                    "General",
                    "description",
                );
                append_stage_dismissed_concerns(
                    &mut state.all_dismissed_concerns,
                    &out.dismissed_concerns,
                    stage_num,
                );
            })
            .build(),
    )
}

pub fn resolve_qemu_analysis_stages_with_options(
    state: &ReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<ReviewState>>> {
    let selected_stages = if let Some(ref manual) = state.manual_stages {
        manual.clone()
    } else if !state.planned_stages.is_empty() {
        state.planned_stages.clone()
    } else {
        vec![1, 2, 3, 4, 5, 6, 7]
    };

    let mut stages = Vec::new();
    for num in selected_stages {
        match num {
            1 => stages.push(analysis_stage(
                1,
                "stage_1",
                STAGE_1_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            2 => stages.push(analysis_stage(
                2,
                "stage_2",
                STAGE_2_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            3 => stages.push(analysis_stage(
                3,
                "stage_3",
                STAGE_3_INSTRUCTION,
                &["technical-patterns.md"],
                max_turns,
                temperature,
            )),
            4 => stages.push(analysis_stage(
                4,
                "stage_4",
                STAGE_4_INSTRUCTION,
                &["subsystem/qom.md"],
                max_turns,
                temperature,
            )),
            5 => stages.push(analysis_stage(
                5,
                "stage_5",
                STAGE_5_INSTRUCTION,
                &["subsystem/concurrency.md"],
                max_turns,
                temperature,
            )),
            6 => stages.push(analysis_stage(
                6,
                "stage_6",
                STAGE_6_INSTRUCTION,
                &["subsystem/memory.md", "subsystem/dma.md"],
                max_turns,
                temperature,
            )),
            7 => stages.push(analysis_stage(
                7,
                "stage_7",
                STAGE_7_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            _ => {}
        }
    }
    stages
}

pub fn stage_8_deduplication(
    max_turns: usize,
    temperature: f32,
) -> Stage<ReviewState, StageConcernsOutput> {
    Stage::builder("stage_8_deduplication")
        .system_prompt(qemu_system_prompt(true))
        .user_prompt(
            PromptTemplate::<ReviewState>::new(format!(
                r#"{STAGE_8_INSTRUCTION}

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Preserve the most precise location details from the input. Do not invent line numbers; use null when exact values are unknown."#
            ))
            .with_var("aggregated_concerns", |s: &ReviewState| {
                serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
            })
            .with_var("aggregated_dismissed_concerns", |s: &ReviewState| {
                serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(
            OutputFormat::json()
                .with_validator(validate_stages_1_to_8)
                .with_feedback_formatter(format_stages_1_to_8_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: StageConcernsOutput| {
            state.deduplicated_concerns = out.concerns;
            state.deduplicated_dismissed_concerns = out.dismissed_concerns;
        })
        .build()
}

pub fn stage_9_conflict_resolution(
    max_turns: usize,
    temperature: f32,
) -> Stage<ReviewState, Stage9Output> {
    Stage::builder("stage_9_conflict_resolution")
        .system_prompt(qemu_system_prompt(true))
        .user_prompt(
            PromptTemplate::<ReviewState>::new(format!(
                r#"{STAGE_9_INSTRUCTION}

Consolidated Concerns:
{{{{deduplicated_concerns}}}}

Consolidated Dismissed Concerns:
{{{{deduplicated_dismissed_concerns}}}}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Preserve the most precise locations from the retained concerns. Do not invent line numbers; use null when exact values are unknown."#
            ))
            .with_var("deduplicated_concerns", |s: &ReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_concerns).unwrap_or_default()
            })
            .with_var("deduplicated_dismissed_concerns", |s: &ReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::json())
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: Stage9Output| {
            state.conflict_resolved_concerns = out.concerns;
        })
        .build()
}

pub fn stage_10_verification(
    max_turns: usize,
    temperature: f32,
) -> Stage<ReviewState, Stage10Output> {
    Stage::builder("stage_10_verification")
        .system_prompt(qemu_system_prompt(true))
        .user_prompt(
            PromptTemplate::<ReviewState>::new(format!(
                r#"{STAGE_10_INSTRUCTION}

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid. If you cannot find concrete proof of safety, you must retain the concern.{{{{follow_up_series_section}}}}

Consolidated Concerns:
{{{{conflict_resolved_concerns}}}}

Return ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: "problem" (a string containing the vulnerability description), "severity" (a string: Low, Medium, High, or Critical), "severity_explanation" (a string detailing the reasoning and proof), "preexisting" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), "locations" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters)."#
            ))
            .include_file("false-positive-guide.md")
            .include_file("severity.md")
            .with_var("follow_up_series_section", |s: &ReviewState| {
                s.follow_up_series_context
                    .as_ref()
                    .map(|ctx| format!("\n\n{}", ctx))
                    .unwrap_or_default()
            })
            .with_var("conflict_resolved_concerns", |s: &ReviewState| {
                serde_json::to_string_pretty(&s.conflict_resolved_concerns).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::json())
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: Stage10Output| {
            state.findings = out.findings;
        })
        .build()
}

pub fn stage_11_inline_report(max_turns: usize, temperature: f32) -> Stage<ReviewState, String> {
    Stage::builder("stage_11_report")
        .system_prompt(qemu_system_prompt(true))
        .user_prompt(
            PromptTemplate::<ReviewState>::new(format!(
                r#"{STAGE_11_INSTRUCTION}

Findings:
{{{{findings}}}}

Return raw text output, not JSON."#
            ))
            .include_file("inline-template.md")
            .with_var("findings", |s: &ReviewState| {
                serde_json::to_string_pretty(&s.findings).unwrap_or_default()
            })
            .with_var("target_commit_sha", |s: &ReviewState| {
                s.target_commit_sha.clone()
            }),
        )
        .output_format(OutputFormat::text_with_validator(
            validate_inline_format,
            format_inline_feedback,
        ))
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            recitation_policy: RecitationPolicy::FallbackToFreeForm {
                reminder: "Do not quote code verbatim. Summarize your review directly.".to_string(),
            },
            ..Default::default()
        })
        .reduce(|state, out: String| {
            state.review_inline = out;
        })
        .build()
}

/// Constructs the declarative workflow for QEMU patch review with default limits.
pub fn build_qemu_review_workflow() -> Workflow<ReviewState> {
    build_qemu_review_workflow_with_options(20, 0.0)
}

/// Constructs the declarative workflow for QEMU patch review with custom limits and temperature.
pub fn build_qemu_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<ReviewState> {
    Workflow::builder("qemu_code_review")
        .stage(prescreen_stage())
        .dynamic_parallel(
            planning_stage(),
            move |state| resolve_qemu_analysis_stages_with_options(state, max_turns, temperature),
            ParallelPolicy::FailFast,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty(),
            "No concerns raised in initial analysis stages",
        )
        .stage(stage_8_deduplication(max_turns, temperature))
        .early_exit_if(
            |s| s.deduplicated_concerns.is_empty(),
            "No concerns remaining after deduplication",
        )
        .stage(stage_9_conflict_resolution(max_turns, temperature))
        .early_exit_if(
            |s| s.conflict_resolved_concerns.is_empty(),
            "No concerns remaining after conflict resolution",
        )
        .stage(stage_10_verification(max_turns, temperature))
        .early_exit_if(
            |s| s.findings.is_empty(),
            "No findings validated in verification stage",
        )
        .stage(stage_11_inline_report(max_turns, temperature))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_qemu_workflow_structure() {
        let workflow = build_qemu_review_workflow();
        assert_eq!(workflow.name, "qemu_code_review");
        assert_eq!(workflow.steps.len(), 10);
    }

    #[test]
    fn test_qemu_system_prompt_renders_identity() {
        let template = qemu_system_prompt(true);
        let state = ReviewState {
            target_commit_sha: "abcd1234".to_string(),
            baseline_sha: "beef5678".to_string(),
            target_commit_diff: "diff --git a/hw/scsi/scsi-disk.c b/hw/scsi/scsi-disk.c"
                .to_string(),
            ..Default::default()
        };
        let rendered = template.render_for_log(&state);
        assert!(rendered.contains("You are an expert QEMU maintainer"));
        assert!(rendered.contains("Target Commit SHA: abcd1234"));
        assert!(rendered.contains("Baseline SHA: beef5678"));
        assert!(rendered.contains("diff --git a/hw/scsi/scsi-disk.c"));
    }

    #[test]
    fn test_resolve_qemu_analysis_stages_default_and_manual() {
        let default_state = ReviewState::default();
        let stages = resolve_qemu_analysis_stages_with_options(&default_state, 10, 0.0);
        assert_eq!(stages.len(), 7);

        let manual_state = ReviewState {
            manual_stages: Some(vec![1, 4, 6]),
            ..Default::default()
        };
        let manual_stages = resolve_qemu_analysis_stages_with_options(&manual_state, 10, 0.0);
        assert_eq!(manual_stages.len(), 3);
        assert_eq!(manual_stages[0].name(), "stage_1");
        assert_eq!(manual_stages[1].name(), "stage_4");
        assert_eq!(manual_stages[2].name(), "stage_6");
    }
}
