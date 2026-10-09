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

//! Declarative patch review workflow for GCC (GNU Compiler Collection).
//!
//! This module defines the multi-stage review pipeline for `--project gcc`,
//! reusing [`LinuxPatchReviewState`] and shared verification helpers while
//! organizing discovery into seven orthogonal issue-class stages (`goal`,
//! `implementation`, `execution-flow`, `resources`, `types-math`,
//! `state-invalidation`, and `diagnostics-abi`) paired with dynamic subsystem
//! guide selection in `pre-screen`.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::workflow::{
    ExecutableStage, OutputFormat, ParallelPolicy, PromptTemplate, RecitationPolicy, Stage,
    StagePolicy, ToolScope, Workflow,
};
use crate::workflows::guard::{normalize_stage_name, sanitize_guide_name};
use crate::workflows::linux_patch_review::{
    AnalysisStage, ConsolidationStage, LinuxPatchReviewState, POST_VERIFICATION_STAGE_NAMES,
    PlanningOutput, PostVerificationOutput, PrescreenOutput, SERIES_CONTEXT_PLACEHOLDER,
    StageConcernsOutput, VerificationOutput, append_stage_dismissed_concerns_with_prompts,
    append_stage_items_with_prompts, batch_hard_cases_by_severity, collect_stage_prompts,
    enrich_post_verification_output, enrich_verification_output, extra_prompt_paths_for_items,
    extra_prompt_paths_for_state, format_post_verification_feedback,
    format_verification_stage_feedback, has_valid_proof_location, record_verified_findings,
    validate_post_verification_batch_output, validate_verification_stage_output,
};

/// State container for a GCC patch review run.
pub type GccPatchReviewState = LinuxPatchReviewState;

// ---------------------------------------------------------------------------
// System Prompt Template
// ---------------------------------------------------------------------------

pub fn gcc_system_prompt(use_log: bool) -> PromptTemplate<GccPatchReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<GccPatchReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are a senior GCC global reviewer and compiler maintainer. Your goal is to perform a deep, rigorous review of a proposed GCC patch to prevent wrong-code bugs (silent miscompilations), Internal Compiler Errors (ICEs / segmentation faults / gcc_assert failures), rejects-valid / accepts-invalid front-end regressions, GGC/container memory corruption, and build/cross-target regressions.

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents contain the official GCC review methodology, architectural invariants, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying compiler bugs and anti-patterns.
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
    .with_var("target_commit_sha", |s: &GccPatchReviewState| {
        s.target_commit_sha.clone()
    })
    .with_var("baseline_sha", |s: &GccPatchReviewState| {
        s.baseline_sha.clone()
    })
    .with_var("target_commit_diff", |s: &GccPatchReviewState| {
        s.target_commit_diff.clone()
    })
    .with_var("target_commit_diff_only", |s: &GccPatchReviewState| {
        s.target_commit_diff_only.clone()
    })
    .with_var("prefetched_block", |s: &GccPatchReviewState| {
        if s.prefetch_failed {
            format!(
                "\n\nAutomatic source prefetch failed for target commit {}. Before analyzing the code, use git_read_files and git_grep at that revision to gather the source context. Do not infer source contents from the physical checkout.\n",
                s.target_commit_sha
            )
        } else if s.prefetched_context.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following source excerpts were fetched from the target commit identified by Source revision below, based on the modified lines in the patch. They include modified definitions and selected dependencies. Parent and series-final revisions must be inspected separately with Git tools.\nIf it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n{}\n</pre_fetched_context>",
                s.prefetched_context
            )
        }
    })
    .with_var("custom_prompt_block", |s: &GccPatchReviewState| {
        s.custom_prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map_or_else(String::new, |p| {
                format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
            })
    })
    .include_files_from_state(|s: &GccPatchReviewState| {
        let mut paths = vec![PathBuf::from("review-core.md")];
        if !s.selected_guides.is_empty() {
            for guide in &s.selected_guides {
                paths.push(PathBuf::from("subsystem").join(guide));
                paths.push(PathBuf::from("patterns").join(guide));
            }
        }
        paths
    })
}

// ---------------------------------------------------------------------------
// Stage Instructions (Orthogonal Issue Classes)
// ---------------------------------------------------------------------------

const STAGE_GOAL_INSTRUCTION: &str = r#"# Analyze commit main goal and architectural soundness

You are a senior GCC global reviewer evaluating the high-level intent and architectural soundness of a proposed GCC commit.
- Analyze the commit message (including any Bugzilla PR references `PR <component>/<id>`) and the conceptual change.
- Evaluate compiler phase and pass ordering invariants: Is this transformation, check, or lowering placed in the right compiler phase (front-end parsing/template instantiation, GENERIC folding, GIMPLE/SSA pass, IPA/LTO summary/materialization, or RTL expansion/combine/peephole)?
- Check whether the conceptual approach violates language standard semantics (C, C++, Fortran), GIMPLE/RTL IR invariants, or target ABI/ISA contracts.
- Check whether newly added guards, early bailouts, or fallback paths inadvertently defeat the commit's stated purpose on valid inputs (e.g. rejecting or pessimization of the exact cases the commit claims to optimize or support).
- Question assumptions made by the author: Does the design assume properties (such as constant integers, non-variable-length vectors, single basic-block predecessors, or non-dependent template expressions) that do not hold across all valid inputs and target architectures?"#;

const STAGE_IMPLEMENTATION_INSTRUCTION: &str = r#"# Verify implementation completeness, symmetry, and API contracts

You are verifying whether the code changes faithfully and completely implement what the commit message claims across all relevant code paths, without introducing unintended side effects or regressions.
- Check for duplicate or leftover code from refactoring/code motion: When a patch moves a call, statement, or state update earlier (e.g., before an `if` branch or loop), verify that the original call later in the function was actually removed rather than left behind to execute a second time!
- Check horizontal symmetry across related code paths, sibling functions, and operators (CRITICAL):
  - If a patch fixes a bug or updates handling for `<` / `<=` (`LT_EXPR`, `LE_EXPR`), check whether `>` / `>=` (`GT_EXPR`, `GE_EXPR`), `EQ_EXPR`, `NE_EXPR`, `BIT_IOR_EXPR` vs `BIT_XOR_EXPR` / `BIT_AND_EXPR`, or reversed operands require corresponding updates.
  - If a patch updates one function, overload, pass instance (e.g. loop vectorizer vs SLP vectorizer, FRE vs PRE, standard vs placement/array forms, scalar vs vector paths), or target pattern variant, inspect adjacent sibling functions/patterns in the same file using `git_read_files` or `git_grep` to check if the fix is incomplete. Report any incomplete symmetric fix with `"preexisting": false`!
- Verify caller/callee and target hook API contracts: Check that all callers and callees of modified functions agree on parameter meanings (e.g. scalar element mode vs vector mode in target hooks, bit offset vs byte offset, inclusive vs exclusive bounds), return values, and side effects.
- Check structural edge cases: empty sequences/constructors, zero-sized types, bit-fields, `RANGE_EXPR` in array `CONSTRUCTOR`s, multi-dimensional arrays, and variadic/default arguments.
- Do not stop after finding one issue; systematically audit every modified function, macro, pattern, and hunk in the diff."#;

const STAGE_EXECUTION_FLOW_INSTRUCTION: &str = r#"# Trace control flow, null/error_mark_node propagation, and assertion preconditions

You are a static analysis engine tracing local and interprocedural control flow through the modified code.
Carefully trace every branch, switch, loop condition, early return, and helper call:
1. Null Pointer Dereferences (`NULL_TREE`, `NULL_RTX`, `NULL`, `nullptr`):
   - `gimple_bb (stmt)` returns `NULL` for statements not yet inserted into a basic block or detached statements (and `SSA_NAME_DEF_STMT` of a default definition is a `GIMPLE_NOP` with `gimple_bb == NULL`).
   - `SINGLE_SSA_TREE_OPERAND (stmt, SSA_OP_USE)` returns `NULL_TREE` if the statement has zero or more than one SSA use operand.
   - `DECL_INITIAL (decl)`, `TREE_TYPE (node)`, `TYPE_SIZE (type)`, `TYPE_SIZE_UNIT (type)`, `TYPE_DOMAIN (type)`, `gimple_lhs (stmt)`, and `SET_DEST` / `SET_SRC` can be `NULL` depending on node kind or language construct.
2. `error_mark_node` Propagation (`ice-on-invalid-code`):
   - In front-ends and middle-end helpers reachable after diagnostic errors, functions often return `error_mark_node` (or a node whose `TREE_TYPE` is `error_mark_node`). Passing `error_mark_node` to `TYPE_MAIN_VARIANT`, `TYPE_PRECISION`, `DECL_CONTEXT`, or `tree_to_uhwi` without checking `error_operand_p (t)` or `t == error_mark_node` causes an immediate ICE.
