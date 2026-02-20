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
use std::path::{Path, PathBuf};
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

    /// Builds the complete knowledge base string.
    /// This is used for:
    /// 1. Populating the Context Cache.
    /// 2. Constructing the full prompt in non-cached mode.
    pub async fn build_context(&self) -> Result<String> {
        let mut content = String::with_capacity(50_000);

        // 1. System Identity
        content.push_str(SYSTEM_IDENTITY);
        content.push_str("\n\n");

        // 2. Core Protocol & Patterns
        self.append_file(&mut content, "review-core.md").await?;
        self.append_file(&mut content, "inline-template.md").await?;
        self.append_file(&mut content, "technical-patterns.md")
            .await?;
        self.append_file(&mut content, "severity.md").await?;

        // 3. Subsystem Guidelines
        let subsystem_dir = self.base_dir.join("subsystem");
        if subsystem_dir.exists() {
            // New Structure (e.g. Kernel)
            self.append_directory(&mut content, &subsystem_dir, |name| {
                !matches!(name, "README.md" | "subsystem-template.md")
            })
            .await?;

            // Explicitly load nfsd from subsystem if it exists
            self.append_directory(&mut content, &subsystem_dir.join("nfsd"), |_| true)
                .await?;
        } else {
            // Old Structure / Systemd (root-based)
            self.append_directory(&mut content, &self.base_dir, |name| {
                !matches!(
                    name,
                    "review-core.md"
                        | "inline-template.md"
                        | "technical-patterns.md"
                        | "README.md"
                        | "review-one.md"
                        | "review-stat.md"
                        | "debugging.md"
                        | "lore-thread.md"
                        | "severity.md"
                )
            })
            .await?;
        }

        // 4. Specific Pattern Directories
        self.append_directory(&mut content, &self.base_dir.join("patterns"), |_| true)
            .await?;

        // Check root nfsd only if not using subsystem structure
        if !subsystem_dir.exists() {
            self.append_directory(&mut content, &self.base_dir.join("nfsd"), |_| true)
                .await?;
        }

        Ok(content)
    }

    /// Returns the initial user message to start the task.
    /// - `use_cache`: If true, assumes `build_context` is already in the cache.
    /// - `series_range`: Optional git range of the series (e.g. "base..head") to check for fixes.
    pub async fn get_user_task_prompt(
        &self,
        use_cache: bool,
        series_range: Option<String>,
    ) -> Result<String> {
        let trigger = if use_cache {
            "Refer to the `# review-core.md` section in the pre-loaded context and run a deep dive regression analysis as described in the protocol of the top commit in the Linux source tree. Do NOT attempt to load any additional prompts.

STAGE 1: EXPLORATION.
Brainstorm every possible way this code could fail. Focus on edge cases, race conditions, and boundary violations. 
Output your findings in JSON format according to the provided schema. 
If you suspect no regressions, return an empty `hypotheses` array and set `exploration_complete` to true."
        } else {
            "Load the protocol from `review-core.md` and run a deep dive regression analysis as described in the protocol of the top commit in the Linux source tree. You also must load the `inline-template.md` and `severity.md` prompts.

STAGE 1: EXPLORATION.
Brainstorm every possible way this code could fail. Focus on edge cases, race conditions, and boundary violations. 
Output your findings in JSON format according to the provided schema. 
If you suspect no regressions, return an empty `hypotheses` array and set `exploration_complete` to true."
        };

        let mut prompt = trigger.to_string();

        if let Some(range) = series_range {
            prompt.push_str(&format!(
                "\n\nImportant: Check every finding on being present at the end of the series. Use the `git_checkout` tool to check out the desired commit. If the bug was fixed within the series (range: {}), it should not be reported in the final report. Do not skip this check!",
                range
            ));
        }

        Ok(prompt)
    }

    pub fn get_stage_verification_prompt(&self) -> String {
        "STAGE 2: VERIFICATION.
Now, using your available tools, systematically verify each hypothesis from the brainstorming phase. 
For each: 
1. Trace the execution flow. 
2. Provide proof if it is a real regression. 
3. If it is impossible, explain why.

Important: If you discover new, unexpected issues during your research, you MUST include them as additional findings in the `verifications` list.
Once you have exhausted your research, set `verification_complete` to true.".to_string()
    }

    pub fn get_stage_reporting_prompt(&self) -> String {
        format!(
            "STAGE 3: REPORTING.
Review your confirmed findings from the verification stage. 
Apply the `severity.md` escalation protocol to each confirmed regression. 
Provide the mandatory justification for the assigned severity. 
Finally, generate the final JSON output following the provided schema.

{}",
            OUTPUT_FORMAT_INSTRUCTION
        )
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

    async fn append_directory<F>(&self, buffer: &mut String, dir: &Path, filter: F) -> Result<()>
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
            if path.extension().is_some_and(|ext| ext == "md") {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if filter(name) {
                        paths.push(path);
                    }
                }
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
        assert_eq!(hash1.len(), 64); // SHA256 hex is 64 chars
    }

    #[test]
    fn test_content_hash_differs_with_tools() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());

        let content = "test content";
        let tools = vec!["tool1", "tool2"];

        let hash_no_tools = registry.calculate_content_hash::<()>(content, None);
        let hash_with_tools = registry.calculate_content_hash(content, Some(&tools));

        assert_ne!(hash_no_tools, hash_with_tools);
    }

    #[tokio::test]
    async fn test_build_context_includes_identity() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());

        let context = registry.build_context().await.unwrap();
        assert!(context.starts_with(SYSTEM_IDENTITY));
    }

    #[tokio::test]
    async fn test_build_context_includes_core() {
        let temp_dir = tempfile::tempdir().unwrap();
        let core_content = "# Test Protocol\nThis is a test.";
        std::fs::write(temp_dir.path().join("review-core.md"), core_content).unwrap();

        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let context = registry.build_context().await.unwrap();

        assert!(context.contains("# review-core.md"));
        assert!(context.contains("# Test Protocol"));
    }

    #[tokio::test]
    async fn test_build_context_excludes_readme_and_others() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("README.md"),
            "# README\nDo not include",
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join("debugging.md"),
            "# Debugging\nDo not include",
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join("subsystem.md"),
            "# Subsystem\nInclude me",
        )
        .unwrap();

        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let context = registry.build_context().await.unwrap();

        assert!(!context.contains("Do not include"));
        assert!(context.contains("Include me"));
    }

    #[tokio::test]
    async fn test_user_task_prompt_cached() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());

        let prompt = registry.get_user_task_prompt(true, None).await.unwrap();

        // In cached mode, the prompt is minimal and relies on pre-loaded context.
        assert!(!prompt.contains(SYSTEM_IDENTITY));
        assert!(prompt.contains("Refer to the `# review-core.md` section"));
        assert!(prompt.contains("Do NOT attempt to load any additional prompts"));
    }

    #[tokio::test]
    async fn test_user_task_prompt_non_cached() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("review-core.md"), "Protocol content").unwrap();

        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let prompt = registry.get_user_task_prompt(false, None).await.unwrap();

        assert!(!prompt.contains(SYSTEM_IDENTITY));
        assert!(prompt.contains("Load the protocol from `review-core.md`"));
        assert!(!prompt.contains("Refer to the protocol in the pre-loaded context"));
        assert!(prompt.contains("STAGE 1: EXPLORATION"));
    }

    #[tokio::test]
    async fn test_user_task_prompt_with_series_range() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());

        let range = "base..head";
        let prompt = registry
            .get_user_task_prompt(true, Some(range.to_string()))
            .await
            .unwrap();

        assert!(prompt.contains(range));
        assert!(prompt.contains("Check every finding on being present at the end of the series"));
    }

    #[tokio::test]
    async fn test_build_context_structure() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();

        // 1. Root files (excluding ignored)
        std::fs::write(root.join("root_sub.md"), "Root Content").unwrap();
        std::fs::write(root.join("debugging.md"), "Ignored Debugging").unwrap();

        // 2. Patterns directory
        let patterns_dir = root.join("patterns");
        std::fs::create_dir(&patterns_dir).unwrap();
        std::fs::write(patterns_dir.join("pat1.md"), "Pattern Content").unwrap();

        // 3. NFSD directory
        let nfsd_dir = root.join("nfsd");
        std::fs::create_dir(&nfsd_dir).unwrap();
        std::fs::write(nfsd_dir.join("nfsd1.md"), "NFSD Content").unwrap();

        // 4. Random subdirectory (should be ignored)
        let other_dir = root.join("other_sub");
        std::fs::create_dir(&other_dir).unwrap();
        std::fs::write(other_dir.join("other.md"), "Ignored Subdir Content").unwrap();

        let registry = PromptRegistry::new(root.to_path_buf());
        let context = registry.build_context().await.unwrap();

        // Verify inclusions
        assert!(context.contains("Root Content"));
        assert!(context.contains("## patterns/pat1.md"));
        assert!(context.contains("Pattern Content"));
        assert!(context.contains("## nfsd/nfsd1.md"));
        assert!(context.contains("NFSD Content"));

        // Verify exclusions
        assert!(!context.contains("Ignored Debugging"));
        assert!(!context.contains("Ignored Subdir Content"));
    }

    #[tokio::test]
    async fn test_build_context_includes_inline_template_after_review_core() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();
        std::fs::write(root.join("review-core.md"), "CORE CONTENT").unwrap();
        std::fs::write(root.join("inline-template.md"), "TEMPLATE CONTENT").unwrap();
        std::fs::write(root.join("technical-patterns.md"), "PATTERNS CONTENT").unwrap();

        let registry = PromptRegistry::new(root.to_path_buf());
        let context = registry.build_context().await.unwrap();

        let core_idx = context.find("CORE CONTENT").unwrap();
        let template_idx = context.find("TEMPLATE CONTENT").unwrap();
        let patterns_idx = context.find("PATTERNS CONTENT").unwrap();

        assert!(core_idx < template_idx);
        assert!(template_idx < patterns_idx);
    }

    #[tokio::test]
    async fn test_build_context_excludes_subsystem_template() {
        let temp_dir = tempfile::tempdir().unwrap();
        let subsystem_dir = temp_dir.path().join("subsystem");
        std::fs::create_dir(&subsystem_dir).unwrap();

        std::fs::write(subsystem_dir.join("real.md"), "Real Content").unwrap();
        std::fs::write(
            subsystem_dir.join("subsystem-template.md"),
            "Template Content",
        )
        .unwrap();

        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let context = registry.build_context().await.unwrap();

        assert!(context.contains("Real Content"));
        assert!(!context.contains("Template Content"));
    }
}
