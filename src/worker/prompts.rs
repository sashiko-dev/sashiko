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

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::fs;

/// System identity prompt - used across all AI interactions
pub const SYSTEM_IDENTITY: &str = "You're an expert Linux kernel developer and upstream maintainer with deep knowledge of Linux kernel, Operating Systems, CPU architectures, modern hardware and Linux kernel community standards and processes.";

pub const OUTPUT_FORMAT_INSTRUCTION: &str = r#"Important: `review_inline` field of the final output *MUST* follow the format and guidelines provided in `inline-template.md`.

When you are completely finished with your investigation and have no more tools to call, output the final result strictly as a JSON object matching this schema:
```json
{
  "summary": "High-level summary of the original change being reviewed.",
  "review_inline": "The full content of the inline review (formatted according to inline-template.md). This MUST be populated if there are any findings.",
  "findings": [
    {
      "severity": "Low|Medium|High|Critical",
      "severity_explanation": "Concise explanation (e.g. 'memory leak on a hot path')",
      "problem": "Description of the problem",
      "suggestion": "Suggested fix"
    }
  ]
}
```"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStage {
    /// Stage 1: Hypothesis Generation. Brainstorm potential failure modes.
    Exploration,
    /// Stage 2: Research & Verification. Prove or disprove hypotheses.
    Verification,
    /// Stage 3: Severity & Final Report. Consolidate and grade findings.
    Reporting,
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

    /// Builds the complete knowledge base string for initial loading.
    pub async fn build_context(&self) -> Result<String> {
        let mut content = String::with_capacity(50_000);

        // 1. System Identity
        content.push_str(SYSTEM_IDENTITY);
        content.push_str("\n\n");

        // 2. Core Philosophy
        self.append_file(&mut content, "review-philosophy.md")
            .await?;

        // 3. Optional Index (for discovery)
        let subsystem_dir = self.base_dir.join("subsystem");
        if subsystem_dir.exists() {
            self.append_file(&mut content, "subsystem/README.md")
                .await?;
        }

        Ok(content)
    }

    /// Returns the initial user message to start the task.
    pub async fn get_user_task_prompt(
        &self,
        use_cache: bool,
        series_range: Option<String>,
    ) -> Result<String> {
        let trigger = if use_cache {
            "Refer to the `# review-philosophy.md` section in the pre-loaded context and run a deep dive regression analysis of the top commit in the Linux source tree. Do NOT attempt to load any additional prompts yet."
        } else {
            "Load the `# review-philosophy.md` and run a deep dive regression analysis of the top commit in the Linux source tree."
        };

        let mut prompt = trigger.to_string();

        if let Some(range) = series_range {
            prompt.push_str(&format!(
                "\n\nImportant: Check every finding on being present at the end of the series. Use the `git_checkout` tool to check out the desired commit. If the bug was fixed within the series (range: {}), it should not be reported in the final report. Do not skip this check!",
                range
            ));
        }

        prompt.push_str("\n\n");
        prompt.push_str(&self.get_exploration_instructions());

        Ok(prompt)
    }

    pub fn get_exploration_instructions(&self) -> String {
        "Brainstorm every possible way this code could fail. Focus on edge cases, race conditions, and boundary violations. 
    Don't read any files yet. I want you to figure be bold and focused. Give me all possible ways this code could break the kernel.

    Output your hypotheses as a JSON string using the `cmd_submit_results` tool.

    Expected JSON Structure:
    {
    \"hypotheses\": [
    {
      \"id\": 1,
      \"problem_description\": \"...\",
      \"potential_impact\": \"...\"
    }
    ],
    \"exploration_complete\": true
    }".to_string()
    }

    pub async fn get_verification_instructions(&self) -> Result<String> {
        let mut content = "That is great.
    Now, using your available tools, systematically verify each hypothesis from the brainstorming phase. Build your context, read the appropriate files, disregard hypothesis that have no base, and explore those that do.

    So, I would like you to:
    1. For each hypothesis: Trace the execution flow and provide proof if it is a real regression.
    2. If you discover new, unexpected issues during research, include them in the `verifications` list.
    3. Once research is exhausted, set `verification_complete` to true and submit via `cmd_submit_results` (expected structure below).

    USE THESE PATTERNS FOR VERIFICATION:\n".to_string();

        self.append_file(&mut content, "technical-patterns.md")
            .await?;

        content.push_str(
            "\n\nExpected JSON Structure:
    {
    \"verifications\": [
    {
      \"evidence\": \"Detailed code proof or execution trace...\",
      \"suggestion\": \"The potential fix...\",
      \"is_confirmed\": true,
      \"hypothesis_id\": 1
    }
    ],
    \"verification_complete\": true
    }",
        );
        Ok(content)
    }

    pub async fn get_reporting_instructions(&self) -> Result<String> {
        let mut content = "Excellent! Finally, consolidate your confirmed findings into a final report. 

    Instructions:
    1. Apply the provided `severity.md` escalation protocol to each confirmed regression.
    2. Use BOTTOM-UP REASONING: For each finding, document the technical problem and suggestion BEFORE assigning the severity label.
    3. Generate the final summary AFTER you have processed all findings.
    4. Provide the `review_inline` text following the `inline-template.md` guidelines.
    5. Submit the final result via `cmd_submit_results`.

    REPORTING GUIDELINES:\n".to_string();

        self.append_file(&mut content, "severity.md").await?;
        self.append_file(&mut content, "inline-template.md").await?;

        content.push_str(
            "\n\nExpected JSON Structure:
    {
    \"findings\": [
    {
      \"problem\": \"Technical description...\",
      \"suggestion\": \"Fix...\",
      \"severity_explanation\": \"Why it meets the escalation gate...\",
      \"severity\": \"Low/Medium/High/Critical\"
    }
    ],
    \"summary\": \"Overall high-level summary...\",
    \"review_inline\": \"Full formatted inline review text...\"
    }",
        );
        Ok(content)
    }

    async fn append_file(&self, buffer: &mut String, filename: &str) -> Result<()> {
        let path = self.base_dir.join(filename);
        if path.exists() {
            buffer.push_str(&format!("# {}\n", filename));
            buffer.push_str(
                &fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("Failed to read {}", filename))?,
            );
            buffer.push_str("\n\n");
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
        if let Some(tools) = tools {
            if let Ok(json) = serde_json::to_string(tools) {
                hasher.update(json);
            }
        }
        format!("{:x}", hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_identity_constant() {
        let identity = PromptRegistry::get_system_identity();
        assert!(identity.starts_with("You're an expert Linux kernel developer"));
        assert!(identity.contains("maintainer"));
    }

    #[test]
    fn test_content_hash_deterministic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());

        let content = "test content";
        let hash1 = registry.calculate_content_hash::<()>(content, None);
        let hash2 = registry.calculate_content_hash::<()>(content, None);

        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64);
    }

    #[tokio::test]
    async fn test_build_context_includes_identity() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let context = registry.build_context().await.unwrap();

        assert!(context.contains(SYSTEM_IDENTITY));
    }

    #[tokio::test]
    async fn test_build_context_includes_philosophy() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("review-philosophy.md"),
            "# review-philosophy.md\nSoul",
        )
        .unwrap();

        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let context = registry.build_context().await.unwrap();

        assert!(context.contains("# review-philosophy.md"));
        assert!(context.contains("Soul"));
    }

    #[tokio::test]
    async fn test_user_task_prompt_cached() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let prompt = registry.get_user_task_prompt(true, None).await.unwrap();

        assert!(prompt.contains("Refer to the `# review-philosophy.md` section"));
        assert!(prompt.contains("Brainstorm every possible way"));
    }

    #[tokio::test]
    async fn test_user_task_prompt_non_cached() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let prompt = registry.get_user_task_prompt(false, None).await.unwrap();

        assert!(prompt.contains("Load the `# review-philosophy.md`"));
        assert!(prompt.contains("Brainstorm every possible way"));
    }

    #[tokio::test]
    async fn test_user_task_prompt_with_series_range() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let prompt = registry
            .get_user_task_prompt(true, Some("base..head".to_string()))
            .await
            .unwrap();

        assert!(prompt.contains("base..head"));
        assert!(prompt.contains("git_checkout"));
    }

    #[tokio::test]
    async fn test_verification_instructions_include_patterns() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("technical-patterns.md"),
            "# Patterns\nEH-001",
        )
        .unwrap();

        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let instructions = registry.get_verification_instructions().await.unwrap();

        assert!(instructions.contains("systematically verify each hypothesis"));
        assert!(instructions.contains("# Patterns"));
        assert!(instructions.contains("EH-001"));
    }

    #[tokio::test]
    async fn test_reporting_instructions_include_guidelines() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("severity.md"), "# Severity").unwrap();
        std::fs::write(temp_dir.path().join("inline-template.md"), "# Template").unwrap();

        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let instructions = registry.get_reporting_instructions().await.unwrap();

        assert!(instructions.contains("consolidate your confirmed findings"));
        assert!(instructions.contains("# Severity"));
        assert!(instructions.contains("# Template"));
    }
}