3. Checked Accessor Macro Preconditions (`--enable-checking`):
   - `DECL_CONTEXT (node)`, `DECL_NAME (node)`, `DECL_UID (node)`, `DECL_SOURCE_LOCATION (node)` require `DECL_P (node)`. Calling `DECL_*` macros on a node that can be a `TYPE_P` (which uses `TYPE_CONTEXT`, `TYPE_NAME`, `TYPE_MAIN_DECL`, or `location_of`) or an `EXPR_P` triggers a `tree_check_failed` ICE!
   - `TREE_INT_CST_LOW (t)`, `tree_to_shwi (t)`, and `tree_to_uhwi (t)` assert that `t` is an `INTEGER_CST` fitting in signed/unsigned `HOST_WIDE_INT`. Always verify `tree_fits_shwi_p (t)` / `tree_fits_uhwi_p (t)` first.
   - `INTVAL (rtx)` requires `CONST_INT_P (rtx)`. `REGNO (rtx)` requires `REG_P (rtx)` (check `SUBREG_P` first if subregs can reach the site).
4. Indexing, Bounds & Loop Control Flow:
   - Check off-by-one bounds in array/vector loops, `continue` / `break` jumps that skip loop-tail updates, and fallthrough in `switch` statements."#;

const STAGE_RESOURCES_INSTRUCTION: &str = r#"# Audit resource management, object/state lifetimes, rollback, and aliasing

You are an expert in resource management, memory safety, and state lifetime auditing the patch across all compiler layers:
1. Allocation, Initialization & Error-Path Cleanup Lifecycles (`alloc -> init -> use -> cleanup -> free`):
   - Trace every local pointer, buffer, and handle across all `goto` labels and early returns. Check whether any `goto` or early exit jumps over variable initialization into a cleanup block that calls `free ()`, `XDELETEVEC`, `BITMAP_FREE`, or `.release ()` on an uninitialized variable!
   - Check for resource and memory leaks on normal and error paths: `vec<T, va_heap>` initialized via `.create (n)` or `vNULL` with `.safe_push ()` without `.release ()` (prefer `auto_vec<T>`), `bitmap` allocated via `BITMAP_ALLOC` without `BITMAP_FREE` (prefer `auto_bitmap`), `obstack`, `mpz_t` (`mpz_init` without `mpz_clear`), frontend expressions (`gfc_free_expr`), and heap allocations (`XNEW` / `new` / `path_range_query`).
2. Speculative / Tentative State Rollback & Scoped Overrides (CRITICAL):
   - When a loop or helper tentatively accumulates state (such as an offset accumulator `extra_off`, a candidate list, or partial IR/AST mutations) across multiple iterations and then fails to find a valid match on a later iteration, verify that the accumulated state is rolled back or discarded rather than leaked into the caller or subsequent iterations!
   - When a function supports a dry-run / capability-probing flag (such as `d->testing_p` in target expanders or tentative parsing in frontends), verify it does not mutate persistent compiler state or query uninitialized CFG/BB structures when probing.
   - Verify that temporary overrides of global/pass state (`input_location`, `cfun`, `current_function_decl`, `processing_template_decl`, `current_class_type`, `inhibit_evaluation_warnings`) are restored on all return and error paths.
3. Container Reallocation & Re-Entrancy Invalidation (Use-After-Free):
   - Taking a pointer or reference to an element inside a `vec<T>` (`&v[i]`, `v.last ()`) or `hash_table` / `hash_map` (`map.get_or_insert (k)`) and holding it across any operation that may grow or reallocate the container (`safe_push`, `reserve`, `get_or_insert`, or recursive calls like `complete_type` / `tsubst` that insert into the same table) causes a dangling pointer/reference UAF!
   - Never insert or finalize new symbol table nodes (`varpool_node::add`, `cgraph_node::create`, `decl_attributes` creating auxiliary variables) while iterating over the symbol table (`FOR_EACH_VARIABLE`, `FOR_EACH_DEFINED_VARIABLE`, `FOR_EACH_FUNCTION`).
4. GGC (`ggc_collect`), `GTY(())`, and Expression Unsharing:
   - Any `static` or global variable holding GC-allocated pointers (`tree`, `gimple *`, `rtx`, `cgraph_node *`) across passes must be annotated with `GTY(())`, and heap containers (`vec<tree, va_heap>`, `std::vector`) are NOT scanned by `ggc_collect ()`.
   - Verify `ggc_free (targs)` or manual deletion is never called when a newly cached specialization, type, or IR node still references that object.
   - Never mutate a cached or shared `CONSTRUCTOR` (`unshare_constructor` / `unshare_expr`, including `RANGE_EXPR` indices) or shared `rtx` (`copy_rtx`) in place."#;

const STAGE_TYPES_MATH_INSTRUCTION: &str = r#"# Audit type representations, bit-widths, overflow semantics, and FP/algebraic math

You are a compiler type-system and computer-arithmetic specialist auditing bit-widths, precisions, modes, overflow rules, and mathematical transformations across the patch:
1. Bit-Width, Precision, Signedness & Mode Mismatches (CRITICAL):
   - `wide_int` vs `widest_int` / `offset_int`: `wi::to_wide (t)` has precision `TYPE_PRECISION (TREE_TYPE (t))`. Comparing or combining `wide_int` values from operands of potentially different precisions (such as shift counts `C1` and `C3`, or index vs pointer types) without converting to a common precision or using `wi::to_widest` / `wi::to_offset` triggers `gcc_checking_assert` ICEs!
   - Vector & Boolean precision traps: Calling `TYPE_PRECISION (type)` on a `VECTOR_TYPE` triggers a `tree_check_failed` ICE (use `element_precision (type)` or guard with `INTEGRAL_TYPE_P`). In Fortran (`LOGICAL`) and vector masks (`VECTOR_BOOLEAN_TYPE_P`), `BOOLEAN_TYPE` can have `TYPE_PRECISION > 1` or signed `-1`/`0` representation; never fold boolean negation into `BIT_NOT_EXPR` (`~x`) unless `TYPE_PRECISION (type) == 1 && TYPE_UNSIGNED (type)`.
   - Signed-to-unsigned domain conversions: In array domains (`complete_array_type`), zero-sized arrays `{}` have signed `maxindex = -1` (`[0, -1]`). Passing `maxindex` to `build_index_type (maxindex)` instead of `build_range_type (sizetype, size_zero_node, maxindex)` converts `-1` to unsigned `sizetype` (`0xffffffff` on 32-bit targets)!
   - RTL Modes & `poly_int`: `CONST_INT` has `VOIDmode`, whereas `CONST_POLY_INT` has an explicit integer mode (e.g. `SImode`) while satisfying `CONSTANT_P (op)`. When widening or unwrapping `ZERO_EXTEND` / `SIGN_EXTEND` across binary RTL operations, convert constant operands to the outer mode via `simplify_gen_unary` so a narrower `CONST_POLY_INT` is not passed into a wider `PLUS`. Never call `.to_constant ()` on `poly_int64` / `poly_uint64` unless guarded by `.is_constant (&val)`.
   - Host integer UB: Check for `1 << n` instead of `HOST_WIDE_INT_1U << n` (`1ULL << n`) and signed `HOST_WIDE_INT` overflow on the host compiler.
2. Integer Overflow & Wrap-Around Semantics:
   - Verify `TYPE_OVERFLOW_UNDEFINED (type)` vs `TYPE_OVERFLOW_WRAPS (type)` vs `TYPE_OVERFLOW_TRAPS (type)`.
   - When reassociating or cancelling constants across `MIN_EXPR` / `MAX_EXPR`, shifts, or comparisons (e.g. `minmax (a - c, b) + c -> minmax (a, b + c)`), verify that no new overflow is introduced on ANY path (both `a - c` and `b + c` must be proven overflow-free, or rewritten in an unsigned wrapping type via `rewrite_to_defined_overflow`).
   - Guard division/modulo/negation rewrites against `TYPE_MIN_VALUE` (`INT_MIN / -1`, `-INT_MIN`) and shift rewrites against negative or `>= precision` shift counts (`SHIFT_COUNT_TRUNCATED`).
3. IEEE-754 Floating-Point & Comparison Reversal Semantics:
   - Check `HONOR_NANS (type/mode)`, `HONOR_INFINITIES`, `HONOR_SIGNED_ZEROS`, `HONOR_SIGN_DEPENDENT_ROUNDING`, and `flag_trapping_math` (including vector reduction neutral values for `-0.0` vs `+0.0`).
   - `reversed_comparison_code (cmp, insn)` returns `UNKNOWN` when a condition cannot be safely inverted (e.g. FP comparisons when NaNs are honored or target CC modes cannot represent the inverse). Always check `code != UNKNOWN` before using the result, and never use `reverse_condition_maybe_unordered` on FP comparisons unless unordered cases are explicitly proven safe.
4. Algebraic Modifiers (`:c`, `:s`):
   - Check for missing `:c` commutativity annotations on binary/comparison patterns with asymmetric operands, and missing `:s` (`single_use`) when a replacement emits multiple operations or pushes an operation across a cast."#;

