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

/// Output format guidance appended to analysis stage prompts.
pub const ANALYSIS_FORMAT_GUIDANCE: &str = include_str!("prompts/analysis_format_guidance.md");

/// Stage 12 (synthesis): final cherry-pick review report.
pub const CONFLICT_REPORT: &str = include_str!("prompts/conflict_report.md");

/// Stage 2 (analysis): detection of changes dropped during resolution.
pub const DROPPED_CHANGES: &str = include_str!("prompts/dropped_changes.md");

/// Framing header for merge-conflict resolution review context.
pub const MERGE_CONFLICT_REVIEW_FRAMING: &str =
    include_str!("prompts/merge_conflict_review_framing.md");

/// Stage 3 (analysis): structural merge-correctness verification.
pub const MERGE_CORRECTNESS: &str = include_str!("prompts/merge_correctness.md");

/// Origin classification: label each finding resolution_introduced /
/// original_patch_preexisting / base_preexisting before filtering.
pub const ORIGIN_CLASSIFICATION: &str = include_str!("prompts/origin_classification.md");

/// Planning stage instructions for cherry-pick review.
pub const PLANNING: &str = include_str!("prompts/planning.md");

/// Header for prefetched AST context block.
pub const PREFETCHED_CONTEXT_HEADER: &str = include_str!("prompts/prefetched_context_header.md");

/// Stage 1 (analysis): semantic-intent preservation of the resolution.
pub const SEMANTIC_INTENT: &str = include_str!("prompts/semantic_intent.md");

/// Stage 10 (synthesis): verification and severity estimation.
pub const VERIFICATION: &str = include_str!("prompts/verification.md");
