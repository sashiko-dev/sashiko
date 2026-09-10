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

//! Policies governing stage limits, tool availability, retries, and errors.

/// Defines which tools are exposed to the LLM during a stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolScope {
    /// No tools are exposed (e.g. for pure synthesis or summary stages).
    None,
    /// All tools configured in the active ToolBox are enabled.
    All,
    /// Only specific tool names are enabled.
    Selected(Vec<String>),
}

/// Policy for handling failures when executing parallel stages concurrently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ParallelPolicy {
    /// If any parallel stage fails, abort the entire parallel batch immediately.
    #[default]
    FailFast,
    /// Continue running remaining parallel stages, logging warnings for failed ones.
    BestEffort,
}

/// Policy for handling provider recitation / safety filter errors.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum RecitationPolicy {
    /// Abort the stage immediately on recitation error.
    #[default]
    Fail,
    /// Retry once after appending a reminder to avoid quoting patch code verbatim.
    RetryWithReminder(String),
    /// Switch the stage into a free-form summary mode and retry with a custom reminder.
    FallbackToFreeForm { reminder: String },
}

/// Execution policies, limits, and retry configuration for a stage.
#[derive(Clone, Debug)]
pub struct StagePolicy {
    /// Maximum conversational turns allowed for this stage.
    pub max_turns: usize,
    /// Maximum validation retries on invalid output format.
    pub max_validation_attempts: usize,
    /// Sampling temperature for the model.
    pub temperature: f32,
    /// Tools exposed to the LLM for this stage.
    pub tools: ToolScope,
    /// Policy for handling recitation errors.
    pub recitation_policy: RecitationPolicy,
}

impl Default for StagePolicy {
    fn default() -> Self {
        Self {
            max_turns: 15,
            max_validation_attempts: 3,
            temperature: 0.0,
            tools: ToolScope::All,
            recitation_policy: RecitationPolicy::RetryWithReminder(
                "IMPORTANT: Your previous response was blocked by a recitation filter. \
                 To bypass this filter, please rephrase your response: do NOT quote large blocks \
                 of code verbatim. Describe code logic in your own words, summarize findings, or \
                 quote only short snippets (1-2 lines). Re-emit your JSON output now."
                    .to_string(),
            ),
        }
    }
}