const STAGE_STATE_INVALIDATION_INSTRUCTION: &str = r#"# Audit cached state/metadata invalidation, IR synchronization, convergence, and determinism

You are a compiler state-consistency and dataflow verification expert auditing whether transformations properly invalidate stale metadata, synchronize IR/context state, terminate, and remain deterministic:
1. Stale Flow-Sensitive SSA Info & Function-Wide Analysis Sets (CRITICAL):
   - Whenever a GIMPLE statement is moved (`gsi_move_before`, `gsi_move_after`, `gsi_move_to_bb_end`), hoisted, sunk, or its defining expression is widened/rewritten so an `SSA_NAME` executes under a weaker condition or can hold values outside its old range (e.g. rewriting signed `X % Y` into `X & (Y - 1)` when `X` can be negative), verify `reset_flow_sensitive_info (lhs)` (or `reset_flow_sensitive_info_in_bb`) is called! Leaving stale `SSA_NAME_RANGE_INFO` or `SSA_NAME_PTR_INFO` causes wrong-code miscompilations in downstream passes.
   - When a pass queries a function-wide property set (such as `get_non_trapping ()` in `tree-ssa-phiopt.cc`) to justify hoisting or sinking a memory access out of a conditional basic block, verify that the set was not populated by a conditional load/store inside the guarded block itself!
2. IR & Evaluation Context Synchronization:
   - Whenever a GIMPLE statement's operands are modified in place, verify `update_stmt (stmt)` is called.
   - When replacing, splitting, or removing memory statements (`gimple_vdef` / `gimple_vuse`), verify virtual SSA form is maintained (`unlink_stmt_vdef`, copying `gimple_vuse`/`gimple_vdef`, or `TODO_update_ssa_only_virtuals`).
   - In `constexpr.cc`, when stepping into base class initializers, subobjects, or union members, verify `constexpr_ctx::ctor` and `constexpr_ctx::object` stay synchronized with the subobject being initialized.
   - When accessing `DECL_LANG_SPECIFIC (decl)` on a `VAR_DECL` or `FUNCTION_DECL` that may have been created by middle-end/common code or deserialized in C++ modules without language-specific data, verify `retrofit_lang_decl (decl)` is called first.
3. Pass Convergence & Non-Termination (`compile-time-hog`):
   - Check whether any new `match.pd`, `fold-const.cc`, `combine`, or `simplify-rtx` rule can ping-pong with an inverse canonicalization rule (e.g., a `match.pd` rule reversing a canonical `fold-const.cc` transformation without `#if GIMPLE`), causing an infinite folding loop.
   - Check that recursive walks over `SSA_NAME` definitions, `VALUE` CSE chains, or template/type graphs bound their depth or track visited nodes.
4. Codegen Determinism (`-fcompare-debug`):
   - Never iterate over a `hash_table`, `hash_map`, or `hash_set` keyed by raw pointer addresses when iteration order affects emitted instructions, basic-block ordering, or `DECL_UID` / `SSA_NAME_VERSION` allocation.
   - Verify `is_gimple_debug (stmt)` / `DEBUG_INSN_P (insn)` never alters pass decisions, instruction counts, or UID allocation."#;

const STAGE_DIAGNOSTICS_ABI_INSTRUCTION: &str = r#"# Audit diagnostic gating (SFINAE), ABI/target constraints, and build portability

You are a compiler interface, target ABI, and portability reviewer auditing diagnostic emission, SFINAE, target/ISA constraints, linkage, and host/target portability:
1. Speculative Compilation & SFINAE Diagnostic Gating (`tsubst_flags_t complain`):
   - In any C++ front-end function taking `tsubst_flags_t complain` (or called during template substitution / tentative parsing), verify that diagnostics (`error`, `error_at`, `permerror`, `pedwarn`, `warning`, `warning_at`) are ONLY emitted when `(complain & tf_error)` or `(complain & tf_warning)` is non-zero! Emitting an unguarded diagnostic during SFINAE breaks overload resolution (`rejects-valid`).
   - Verify `TREE_NO_WARNING` / `copy_warning` / `suppress_warning` preservation when folding or lowering expressions, and verify accurate `location_t` handling (`location_of (t)` instead of `DECL_SOURCE_LOCATION (t)` when `t` can be a `TYPE_P`).
2. Target Backend, Machine Description (`.md`), Optab & ISA Constraints:
   - Standard optab name collisions: Pattern names like `addv<mode>3`, `subv<mode>3`, `usubv<mode>3`, `mulv<mode>3`, `negv<mode>2` are reserved for middle-end `-ftrapv` trapping overflow arithmetic. Defining a non-trapping backend instruction under a `*v<mode>3` optab name silently breaks `-ftrapv`!
   - Check `(clobber (reg:CC ...))`, earlyclobber `"=&r"` when an output register is written before all inputs are read, `can_create_pseudo_p ()` in splits/expanders, predicate/constraint agreement, and explicit `(set_attr "mnemonic" ...)` when changing a static output template to C code `{ ... }` on targets with automatic mnemonic derivation (such as s390).
   - Verify ISA feature macro guards match the exact baseline ISA of emitted instructions (e.g., `TARGET_SSE4_1` vs `TARGET_AVX`, `TARGET_AVX512VL`), and guard target-specific function multi-versioning logic in shared code with `TARGET_HAS_FMV_TARGET_ATTRIBUTE`.
3. Linkage, Visibility, Offloading & Builtin Contracts:
   - Check symbol visibility and mangling invariants across translation units: Fortran `PRIVATE` module entities with `BIND(C)` and an explicit `binding_label` must keep default ELF visibility (not `VISIBILITY_HIDDEN`); C++20 modules must not force out TU-local anonymous namespaces (`!DECL_NAME (ns)`); class-scope anonymous unions must not change mangling across `-std=c++17` and `-std=c++20` (`TYPE_NAMESPACE_SCOPE_P`).
   - In outlined OpenMP `target` / `parallel` regions, Fortran dummy array descriptor fields (bounds, stride, `span`, data pointer) must be captured in local `DECL` variables on procedure entry rather than reloaded from the unmapped outer dummy descriptor.
   - When instrumenting calls or registering builtins (`builtins.def` / `builtin-types.def`), do not recursively instrument internal sanitizer/reporting builtins as user noreturn calls, and ensure builtin prototype parameter types match `gimple_call_builtin_p`.
4. Preprocessor Traps & Host/Target Library Portability:
   - In `match.pd`, ALWAYS use `#if GIMPLE` / `#if GENERIC`, NEVER `#ifdef GIMPLE` / `#ifdef GENERIC` (`genmatch` defines both `GIMPLE` and `GENERIC` to `0` or `1`, so `#ifdef GIMPLE` is ALWAYS true!).
   - Check for variables or helpers defined outside `#ifdef TARGET_*` / `#if ...` but only used inside conditional blocks, which break bootstrap under `-Werror=unused-variable` / `-Werror=unused-function`.
   - Check host C++14 and target library (`libstdc++-v3`, `libgomp`, `libgcc`) portability: lock-free `std::atomic` availability on 32-bit/non-lock-free targets, over-aligned `alignof` allocation in C++14 without custom `operator new`, and `[[__no_unique_address__]]` wrapper structs requiring an NSDMI `_Tp _M_v{};` when `_Tp` has an `explicit` default constructor."#;

const STAGE_VERIFICATION_INSTRUCTION: &str = r#"# Verification and severity estimation

You are the lead GCC reviewer consolidating `concerns` and `dismissed_concerns` generated by parallel specialized review stages.
Your task is to (1) deduplicate overlapping items across both lists while preserving exact bug boundaries, and (2) classify every unique candidate into one of two high-level categories:
- **Category 1: Well-Justified** — split into **1a. Well-Justified Concerns (`findings`)** and **1b. Well-Justified Dismissals (`dismissed_concerns`)**.
- **Category 2: Speculative or Contested (`hard_cases`)** — routed to parallel per-finding `post-verification` for deep codebase verification with tools.

### Step 1: Deduplication and Boundary Preservation
1. Group `concerns` and `dismissed_concerns` that refer to the same underlying root cause AND the same function/pass/pattern.
2. Do NOT merge distinct bugs in separate functions, passes, or `.md` patterns, or distinct failure mechanisms (e.g., a wrong-code missing `reset_flow_sensitive_info` vs. a null pointer dereference on `gimple_bb`) even within the same function—either keep them as separate items or explicitly name ALL distinct functions, passes, and failure mechanisms in the item's title/description and explanation fields (`problem` and `severity_explanation` for `findings`, or `description` and `concern_arguments` for `hard_cases`).
3. SPECIFICITY REQUIREMENT: When merging overlapping items, preserve and consolidate the most specific details: exact function/pattern names, file paths, line numbers when known, and ALL distinct triggering inputs, target architectures/flags, and consequences (e.g. `wrong-code`, `ICE`, `rejects-valid`, `accepts-invalid`, `-fcompare-debug` failure) mentioned across the merged items. Preserve and merge the `locations` arrays. Do not invent line numbers; use `null` when unknown.
4. Set `"preexisting": false` whenever the patch introduces, modifies, triggers, exposes, or relies on the buggy code path, caller/callee interaction, or pattern—OR when the patch is an **incomplete symmetric fix** (i.e., the patch fixes a bug or adds a check/feature in one function, operator, overload, or pass, and misses the corresponding sibling function, operator, overload, or pass in the same file/subsystem). Set `"preexisting": true` ONLY if the bug is a completely unrelated pre-existing defect in untouched code whose reachability, inputs, and behavior are unaffected by the patch.

