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

pub const OUTPUT_FORMAT_INSTRUCTION: &str = "Important: If you have ANY findings, you *MUST* produce the `review-inline.txt` file. This file *MUST* follow the format and guidelines provided in `inline-template.md`. Once you generated requested files, produce the final JSON response.";

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

        // 3. Subsystem Guidelines (root *.md files)
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
            )
        })
        .await?;

        // 4. Specific Pattern Directories
        self.append_directory(&mut content, &self.base_dir.join("patterns"), |_| true)
            .await?;
        self.append_directory(&mut content, &self.base_dir.join("nfsd"), |_| true)
            .await?;

        // 5. Task Instructions
        content.push_str("\n\n");
        content.push_str(OUTPUT_FORMAT_INSTRUCTION);

        Ok(content)
    }

    /// Returns the initial user message to start the task.
    /// - `use_cache`: If true, assumes `build_context` is already in the cache.
    pub async fn get_user_task_prompt(&self, use_cache: bool) -> Result<String> {
        if use_cache {
            // In cached mode, the context (Identity + Instructions + Protocol) is already pre-loaded.
            // We provide a minimal trigger that references the specific section.
            Ok("Refer to the `# review-core.md` section in the pre-loaded context and run a deep dive regression analysis as described in the protocol of the top commit in the Linux source tree. Do NOT attempt to load any additional prompts.".to_string())
        } else {
            // In non-cached mode, we must provide the Output Instructions explicitly,
            // followed by the trigger to load the protocol.
            let trigger = "Load the protocol from `review-core.md` and run a deep dive regression analysis as described in the protocol of the top commit in the Linux source tree. You also must load the `inline-template.md` and `severity.md` prompts.";
            Ok(format!("{}\n\n{}", OUTPUT_FORMAT_INSTRUCTION, trigger))
        }
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

        let prompt = registry.get_user_task_prompt(true).await.unwrap();

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
        let prompt = registry.get_user_task_prompt(false).await.unwrap();

        assert!(!prompt.contains(SYSTEM_IDENTITY));
        assert!(prompt.contains("Load the protocol from `review-core.md`"));
        assert!(!prompt.contains("Refer to the protocol in the pre-loaded context"));
        assert!(prompt.contains("Important: If you have ANY findings"));
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
    async fn test_build_context_includes_instruction() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let context = registry.build_context().await.unwrap();
        assert!(context.contains("Important: If you have ANY findings"));
        // Ensure JSON schema is NOT present
        assert!(!context.contains("Your final response must be a valid JSON object"));
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
}
