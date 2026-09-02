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

//! Declarative review workflow definition for LLVM compiler infrastructure.
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
pub const LLVM_STAGE_EXCLUSIVE_GUIDES: &[&str] = &["transforms.md", "codegen.md"];

/// LLVM system prompt template.
pub fn llvm_system_prompt(use_log: bool) -> PromptTemplate<ReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<ReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are an expert LLVM maintainer and compiler engineer. Your goal is to perform a deep, rigorous review of a proposed LLVM compiler change to ensure transformation soundness, SSA invariance, type safety, memory safety, and adherence to LLVM coding standards.

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

You are a senior LLVM maintainer evaluating the high-level compiler architecture and transformation intent of a proposed commit. Analyze the commit message and the conceptual change. Focus on the big picture: Are there architectural flaws, pass pipeline ordering issues, IR construct design flaws, or bitcode backwards compatibility breakages? Consider the long-term maintainability, compile-time complexity, and system-wide implications of this design. If the core idea is unsound, incorrect, or violates LLVM design principles, raise a concern. Be open-minded but thorough; question assumptions made by the author and consider alternative, cleaner designs."#;

const STAGE_2_INSTRUCTION: &str = r#"# Stage 2. High-level implementation verification

You are verifying if the provided code changes actually implement what the commit message claims. Look for undocumented side-effects, missing cases (e.g., handling one order of commutative operands in InstCombine but forgetting m_c_* or reverse operand matching), unhandled vector or pointer types, and test case omissions. Verify that all claims in the commit message are fully realized in the code and covered by LIT tests. Identify any incomplete implementations, implicit behavioral changes, or API contract violations. Verify that the logic is mathematically and semantically sound. Check for off-by-one errors in bitwidths, incorrect APInt/APSInt operations, and verify that all arguments passed to IRBuilder or AST helpers are valid. Don't trust the commit message without verifying each claim. Assume that the message might be incorrect or incomplete.

MULTI-FILE & CALLSITE AUDIT REQUIREMENT: If the patch touches multiple files or callsites, you MUST systematically audit EVERY modified file and callsite, not just the primary or largest file. Small callsite changes or API adaptation sites (such as in coroutine builders, AST consumers, or frontends) frequently hide unhandled error paths, leaked or uncleared state, and parameter or alignment mismatches."#;

const STAGE_3_INSTRUCTION: &str = r#"# Stage 3. Execution flow, cast safety, and static analysis

You are a static analysis engine tracing execution flow in LLVM C++ code. Carefully trace the control flow of the provided patch. Exhaustively examine logic errors, incorrect loop conditions, unhandled error paths, and off-by-one errors. Check every branch, switch statement, and conditional. Specifically look for cast safety violations:
1. Cast safety: Audit all uses of dyn_cast<T>, cast<T>, and isa<T>. Ensure dyn_cast<T> return values are null-checked before dereferencing. If cast<T> is used, verify that the type invariant is guaranteed by callers or preceded by isa<T>.
2. Pointer dereferences: Check for unchecked nullptr dereferences, especially on VNInfo, MachineInstr operands, Type pointers, and Constant expressions.
3. Enum coverage: Check that switch statements over IR opcode or AST Kind handle all relevant enumerators or have a safe default.
4. State management and optional handling: In loops, callbacks, or lambda helpers, check that failed lookups or nullopt optional results properly clear out-parameters and reset temporary state rather than leaking stale values from previous iterations."#;

const STAGE_4_INSTRUCTION: &str = r#"# Stage 4. Lifetime, memory safety, and iterator invalidation

You are an expert in LLVM data structures, memory management, and C++ lifetime rules. Analyze the patch for memory leaks, Use-After-Free (UAF), double frees, uninitialized variables, and invalid access patterns:
1. Iterator invalidation: Audit loops that mutate IR or AST structures. In particular, mutating or erasing instructions while iterating over basic blocks (for (Instruction &I : BB) { I.eraseFromParent(); }) causes dangling iterators; make_early_inc_range or backward iteration must be used.
2. RAII and ownership: Ensure std::unique_ptr, ArrayRef, StringRef, and SmallVector lifetimes are sound. Scrutinize StringRef and ArrayRef instances to ensure their backing buffers outlive their usage (avoid returning references to temporary SmallStrings or vectors).
3. Use lists and Def-Use chains: Verify that replaceAllUsesWith (RAUW) and dropAllReferences are called properly before instruction erasure to avoid dangling Use handles.
4. ValueHandle tracking: Ensure WeakVH and TrackingVH are used where instructions or basic blocks may be deleted by other passes or subroutines."#;