### Step 2: Classification Rules (1: Well-Justified vs. 2: Speculative or Contested)
Classify every consolidated item using these strict signal rules. **Repetition is NOT justification: multiple overlapping items are never enough for Category 1 (1a or 1b) unless their reasoning is backed by specific, concrete code proof.**

1. **Category 1a — Well-Justified Concern (`findings` array):**
   - **Mandatory prerequisite & strong signals:** Concrete, self-contained code proof directly visible in the target diff, prefetched context, and cited `locations`, with **no** competing `dismissed_concern`, **no** reliance on unverified assumptions about unseen code, and **no** potential resolution by a follow-up patch in `=== Follow-Up Patches in Series ===`. Multiple deduplicated `concerns` with no attempts to dismiss is a strong signal for 1a **only when grounded in concrete code proof** (if multiple stages repeat an unproven assumption or vague claim without specific code proof, classify into `hard_cases` instead).
   - **Action:** Validate it directly and emit it in `findings`. Assign a calibrated `severity` (`Low`, `Medium`, `High`, or `Critical`) following `severity.md`, state all consequences (e.g. wrong-code miscompilation, ICE, rejects-valid, GGC/container UAF), triggering inputs/flags, and reachability at the start of `severity_explanation`, formulate a concise bug title (`problem`) under 80 characters starting with a GCC component prefix (e.g. `tree-optimization:`, `c++:`, `target:`, `middle-end:`, `rtl-optimization:`, `fortran:`, `ipa:`, `libstdc++:`, `c:`), set `"preexisting"`, and include `locations`.

2. **Category 1b — Well-Justified Dismissal (`dismissed_concerns` array):**
   - **Mandatory prerequisite & strong signals:** Concrete disproving `code_snippet` in `locations` (showing the exact local guard, type/mode check, `reset_flow_sensitive_info` call, or invariant in the same function/pattern that prevents the bug), with **no** competing `concern` and **no** reliance on unverified assumptions about external callers, callees, target modes, or compiler flags. Multiple overlapping `dismissed_concerns` for the same code is NOT a signal that the dismissal is safe—it indicates multiple analysts independently found the code suspicious; when multiple stages flag and dismiss the same non-trivial mechanism using assumptions about caller invariants, earlier passes, downstream recovery, or target constraints rather than a direct local guard in the same function, classify into `hard_cases` with `"signal_reason": "speculative_dismissal"`.
   - **Disqualifiers for Category 1b (NEVER classify as 1b):**
     - **Single-caller or happy-path-only proofs:** A dismissal that proves an invariant (such as non-null `tree`/`rtx`/`gimple_bb`, `INTEGER_CST` operand, `testing_p == false`, or matching `TYPE_PRECISION`) in only one caller or normal optimization level without verifying ALL callers and entry points in the tree (`git_grep` across all callers) is INVALID.
     - **Downstream-recovery, duplicate-call, asymmetry, or stale-state rationalizations:** A dismissal that argues an unrolled-back accumulator/state modification in a failed loop (such as `extra_off`), a `goto` jumping over variable initialization, a duplicate function call left behind after code motion, an incomplete symmetric fix in a sibling function/operator, an unhandled `UNKNOWN` return from `reversed_comparison_code`, missing `reset_flow_sensitive_info`, a function-wide `get_non_trapping` check satisfied by a conditional load in the guarded block itself, dropping `TYPE_QUAL_CONST` from a lambda `this` capture proxy, `TYPE_PRECISION` on vector types in `match.pd`, unshared `CONSTRUCTOR` with `RANGE_EXPR`, or mutating state during `testing_p` query mode is "harmless", "a safe fallback", "overwritten/ignored later", or "collected by GGC" is INVALID.
   - **Action:** Place in `dismissed_concerns` (dropped from further verification).
   - **CRITICAL INVARIANT:** Never place any item that was raised as a `concern` into `dismissed_concerns` in this stage. If a raised `concern` is contested by a `dismissed_concern` or appears questionable, it MUST be placed in `hard_cases` for tool-based `post-verification`.

3. **Category 2 — Speculative or Contested (`hard_cases` array — sent to parallel `post-verification`):**
   - **Strong signals:**
     - **Mixed signals (`"mixed_signals"`):** Similar or overlapping `concerns` and `dismissed_concerns` exist for the same root cause, function, or code path.
     - **Speculative or assumption-based dismissal (`"speculative_dismissal"`):** Even when NO stage raised a `concern`, inspect every standalone dismissal critically! If one or more `dismissed_concerns` identified a plausible compiler bug and dismissed it using a single-caller proof, an assumption not proven by the cited `code_snippet`, or a rationalization that unrolled-back tentative state, a duplicate call, an incomplete symmetric fix, stale range info, `UNKNOWN` comparison code, or `testing_p` state mutation is "harmless" or "ignored downstream", you MUST classify the item into `hard_cases` with `"signal_reason": "speculative_dismissal"`.
     - **Speculative or incomplete concern (`"speculative_concern"`):** One or more overlapping `concerns` whose argument relies on assumptions not based on specific code in the diff/locations (requiring tool inspection of callers, callees, macro definitions, or `.md` predicates/constraints), or mixes a partially inaccurate premise with a potentially real underlying bug in the same code path.
     - **Series interaction (`"series_interaction"`):** Any concern that could plausibly be resolved, wired up, or rewritten by a subsequent patch listed in `=== Follow-Up Patches in Series ===`.
   - **Action:** Emit into `hard_cases` with `"estimated_severity"` (`Critical`, `High`, `Medium`, or `Low`), `"signal_reason"`, `"concern_arguments"`, `"dismissal_arguments"`, a concrete `"verification_question"` specifying what code `post-verification` must inspect with tools, `"preexisting"`, and `"locations"`."#;

const STAGE_POST_VERIFICATION_INSTRUCTION: &str = r#"# Per-finding post-verification and conflict resolution

You are the lead GCC reviewer performing deep, tool-assisted codebase verification of a speculative or contested candidate issue (`hard_cases`) identified during initial verification.
1. **Targeted Tool Verification:** Use the available Git and file tools (`git_read_files`, `git_grep`, `git_diff`, `git_show`, `git_blame`) to answer each candidate's `verification_question` and inspect the actual repository code for both `concern_arguments` and `dismissal_arguments`.
2. **SYMMETRICAL PROOF BAR & ALL-CALLERS VERIFICATION:** Both `concern_arguments` and `dismissal_arguments` are untrusted hypotheses. To discard a candidate issue as a false positive, you MUST find concrete proof in the codebase that explicitly invalidates the failure mechanism across ALL callers, entry points, target modes, and compiler flags. Citing a single caller does NOT disprove a null dereference, `TREE_CHECK` failure, `wide_int` precision mismatch, or `testing_p` side effect in a helper function unless `git_grep` across all callers proves every caller upholds the invariant. Never discard an issue based on unverified assumptions about external callers, earlier passes, or target constraints.
3. **LOCAL BOUNDARY, ROLLBACK, FORGOTTEN REMOVAL & ASYMMETRY RULE:** Do not discard a defect within the modified code of the patch by assuming that surrounding passes or callers will mask or prevent the issue, or by rationalizing unrolled-back accumulator/state modifications in a failed loop, a duplicate/unremoved function call left behind after code motion, an incomplete symmetric fix in a sibling function/operator, missing `reset_flow_sensitive_info`, a function-wide `get_non_trapping` check satisfied by a conditional load inside the guarded block, dropping `TYPE_QUAL_CONST` from a lambda `this` capture proxy, `TYPE_PRECISION` on potentially vector types or `#ifdef GIMPLE` in `match.pd`, unhandled `UNKNOWN` return from `reversed_comparison_code`, `DECL_LANG_SPECIFIC` access without `retrofit_lang_decl`, `CONSTRUCTOR` aliasing with `RANGE_EXPR`, or state mutation when `testing_p` is true as "harmless", "a safe fallback", "overwritten/ignored later", or "cleaned up by GGC".
4. **PROMOTING SPECULATIVE DISMISSALS:** When `"signal_reason"` is `"speculative_dismissal"`, a previous analyst spotted the candidate bug described in `concern_arguments` and dismissed it using `dismissal_arguments`. Inspect the actual code with tools: if the dismissal's assumption is false or incomplete, you MUST report the bug as a verified finding in `findings`.
5. **REFINING PARTIALLY INACCURATE PREMISES:** If a candidate concern contains a partially inaccurate premise while also identifying a real bug in the same code path, refine and report the valid underlying bug rather than discarding the entire candidate.
6. **SERIES VALIDATION RULE:** If follow-up patches in this series are provided in the context, check whether each candidate issue is resolved, fixed, or rewritten in the final state of the series (`Series End Commit`) using tools (`git_read_files` or `git_diff` at `Series End Commit`); do not trust promises in commit messages. If resolved by the end of the series, discard it in `dismissed_concerns` citing the resolving commit.
7. **SEVERITY CALIBRATION AND COMPLETENESS:** Assign a severity (`Low`, `Medium`, `High`, or `Critical`) to each validated finding following `severity.md`: reason through all consequences (`wrong-code`, `ICE`, `rejects-valid`, `accepts-invalid`, GGC/container UAF, or `-fcompare-debug` divergence), triggering inputs/flags, and reachability, and state that reasoning at the start of `severity_explanation`. Formulate a concise bug title (`problem`) under 80 characters starting with a GCC component prefix (e.g. `tree-optimization:`, `c++:`, `target:`, `middle-end:`, `rtl-optimization:`, `fortran:`, `ipa:`, `libstdc++:`, `c:`). Preserve all distinct function/pattern names, file paths, line numbers when known, and consequences. Set `"preexisting": false` whenever the patch introduces, modifies, triggers, exposes, or relies on the buggy code path, or when the patch is an incomplete symmetric fix that misses a sibling function/operator in the same file; mark `"preexisting": true` ONLY if the bug is a completely unrelated pre-existing issue in untouched code."#;

pub const STAGE_REPORT_INSTRUCTION: &str = r#"# gcc-patches inline review report generation

You are an automated review bot generating a report for `gcc-patches@gcc.gnu.org`. Convert the provided JSON findings into a polite, technical, inline-commented plain-text email reply following `inline-template.md`.

Follow the formatting rules strictly. Do not use markdown headers or backticks. Ensure the tone is constructive, technical, and concise.

SPECIFICITY REQUIREMENT: Each inline comment MUST reference the exact function or pattern name, file, and specific triggering condition (such as input construct, optimization flag, or target mode). Prefer the finding's `locations` field when present. State precisely what goes wrong (e.g., wrong-code miscompilation, ICE in `tree_check_failed`, rejects-valid under SFINAE) and why.

PRE-EXISTING ISSUES: If any finding has `"preexisting": true`, include it in the report and state explicitly at the start of its comment that the problem was not introduced by this patch."#;

const STAGE_JSON_SCHEMA_EXAMPLE: &str = r#"Once you have gathered sufficient information, return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
If you find no concerns and no dismissed concerns, return {"concerns": [], "dismissed_concerns": []}.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
- "type": A short category string (e.g. "Wrong-Code / Stale SSA Info", "ICE / Null Dereference", "Rejects-Valid / SFINAE", "Resource / State Rollback Bug", "Type / Width Mismatch").
- "description": A clear description of the problem.
- "reasoning": A step-by-step explanation of how the bug is triggered.
- "preexisting": true ONLY if this bug is an unrelated pre-existing defect in untouched code completely unaffected by this patch; false if the issue was newly introduced, modified, triggered, or exposed by this patch, OR if this patch is an incomplete symmetric fix that updates one function/operator/pass and misses a sibling function/operator/pass in the same file.
- "locations": An array of objects, each containing "file", "function_or_symbol", "line", "code_snippet" and "why_this_location_matters".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations". They mean the same as above, except that "description" is the candidate concern that was investigated and disproved, "reasoning" is the evidence proving it does not apply, and "locations" MUST cite the concrete disproving code (the exact guard, check, or caller/callee implementation that proves the issue cannot occur — not merely repeating the suspected line from the diff).

Use the 'dismissed_concerns' array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence.

NO DISMISSAL WITHOUT VERIFIED PROOF: To place a candidate issue in 'dismissed_concerns' (or to discard a suspected issue), you MUST find concrete proof in the code ('file', 'function_or_symbol', 'line', and verbatim 'code_snippet' in 'locations') that explicitly invalidates the concern's reasoning. If the disproving code lives outside the diff (for example, in a caller, callee, macro, or `.md` predicate), you MUST verify that code first using tools ('git_read_files' or 'git_grep') and quote the verified disproving snippet in 'locations'. If you cannot find definitive code proof that the candidate issue is impossible, you MUST report it in 'concerns' (NOT 'dismissed_concerns') and make the condition explicit: if X is possible, then problem Y can occur.
- Citing a single caller does NOT disprove a null dereference, `TREE_CHECK` failure, `wide_int` precision mismatch, or `testing_p` side effect in a helper function unless every caller in the tree ('git_grep' across all callers) is verified to uphold the invariant; otherwise report it in 'concerns'.
- Never dismiss an unrolled-back accumulator/state modification in a loop that fails to match, a `goto` jumping over variable initialization, a duplicate function call left behind after code motion, an incomplete symmetric fix in a sibling function/operator, a missing `reset_flow_sensitive_info` call, an unchecked `UNKNOWN` return from `reversed_comparison_code`, or a state mutation during `testing_p` query mode by rationalizing that a downstream bailout, overwrite, or fallback makes it "harmless".

SPECIFICITY REQUIREMENT: When reporting a concern or dismissed_concern, cite exact function/pattern name(s), file path(s), and line number(s) when known. Do not invent line numbers; use null when exact values are unknown.

CRITICAL REVIEW DIRECTIVE: Do NOT dismiss concerns just because you assume the surrounding compiler passes or callers handle it perfectly. Do not be overly charitable to the code. Assume valid or invalid user source code can reach the compiler with arbitrary combinations of optimization and target flags.

Example Output:
```json
{
  "concerns": [
    {
      "type": "Wrong-Code / Stale SSA Info",
      "description": "Missing reset_flow_sensitive_info when moving statement in fold_stmt_x",
      "reasoning": "1. Statement defining lhs is moved from conditional bb to dominator bb.\n2. SSA_NAME_RANGE_INFO on lhs is not cleared, leaving a range valid only under the original branch condition.",
      "preexisting": false,
      "locations": [
        {
          "file": "gcc/tree-ssa-phiopt.cc",
          "function_or_symbol": "fold_stmt_x",
          "line": 420,
          "code_snippet": "gsi_move_before (&gsi_from, &gsi_to);",
          "why_this_location_matters": "Moves the SSA definition out of the guarded basic block without resetting flow-sensitive range info."
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "ICE / Null Dereference",
      "description": "Possible NULL dereference of gimple_bb (def_stmt) in check_def",
      "reasoning": "Verified that SSA_NAME_IS_DEFAULT_DEF (name) is checked on line 405 before accessing gimple_bb (def_stmt).",
      "locations": [
        {
          "file": "gcc/tree-ssa-phiopt.cc",
          "function_or_symbol": "check_def",
          "line": 405,
          "code_snippet": "if (SSA_NAME_IS_DEFAULT_DEF (name)) return false;",
          "why_this_location_matters": "Excludes default definitions whose GIMPLE_NOP has a NULL basic block."
        }
      ]
    }
  ]
}
```"#;

// ---------------------------------------------------------------------------
// Stage Table Definitions
// ---------------------------------------------------------------------------

pub static ANALYSIS_STAGES: &[AnalysisStage] = &[
    AnalysisStage {
        name: "goal",
        short: "Goal Analysis",
        instruction: STAGE_GOAL_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: true,
    },
    AnalysisStage {
        name: "implementation",
        short: "Implementation",
        instruction: STAGE_IMPLEMENTATION_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: true,
    },
    AnalysisStage {
        name: "execution-flow",
        short: "Execution Flow",
        instruction: STAGE_EXECUTION_FLOW_INSTRUCTION,
        guides: &["technical-patterns.md"],
        uses_commit_log: false,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "resources",
        short: "Resource & State",
        instruction: STAGE_RESOURCES_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "types-math",
        short: "Types & Math",
        instruction: STAGE_TYPES_MATH_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "state-invalidation",
        short: "State & Caching",
        instruction: STAGE_STATE_INVALIDATION_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "diagnostics-abi",
        short: "Diag & Portability",
        instruction: STAGE_DIAGNOSTICS_ABI_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
];

pub static VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "verification",
    short: "Verification",
    wants_series_context: true,
};

pub static POST_VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "post-verification",
    short: "Post-Verification",
    wants_series_context: true,
};

pub static REPORT: ConsolidationStage = ConsolidationStage {
    name: "report",
    short: "Report Generation",
    wants_series_context: false,
};

pub static CONSOLIDATION_STAGES: &[&ConsolidationStage] =
    &[&VERIFICATION, &POST_VERIFICATION, &REPORT];

fn series_context_placeholder(wants: bool) -> &'static str {
    if wants {
        SERIES_CONTEXT_PLACEHOLDER
    } else {
        ""
    }
}