const STAGE_5_INSTRUCTION: &str = r#"# Stage 5. Correctness, algebraic rewrites, and SSA soundness

You are a world-class compiler correctness and formal verification expert auditing an LLVM patch.
Carefully review the proposed patch for ANY semantic soundness or algebraic correctness bugs:
1. Undefined behavior and poison propagation: Does the transformation preserve poison semantics? Does it incorrectly drop or introduce nsw (no signed wrap), nuw (no unsigned wrap), exact, or fast-math flags? Dropping poison-generating flags when hoisting or commuting can cause silent miscompilation.
2. Type bitwidth matching: Verify that types, integer bitwidths, and vector element counts match in IRBuilder calls (CreateICmp, CreateSelect, CreateBinOp). Creating an ICmp between mismatched integer types or generating a Select with condition vector length mismatch triggers verification/assertion failure.
3. SSA dominance: Does the transformation insert an instruction that uses values defined in non-dominating basic blocks? Are phi node operands correctly dominated along their incoming edges?
4. Termination and infinite loops: In InstCombine or DAGCombine, does the rewrite introduce a cycle with an existing canonicalization rule (A -> B while another rule rewrites B -> A), causing the compiler to hang or time out?
5. Constant folding soundness: Check for division by zero, shifts by bitwidth or greater, and signed overflow in compile-time constant evaluation."#;

const STAGE_6_INSTRUCTION: &str = r#"# Stage 6. Robustness, compiler crash safety, and diagnostics

You are a compiler robustness engineer auditing an LLVM patch. Look for issues that can crash the compiler or produce misleading diagnostics:
1. Assertion safety: Ensure assertions (assert(...)) encode true internal invariants rather than assumptions about external, malformed, or user-supplied code. In frontend (Clang) and linker (LLD) code, invalid user inputs must trigger graceful diagnostics, not assertion crashes.
2. Recursion limits: In recursive algorithms (e.g., ValueTracking computeKnownBits, InstCombine matchers, DAGCombiner), ensure recursion depth limits (e.g., MaxDepth) are enforced to prevent host stack exhaustion.
3. Release build safety: Verify that code behavior is identical in debug and release (NDEBUG) builds; ensure side-effecting code is not placed inside assert().
4. Verifier compliance: Verify that transformed functions or modules will pass the LLVM verifier (llvm::verifyFunction)."#;

const STAGE_7_INSTRUCTION: &str = r#"# Stage 7. Code generation, target lowering, and backend review

You are a compiler backend and code generation engineer reviewing LLVM backend changes. Rigorously review:
1. SelectionDAG / GlobalISel node combinations: Check operand type constraints, legal vs custom lowering rules, and ensure DAG node creation uses legal types for the target.
2. MachineInstr and operand constraints: Verify physical vs virtual register usage, register classes, subregister indices, and operand flags (Kill, Dead, Undef, Def).
3. Calling conventions and ABI: Verify that argument passing, return values, callee-saved registers, and stack frame alignment conform to the target ABI.
4. Architecture-specific features: Check target instruction encodings, relocations, memory model constraints, and atomic ordering requirements on targets like X86, AArch64, RISC-V, and AMDGPU.
If the patch is purely frontend or target-independent IR without backend/target logic, return {"concerns": [], "dismissed_concerns": []}."#;

const STAGE_10_INSTRUCTION: &str = r#"# Stage 10. Verification and severity estimation

You are the lead reviewer validating consolidated concerns. You will be given a list of deduplicated concerns after conflict resolution.
1. Validate each concern and prove the provided reasoning against LLVM compiler invariants. Report all valid concerns as findings. If necessary, use tools to gather additional material. Discard all false positives.
2. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning. If you cannot find definitive proof that the concern is a false positive, it must be reported as a finding.
3. SERIES VALIDATION RULE: If follow-up patches in this series are provided in the context, check if each identified concern is resolved or fixed in the final state of the series. If the problem has been resolved, fixed, or the code was rewritten in a subsequent patch in this series, you MUST discard the concern and NOT report it as a finding.
4. When referring to other patches within this series in your explanation, DO NOT use git hashes. Instead, refer to them by their patch subject.
5. Assign a severity (low, medium, high, critical) following LLVM severity guidelines: reason through consequence (silent wrong code, assertion failure/crash, optimization regression, compile-time hang), triggering path, and input reachability.
6. If the problem did exist in the code before the patch was applied, say it explicitly: 'This problem wasn't introduced by this patch, but...'. Discard low- and medium-severity pre-existing problems, report only high- and critical severity issues.
7. SPECIFICITY REQUIREMENT: Every finding MUST cite the exact function name(s), file path(s), line number(s) when known, and triggering conditions where the bug manifests.
8. Carry forward the locations from the validated concern into each finding."#;