fn with_series_context(
    template: PromptTemplate<GccPatchReviewState>,
    wants: bool,
) -> PromptTemplate<GccPatchReviewState> {
    if !wants {
        return template;
    }
    template.with_var("follow_up_series_section", |s: &GccPatchReviewState| {
        s.follow_up_series_context
            .as_ref()
            .map(|ctx| format!("\n\n{}", ctx))
            .unwrap_or_default()
    })
}

pub fn analysis_stage_by_name(name: &str) -> Option<&'static AnalysisStage> {
    let normalized = normalize_stage_name(name);
    ANALYSIS_STAGES.iter().find(|s| s.name == normalized)
}

pub fn consolidation_stage_by_name(name: &str) -> Option<&'static ConsolidationStage> {
    let normalized = normalize_stage_name(name);
    if POST_VERIFICATION_STAGE_NAMES.contains(&normalized.as_str()) {
        return Some(&POST_VERIFICATION);
    }
    CONSOLIDATION_STAGES
        .iter()
        .copied()
        .find(|s| s.name == normalized)
}

pub fn stage_short_label(name: &str) -> Option<&'static str> {
    if let Some(def) = analysis_stage_by_name(name) {
        return Some(def.short);
    }
    consolidation_stage_by_name(name).map(|s| s.short)
}

pub fn is_stage_exclusive_guide(name: &str) -> bool {
    ANALYSIS_STAGES
        .iter()
        .flat_map(|def| def.guides)
        .any(|guide| guide.rsplit('/').next() == Some(name))
}

pub fn is_known_stage(name: &str) -> bool {
    let normalized = normalize_stage_name(name);
    analysis_stage_by_name(&normalized).is_some()
        || consolidation_stage_by_name(&normalized).is_some()
        || matches!(normalized.as_str(), "pre-screen" | "planning")
}

// ---------------------------------------------------------------------------
// Validators and Helpers
// ---------------------------------------------------------------------------

fn validate_concerns_output(
    output: &StageConcernsOutput,
    _state: &GccPatchReviewState,
) -> Result<(), String> {
    for (idx, concern) in output.concerns.iter().enumerate() {
        let Some(obj) = concern.as_object() else {
            return Err(format!("concerns[{idx}] must be a JSON object."));
        };
        let has_type = obj
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_desc = obj
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = obj
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_type || !has_desc || !has_reasoning {
            return Err(format!(
                "concerns[{idx}] must have non-empty 'type', 'description', and 'reasoning' strings."
            ));
        }
        if !obj.get("preexisting").is_some_and(Value::is_boolean) {
            return Err(format!(
                "concerns[{idx}] must have a boolean 'preexisting' field (true or false)."
            ));
        }
        if !obj.get("locations").is_some_and(Value::is_array) {
            return Err(format!("concerns[{idx}] must have a 'locations' array."));
        }
    }

    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let Some(obj) = dismissed.as_object() else {
            return Err(format!("dismissed_concerns[{idx}] must be a JSON object."));
        };
        let has_type = obj
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_desc = obj
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = obj
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_type || !has_desc || !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must have non-empty 'type', 'description', and 'reasoning' strings."
            ));
        }
        if !has_valid_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and verbatim disproving 'code_snippet' proving the candidate concern cannot occur. If you do not have concrete code proof, move the candidate issue to 'concerns' instead."
            ));
        }
    }

    Ok(())
}

fn format_concerns_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' (each with 'type', non-empty 'description' and 'reasoning' strings, a boolean 'preexisting', and a 'locations' array) and 'dismissed_concerns' (each with 'type', non-empty 'description' and 'reasoning' strings, and a 'locations' array) arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        violation
    )
}

fn validate_inline_format(content: &str, _state: &GccPatchReviewState) -> Result<(), String> {
    if content.lines().any(|l| l.trim_start().starts_with("```")) {
        return Err(
            "The output contains Markdown code blocks ('```'). It must be plain text as per `inline-template.md`."
                .to_string(),
        );
    }
    if !content.lines().any(|l| l.trim_start().starts_with('>')) {
        return Err(
            "The output does not appear to quote any code or context using '>'. Please follow the quoting style in `inline-template.md`."
                .to_string(),
        );
    }
    let has_commit_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("commit "));
    if !has_commit_header {
        return Err(
            "The output is missing the 'commit <hash>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`."
                .to_string(),
        );
    }
    let has_author_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("author:"));
    if !has_author_header {
        return Err(
            "The output is missing the 'Author: <name>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`."
                .to_string(),
        );
    }
    let has_comments = content.lines().any(|l| {
        let trimmed = l.trim();
        if trimmed.is_empty() || trimmed.starts_with('>') {
            return false;
        }
        let lower = trimmed.to_lowercase();
        !lower.starts_with("commit ")
            && !lower.starts_with("author:")
            && !lower.starts_with("date:")
            && !lower.starts_with("link:")
    });
    if !has_comments {
        return Err(
            "The output appears to lack any comments or summary. You must include a summary and interspersed comments explaining the findings."
                .to_string(),
        );
    }
    Ok(())
}

fn format_inline_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Please fix the formatting to match the standard plain-text gcc-patches inline review format with proper headers and '> ' quoted context.",
        violation
    )
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

pub fn prescreen_stage() -> Stage<GccPatchReviewState, PrescreenOutput> {
    Stage::builder("pre-screen")
        .system_prompt(PromptTemplate::<GccPatchReviewState>::new(
            "You are an AI assistant preparing a GCC compiler patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"gimple-ssa.md\", \"match-fold.md\"]}\n```",
        ))
        .user_prompt(
            PromptTemplate::<GccPatchReviewState>::new(
                "<subsystem_guide_index>\n@include(\"subsystem/subsystem.md\")\n</subsystem_guide_index>\n\n<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &GccPatchReviewState| {
                s.target_commit_diff.clone()
            })
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
        .reduce(|state, out: PrescreenOutput| {
            let prompts: Vec<String> = out
                .selected_prompts
                .into_iter()
                .filter(|name| !is_stage_exclusive_guide(name))
                .filter(|name| sanitize_guide_name(name))
                .collect();
            state.selected_guides = prompts;
        })
        .build()
}

pub fn planning_stage() -> Stage<GccPatchReviewState, PlanningOutput> {
    let optional_stages: Vec<&'static str> = ANALYSIS_STAGES
        .iter()
        .filter(|d| d.optional)
        .map(|d| d.name)
        .collect();

    Stage::builder("planning")
        .system_prompt(gcc_system_prompt(true))
        .user_prompt(PromptTemplate::<GccPatchReviewState>::new(
            r#"Analyze the provided GCC patch and determine which of the following review stages (each covering an orthogonal class of compiler defects) are relevant and should be executed:
- resources: Resource management, object/state lifetimes, error-path cleanup, tentative state rollback, container reallocation/re-entrancy invalidation, GGC/GTY safety, and expression/RTX unsharing
- types-math: Type representations, bit-widths/precisions/modes, signed/unsigned overflow semantics, IEEE-754 floating-point rules, comparison reversal, and algebraic folding soundness
- state-invalidation: Cached/flow-sensitive metadata invalidation (e.g. SSA range/nonzero info), IR/context synchronization (update_stmt, virtual SSA, constexpr_ctx, retrofit_lang_decl), pass convergence/termination, and -fcompare-debug determinism
- diagnostics-abi: Diagnostic gating (C++ SFINAE complain & tf_error), target ABI/optab/register/ISA constraints, symbol visibility/linkage/OpenMP capture contracts, preprocessor traps, and host/target library portability

CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo or comment fix, you may omit some stages. Stages not listed above (goal, implementation, execution-flow) always run and should not be included in your answer.

You MUST respond with ONLY a JSON object, no other text. Use the names exactly as given above. Example:
```json
{"relevant_stages": ["resources", "types-math", "state-invalidation", "diagnostics-abi"]}
```"#,
        ))
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "relevant_stages": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": optional_stages,
                    }
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
            let mut stages: Vec<String> = ANALYSIS_STAGES
                .iter()
                .filter(|d| !d.optional)
                .map(|d| d.name.to_string())
                .collect();
            for raw_name in out.relevant_stages {
                if let Some(def) = analysis_stage_by_name(&raw_name) {
                    if def.optional && !stages.iter().any(|s| s == def.name) {
                        stages.push(def.name.to_string());
                    }
                } else {
                    tracing::warn!("Ignoring unknown planned GCC review stage {:?}", raw_name);
                }
            }
            state.planned_stages = stages;
        })
        .build()
}

fn analysis_stage(
    def: &'static AnalysisStage,
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<GccPatchReviewState>> {
    let guide_markers = if def.guides.is_empty() {
        String::new()
    } else {
        let mut s = String::from("\n\n<stage_guidelines>\n");
        for guide in def.guides {
            s.push_str(&format!("@include(\"{guide}\")\n"));
        }
        s.push_str("</stage_guidelines>");
        s
    };
    let mut user_template = PromptTemplate::<GccPatchReviewState>::new(format!(
        "{}{}\n\n{}{}",
        def.instruction,
        guide_markers,
        STAGE_JSON_SCHEMA_EXAMPLE,
        series_context_placeholder(def.wants_series_context)
    ));
    for guide in def.guides {
        user_template = user_template.include_file(*guide);
    }
    let user_template = with_series_context(user_template, def.wants_series_context);

    Box::new(
        Stage::builder(def.name)
            .system_prompt(gcc_system_prompt(def.uses_commit_log))
            .user_prompt(user_template)
            .output_format(
                OutputFormat::json()
                    .with_validator(validate_concerns_output)
                    .with_feedback_formatter(format_concerns_feedback),
            )
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                ..Default::default()
            })
            .reduce_with_outcome(
                move |state: &mut GccPatchReviewState,
                      out: StageConcernsOutput,
                      outcome: &crate::workflow::stage::StageOutcome| {
                    let prompts =
                        collect_stage_prompts(&state.selected_guides, def.guides, outcome);
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

pub fn resolve_analysis_stages_with_options(
    state: &GccPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<GccPatchReviewState>>> {
    let selected_stages: Vec<String> = if let Some(ref manual) = state.manual_stages {
        manual.clone()
    } else if !state.planned_stages.is_empty() {
        state.planned_stages.clone()
    } else {
        ANALYSIS_STAGES.iter().map(|d| d.name.to_string()).collect()
    };

    let mut stages = Vec::new();
    for name in selected_stages {
        match analysis_stage_by_name(&name) {
            Some(def) => stages.push(analysis_stage(def, max_turns, temperature)),
            None => tracing::warn!("Ignoring unknown GCC review stage {:?}", name),
        }
    }
    stages
}

pub fn verification_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<GccPatchReviewState, VerificationOutput> {
    let series_context = series_context_placeholder(VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<GccPatchReviewState>::new(format!(
            r#"{STAGE_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must have concrete evidence in the code that proves the concern is invalid. Never drop a raised concern into 'dismissed_concerns' in this stage: every consolidated concern must be placed either in 'findings' (if well-justified with concrete code proof and no competing dismissal) or in 'hard_cases' (if contested, speculative, or requiring tool verification). Also inspect every standalone dismissed_concern: if it dismissed a plausible bug using an unverified assumption, promote it into 'hard_cases' with '"signal_reason": "speculative_dismissal"'.{series_context}

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'findings', 'hard_cases', and 'dismissed_concerns' arrays.
- Each object in 'findings' (Category 1a: Well-Justified Concerns) MUST use the keys: "problem" (a short naming string under 80 characters starting with a GCC component prefix like 'tree-optimization:', 'c++:', 'target:', 'middle-end:', 'rtl-optimization:', 'fortran:', 'ipa:', 'libstdc++:', 'c:', NEVER using backquotes, using fn_name() format for functions, describing the root cause), "severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), and "locations" (array of location objects), and may include "stages" (array of stage names) and "prompts" (array of prompt files).
- Each object in 'hard_cases' (Category 2: Speculative or Contested) MUST use the keys: "type", "description", "estimated_severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "signal_reason" ("mixed_signals", "speculative_concern", "speculative_dismissal", "series_interaction", or "other"), "concern_arguments" (consolidated arguments for why the bug can occur), "dismissal_arguments" (consolidated arguments/snippets from any competing or standalone dismissal, or "" if none), "verification_question" (the specific code question post-verification must answer with tools), "preexisting" (boolean), and "locations" (array of location objects), and may include "stages" and "prompts".
- Each object in 'dismissed_concerns' (Category 1b: Well-Justified Dismissals) MUST use the keys: "type", "description", "reasoning", and "locations", and may include "stages" and "prompts"."#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(|s: &GccPatchReviewState| {
            extra_prompt_paths_for_state(s, analysis_stage_by_name)
        }),
        VERIFICATION.wants_series_context,
    )
    .with_var("aggregated_concerns", |s: &GccPatchReviewState| {
        serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
    })
    .with_var(
        "aggregated_dismissed_concerns",
        |s: &GccPatchReviewState| {
            serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
        },
    );

    Stage::builder(VERIFICATION.name)
        .system_prompt(gcc_system_prompt(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(validate_verification_stage_output)
                .with_feedback_formatter(format_verification_stage_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce_with_outcome(|state, mut out: VerificationOutput, outcome| {
            enrich_verification_output(state, &mut out, outcome, analysis_stage_by_name);
            record_verified_findings(state, out.findings);
            state.hard_cases = out.hard_cases;
            state
                .deduplicated_dismissed_concerns
                .extend(out.dismissed_concerns);
        })
        .build()
}

pub fn post_verification_stage(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
) -> Stage<GccPatchReviewState, PostVerificationOutput> {
    let expected_items = batch.len().max(1);
    let candidate_json = serde_json::to_string_pretty(&batch).unwrap_or_default();
    let batch_for_prompts = batch.clone();
    let batch_for_reduce = batch;
    let series_context = series_context_placeholder(POST_VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<GccPatchReviewState>::new(format!(
            r#"{STAGE_POST_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a candidate issue as a false positive, you must find concrete evidence in the code that proves the issue is invalid (e.g., verifying with tools that all callers or callees prevent the exact failure mode) and quote that disproving code in `dismissed_concerns[].locations`. If you cannot find concrete proof of safety, you must validate and report the finding in `findings`.{series_context}

Candidate Hard Case(s) to Verify:
{{{{candidate_hard_cases}}}}

Return ONLY a JSON object with 'findings' and 'dismissed_concerns' arrays. Every candidate in this batch MUST be accounted for in either 'findings' (if validated) or 'dismissed_concerns' (ONLY if concrete code disproves the candidate; never return both empty arrays).
- Each object in 'findings' MUST use: "problem" (a short naming string under 80 characters starting with a GCC component prefix like 'tree-optimization:', 'c++:', 'target:', 'middle-end:', 'rtl-optimization:', 'fortran:', 'ipa:', 'libstdc++:', 'c:', NEVER using backquotes, using fn_name() format for functions, describing the root cause), "severity" (Low, Medium, High, Critical, or Unknown), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), and "locations" (array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters).
- Each object in 'dismissed_concerns' MUST use: "description" (the candidate issue that was disproved), "reasoning" (step-by-step explanation of how the inspected code disproves the candidate), and "locations" (a non-empty array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters, quoting the verbatim disproving guard, check, or caller/callee implementation)."#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(move |s: &GccPatchReviewState| {
            extra_prompt_paths_for_items(&s.selected_guides, &batch_for_prompts, analysis_stage_by_name)
        }),
        POST_VERIFICATION.wants_series_context,
    )
    .with_var("candidate_hard_cases", move |_: &GccPatchReviewState| {
        candidate_json.clone()
    });

    Stage::builder(stage_name)
        .system_prompt(gcc_system_prompt(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(move |out, _state| {
                    validate_post_verification_batch_output(out, expected_items)
                })
                .with_feedback_formatter(format_post_verification_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce_with_outcome(move |state, mut out: PostVerificationOutput, outcome| {
            enrich_post_verification_output(
                &state.selected_guides,
                &batch_for_reduce,
                &mut out,
                outcome,
                analysis_stage_by_name,
            );
            record_verified_findings(state, out.findings);
            state
                .deduplicated_dismissed_concerns
                .extend(out.dismissed_concerns);
        })
        .build()
}

pub fn post_verification_stage_for_batch(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<GccPatchReviewState>> {
    Box::new(post_verification_stage(
        stage_name,
        batch,
        max_turns,
        temperature,
    ))
}

pub fn resolve_post_verification_stages_with_options(
    state: &GccPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<GccPatchReviewState>>> {
    let batches = batch_hard_cases_by_severity(&state.hard_cases);
    batches
        .into_iter()
        .enumerate()
        .map(|(idx, batch)| {
            let stage_name = POST_VERIFICATION_STAGE_NAMES[idx];
            post_verification_stage_for_batch(stage_name, batch, max_turns, temperature)
        })
        .collect()
}

pub fn report_stage(max_turns: usize, temperature: f32) -> Stage<GccPatchReviewState, String> {
    Stage::builder(REPORT.name)
        .system_prompt(gcc_system_prompt(true))
        .user_prompt(
            PromptTemplate::<GccPatchReviewState>::new(format!(
                r#"{STAGE_REPORT_INSTRUCTION}

<report_template>
@include("inline-template.md")
</report_template>

Findings:
{{{{findings}}}}

Return raw text output, not JSON."#
            ))
            .include_file("inline-template.md")
            .with_var("findings", |s: &GccPatchReviewState| {
                serde_json::to_string_pretty(&s.findings).unwrap_or_default()
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
                reminder: "CRITICAL OVERRIDE: Your previous response was blocked by the API recitation filter for quoting large blocks of the patch diff verbatim. Do NOT quote full diff hunks or multi-line code blocks. Instead, start with the Commit/Author/Subject headers, write the summary, and for each finding include only a single short '> ' context line (1-2 lines maximum) to anchor the location before describing the issue in plain prose.".to_string(),
            },
            ..Default::default()
        })
        .skip_if(|s| s.skip_report)
        .reduce(|state, out: String| {
            state.review_inline = out;
        })
        .build()
}

// ---------------------------------------------------------------------------
// Complete GCC Review Workflow Graph
// ---------------------------------------------------------------------------

pub fn build_gcc_patch_review_workflow() -> Workflow<GccPatchReviewState> {
    build_gcc_patch_review_workflow_with_options(20, 0.0)
}

pub fn build_gcc_patch_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<GccPatchReviewState> {
    let stage_max_turns = max_turns.min(20);
    Workflow::builder("gcc_patch_review")
        .stage(prescreen_stage())
        .dynamic_parallel(
            planning_stage(),
            move |state| resolve_analysis_stages_with_options(state, stage_max_turns, temperature),
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty() && s.all_dismissed_concerns.is_empty(),
            "No concerns or dismissed concerns raised in initial analysis stages",
        )
        .dynamic_parallel(
            verification_stage(stage_max_turns, temperature),
            move |state| {
                resolve_post_verification_stages_with_options(state, stage_max_turns, temperature)
            },
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.findings.is_empty(),
            "No findings validated in verification stages",
        )
        .stage(report_stage(stage_max_turns, temperature))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gcc_analysis_stages_table() {
        assert_eq!(ANALYSIS_STAGES.len(), 7);
        let names: Vec<&str> = ANALYSIS_STAGES.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "goal",
                "implementation",
                "execution-flow",
                "resources",
                "types-math",
                "state-invalidation",
                "diagnostics-abi",
            ]
        );

        assert!(!analysis_stage_by_name("goal").unwrap().optional);
        assert!(!analysis_stage_by_name("implementation").unwrap().optional);
        assert!(!analysis_stage_by_name("execution-flow").unwrap().optional);

        assert!(analysis_stage_by_name("resources").unwrap().optional);
        assert!(analysis_stage_by_name("types-math").unwrap().optional);
        assert!(
            analysis_stage_by_name("state-invalidation")
                .unwrap()
                .optional
        );
        assert!(analysis_stage_by_name("diagnostics-abi").unwrap().optional);

        assert!(analysis_stage_by_name("goal").unwrap().uses_commit_log);
        assert!(
            analysis_stage_by_name("implementation")
                .unwrap()
                .uses_commit_log
        );
        for stage in [
            "execution-flow",
            "resources",
            "types-math",
            "state-invalidation",
            "diagnostics-abi",
        ] {
            assert!(
                !analysis_stage_by_name(stage).unwrap().uses_commit_log,
                "mechanics stage {stage} must not use commit log"
            );
        }
    }

    #[test]
    fn test_gcc_stage_lookup_and_labels() {
        assert_eq!(stage_short_label("resources"), Some("Resource & State"));
        assert_eq!(stage_short_label("types_math"), Some("Types & Math"));
        assert_eq!(
            stage_short_label("state-invalidation"),
            Some("State & Caching")
        );
        assert_eq!(
            stage_short_label("diagnostics-abi"),
            Some("Diag & Portability")
        );
        assert_eq!(
            stage_short_label("post-verification-2"),
            Some("Post-Verification")
        );
        assert!(is_known_stage("pre-screen"));
        assert!(is_known_stage("planning"));
        assert!(is_known_stage("resources"));
        assert!(is_known_stage("types-math"));
        assert!(is_known_stage("state-invalidation"));
        assert!(is_known_stage("diagnostics-abi"));
        assert!(is_known_stage("verification"));
        assert!(is_known_stage("post-verification-1"));
        assert!(is_known_stage("report"));
        assert!(!is_known_stage("hardware"));
    }

    #[test]
    fn test_gcc_subsystem_guides_not_filtered_by_stage_exclusive_check() {
        assert!(is_stage_exclusive_guide("technical-patterns.md"));
        for guide in [
            "gimple-ssa.md",
            "match-fold.md",
            "rtl-backend.md",
            "c-cpp-frontend.md",
            "fortran-frontend.md",
            "ipa-lto.md",
            "ggc-memory.md",
            "libstdcxx.md",
        ] {
            assert!(
                !is_stage_exclusive_guide(guide),
                "subsystem guide {guide} must not be treated as stage-exclusive so pre-screen can inject it into all stages"
            );
        }
    }

    #[test]
    fn test_validate_inline_format_accepts_minimal_quote_fallback() {
        let state = GccPatchReviewState::default();
        let valid_report = "\
commit 0123456789abcdef
Author: Test Author <test@example.com>

tree-optimization: fix range propagation

This patch causes an ICE when def_stmt has a null basic block.

> +  basic_block bb = gimple_bb (def_stmt);

In check_def(), gimple_bb(def_stmt) returns NULL for default definitions.
";
        assert!(validate_inline_format(valid_report, &state).is_ok());
    }

    #[test]
    fn test_validate_concerns_output_enforces_required_fields() {
        let state = GccPatchReviewState::default();
        let missing_type = StageConcernsOutput {
            concerns: vec![json!({
                "description": "Missing reset_flow_sensitive_info",
                "reasoning": "Moved statement out of conditional bb",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![],
        };
        assert!(validate_concerns_output(&missing_type, &state).is_err());

        let valid = StageConcernsOutput {
            concerns: vec![json!({
                "type": "Wrong-Code / Stale SSA Info",
                "description": "Missing reset_flow_sensitive_info",
                "reasoning": "Moved statement out of conditional bb",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![json!({
                "type": "ICE / Null Dereference",
                "description": "Possible NULL gimple_bb",
                "reasoning": "Checked SSA_NAME_IS_DEFAULT_DEF first",
                "locations": [{
                    "file": "gcc/tree-ssa-phiopt.cc",
                    "function_or_symbol": "check_def",
                    "line": 405,
                    "code_snippet": "if (SSA_NAME_IS_DEFAULT_DEF (name)) return false;",
                    "why_this_location_matters": "Excludes default defs"
                }]
            })],
        };
        assert!(validate_concerns_output(&valid, &state).is_ok());
    }

    #[test]
    fn test_gcc_verification_and_post_verification_reducers_preserve_existing_state() {
        let ver_stage = verification_stage(10, 0.0);
        let mut state = GccPatchReviewState {
            findings: vec![json!({
                "problem": "tree-optimization: earlier finding",
                "severity": "High",
                "preexisting": false,
            })],
            concerns: vec![json!({
                "type": "Pre-existing Issue",
                "description": "Earlier pre-existing issue",
                "preexisting": true,
            })],
            deduplicated_dismissed_concerns: vec![json!({
                "description": "Earlier dismissed concern",
                "reasoning": "Already proved safe",
            })],
            ..Default::default()
        };

        let ver_output = VerificationOutput {
            findings: vec![
                json!({
                    "problem": "c++: new verified finding",
                    "severity": "High",
                    "preexisting": false,
                }),
                json!({
                    "problem": "fortran: verified pre-existing",
                    "severity": "Medium",
                    "severity_explanation": "Unrelated untouched defect",
                    "preexisting": true,
                }),
            ],
            hard_cases: vec![],
            dismissed_concerns: vec![json!({
                "description": "Verified dismissal",
                "reasoning": "Guarded locally",
            })],
        };

        (ver_stage.reducer)(&mut state, ver_output);

        assert_eq!(state.findings.len(), 2);
        assert_eq!(
            state.findings[0]["problem"],
            "tree-optimization: earlier finding"
        );
        assert_eq!(state.findings[1]["problem"], "c++: new verified finding");
        assert_eq!(state.concerns.len(), 2);
        assert_eq!(
            state.concerns[0]["description"],
            "Earlier pre-existing issue"
        );
        assert_eq!(
            state.concerns[1]["description"],
            "fortran: verified pre-existing"
        );
        assert_eq!(state.deduplicated_dismissed_concerns.len(), 2);

        let batch = vec![json!({
            "type": "Wrong-Code",
            "description": "Candidate issue in match.pd",
            "estimated_severity": "Critical",
        })];
        let post_stage = post_verification_stage("post-verification-1", batch, 10, 0.0);
        let post_output = PostVerificationOutput {
            findings: vec![json!({
                "problem": "middle-end: new post-verified finding",
                "severity": "Critical",
                "preexisting": false,
            })],
            dismissed_concerns: vec![json!({
                "description": "Disproved hard case",
                "reasoning": "Checked TYPE_OVERFLOW_WRAPS",
            })],
        };

        (post_stage.reducer)(&mut state, post_output);

        assert_eq!(state.findings.len(), 3);
        assert_eq!(
            state.findings[2]["problem"],
            "middle-end: new post-verified finding"
        );
        assert_eq!(state.deduplicated_dismissed_concerns.len(), 3);
        assert_eq!(
            state.deduplicated_dismissed_concerns[2]["description"],
            "Disproved hard case"
        );
    }
}