const STAGE_11_INSTRUCTION: &str = r#"# Stage 11. Plain-text inline review report generation

You are an automated review bot generating a report for LLVM code reviews. Convert the provided JSON findings into a polite, standard, inline-commented review reply.

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
            "You are an AI assistant preparing an LLVM patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"transforms.md\", \"ir-core.md\"]}\n```",
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
                .filter(|name| !LLVM_STAGE_EXCLUSIVE_GUIDES.contains(&name.as_str()))
                .collect();
            state.selected_guides = prompts;
        })
        .build()
}

pub fn planning_stage() -> Stage<ReviewState, PlanningOutput> {
    Stage::builder("stage_planning")
        .system_prompt(llvm_system_prompt(true))
        .user_prompt(PromptTemplate::<ReviewState>::new(
            r#"Analyze the provided patch and determine which of the following review stages are relevant and should be executed:
- Stage 4: Lifetime, memory safety, and iterator invalidation
- Stage 5: Correctness, algebraic rewrites, and SSA soundness
- Stage 6: Robustness, compiler crash safety, and diagnostics
- Stage 7: Code generation, target lowering, and backend review

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
            .system_prompt(llvm_system_prompt(stage_uses_commit_log(stage_num)))
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

pub fn resolve_llvm_analysis_stages_with_options(
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
                &[],
                max_turns,
                temperature,
            )),
            5 => stages.push(analysis_stage(
                5,
                "stage_5",
                STAGE_5_INSTRUCTION,
                &["subsystem/transforms.md"],
                max_turns,
                temperature,
            )),
            6 => stages.push(analysis_stage(
                6,
                "stage_6",
                STAGE_6_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            7 => stages.push(analysis_stage(
                7,
                "stage_7",
                STAGE_7_INSTRUCTION,
                &["subsystem/codegen.md"],
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
        .system_prompt(llvm_system_prompt(true))
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
        .system_prompt(llvm_system_prompt(true))
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
        .system_prompt(llvm_system_prompt(true))
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
        .system_prompt(llvm_system_prompt(true))
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

/// Constructs the declarative workflow for LLVM patch review with default limits.
pub fn build_llvm_review_workflow() -> Workflow<ReviewState> {
    build_llvm_review_workflow_with_options(20, 0.0)
}

/// Constructs the declarative workflow for LLVM patch review with custom limits and temperature.
pub fn build_llvm_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<ReviewState> {
    Workflow::builder("llvm_code_review")
        .stage(prescreen_stage())
        .dynamic_parallel(
            planning_stage(),
            move |state| resolve_llvm_analysis_stages_with_options(state, max_turns, temperature),
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
    fn test_build_llvm_workflow_structure() {
        let workflow = build_llvm_review_workflow();
        assert_eq!(workflow.name, "llvm_code_review");
        assert_eq!(workflow.steps.len(), 10);
    }

    #[test]
    fn test_llvm_system_prompt_renders_identity() {
        let template = llvm_system_prompt(true);
        let state = ReviewState {
            target_commit_sha: "1234abcd".to_string(),
            baseline_sha: "5678beef".to_string(),
            target_commit_diff:
                "diff --git a/llvm/lib/Transforms/InstCombine/InstCombineAndOrXor.cpp".to_string(),
            ..Default::default()
        };
        let rendered = template.render_for_log(&state);
        assert!(rendered.contains("You are an expert LLVM maintainer"));
        assert!(rendered.contains("Target Commit SHA: 1234abcd"));
        assert!(rendered.contains("Baseline SHA: 5678beef"));
        assert!(rendered.contains("InstCombineAndOrXor.cpp"));
    }

    #[test]
    fn test_resolve_llvm_analysis_stages_default_and_manual() {
        let default_state = ReviewState::default();
        let stages = resolve_llvm_analysis_stages_with_options(&default_state, 10, 0.0);
        assert_eq!(stages.len(), 7);

        let manual_state = ReviewState {
            manual_stages: Some(vec![1, 3, 5]),
            ..Default::default()
        };
        let manual_stages = resolve_llvm_analysis_stages_with_options(&manual_state, 10, 0.0);
        assert_eq!(manual_stages.len(), 3);
        assert_eq!(manual_stages[0].name(), "stage_1");
        assert_eq!(manual_stages[1].name(), "stage_3");
        assert_eq!(manual_stages[2].name(), "stage_5");
    }
}
