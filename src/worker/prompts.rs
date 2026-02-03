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

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::fs;

/// System identity prompt - used across all AI interactions
pub const SYSTEM_IDENTITY: &str = "You're an expert Linux kernel developer and upstream maintainer with deep knowledge of Linux kernel, Operating Systems, CPU architectures, modern hardware and Linux kernel community standards and processes.";

/// Files to exclude from context building
const EXCLUDED_FILES: &[&str] = &[
    "review-core.md",
    "technical-patterns.md",
    "README.md",
    "review-one.md",
    "review-stat.md",
    "debugging.md",
    "lore-thread.md",
];

pub struct PromptRegistry {
    base_dir: PathBuf,
}

impl PromptRegistry {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn get_base_dir(&self) -> &PathBuf {
        &self.base_dir
    }

    /// Returns the system identity prompt
    pub fn get_system_identity() -> &'static str {
        SYSTEM_IDENTITY
    }

    /// Reads the review-core.md protocol file
    pub async fn get_review_core(&self) -> Result<String> {
        let core_path = self.base_dir.join("review-core.md");
        if core_path.exists() {
            Ok(fs::read_to_string(&core_path).await?)
        } else {
            Ok("Deep dive regression analysis protocol.".to_string())
        }
    }

    /// Builds the user task prompt for AI review
    ///
    /// When `use_cache` is true, the protocol is assumed to be in the cache,
    /// so we don't include review-core.md content.
    pub async fn get_user_task_prompt(&self, use_cache: bool) -> Result<String> {
        if use_cache {
            Ok(format!(
                "{}\nRun a deep dive regression analysis of the top commit in the Linux source tree.\n\n\
                 Follow the Review Protocol and all Technical patterns and Subsystem Guidelines available in your context.\n\
                 IMPORTANT: Don't try to load additional prompts using tools, even if guided otherwise, they all are preloaded in your context.\n\
                 IMPORTANT: If you find regressions, you MUST use the `write_file` tool to create `review-inline.txt` as specified in the protocol.",
                SYSTEM_IDENTITY
            ))
        } else {
            let review_core = self.get_review_core().await?;
            Ok(format!(
                "{} Using the prompt review-prompts/kernel/review-core.md run a deep dive regression analysis of the top commit in the Linux source tree.\n\n\
                 ## Review Protocol (review-core.md)\n\
                 {}\n\n\
                 IMPORTANT: If you find regressions, you MUST use the `write_file` tool to create `review-inline.txt` as specified in the protocol.",
                SYSTEM_IDENTITY, review_core
            ))
        }
    }

    /// Builds full context string from prompts directory for caching
    ///
    /// Follows specific inclusion order:
    /// 1. System Identity
    /// 2. review-core.md
    /// 3. technical-patterns.md
    /// 4. All *.md in base_dir (excluding specific files)
    /// 5. All *.md in patterns/
    /// 6. All *.md in nfsd/
    pub async fn build_context(&self) -> Result<String> {
        let mut context = String::new();

        // 1. System identity
        context.push_str(SYSTEM_IDENTITY);
        context.push_str("\n\n");

        // 2. Review protocol (review-core.md)
        context.push_str("# review-code.md\n\n");
        let core_path = self.base_dir.join("review-core.md");
        if core_path.exists() {
            context.push_str(&fs::read_to_string(&core_path).await?);
            context.push_str("\n\n");
        }

        // 3. Technical patterns (technical-patterns.md)
        let tech_path = self.base_dir.join("technical-patterns.md");
        if tech_path.exists() {
            context.push_str("## technical-patterns.md\n");
            context.push_str(&fs::read_to_string(&tech_path).await?);
            context.push_str("\n\n");
        }

        // 4. Subsystem guidelines (root md files)
        context.push_str("# Subsystem Guidelines\n\n");

        let mut entries = fs::read_dir(&self.base_dir).await?;
        let mut paths = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            paths.push(entry.path());
        }
        paths.sort(); // Deterministic order

        for path in paths {
            if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
                let fname = path.file_name().unwrap().to_string_lossy();
                if EXCLUDED_FILES.contains(&fname.as_ref()) {
                    continue;
                }
                context.push_str(&format!("## {}\n", fname));
                context.push_str(&fs::read_to_string(&path).await?);
                context.push_str("\n\n");
            }
        }

        // 5. Technical patterns (patterns/ subdirectory)
        let patterns_dir = self.base_dir.join("patterns");
        if patterns_dir.exists() {
            context.push_str("# Technical Patterns\n\n");
            let mut p_entries = fs::read_dir(&patterns_dir).await?;
            let mut p_paths = Vec::new();
            while let Some(entry) = p_entries.next_entry().await? {
                p_paths.push(entry.path());
            }
            p_paths.sort();

            for path in p_paths {
                if path.extension().is_some_and(|ext| ext == "md") {
                    context.push_str(&format!(
                        "## patterns/{}\n",
                        path.file_name().unwrap().to_string_lossy()
                    ));
                    context.push_str(&fs::read_to_string(&path).await?);
                    context.push_str("\n\n");
                }
            }
        }

        // 6. NFSD guidelines (nfsd/ subdirectory)
        let nfsd_dir = self.base_dir.join("nfsd");
        if nfsd_dir.exists() {
            context.push_str("# NFSD Guidelines\n\n");
            let mut n_entries = fs::read_dir(&nfsd_dir).await?;
            let mut n_paths = Vec::new();
            while let Some(entry) = n_entries.next_entry().await? {
                n_paths.push(entry.path());
            }
            n_paths.sort();

            for path in n_paths {
                if path.extension().is_some_and(|ext| ext == "md") {
                    context.push_str(&format!(
                        "## nfsd/{}\n",
                        path.file_name().unwrap().to_string_lossy()
                    ));
                    context.push_str(&fs::read_to_string(&path).await?);
                    context.push_str("\n\n");
                }
            }
        }

        Ok(context)
    }

    /// Calculates SHA256 hash of content, optionally including tools signature
    pub fn calculate_content_hash<T: serde::Serialize>(
        &self,
        content: &str,
        tools: Option<&[T]>,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);

        // Also hash tools signature if present, so we rotate cache if tools change
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

        assert!(context.contains("# Test Protocol"));
        assert!(context.contains("This is a test."));
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

        assert!(prompt.contains(SYSTEM_IDENTITY));
        assert!(prompt.contains("regression analysis"));
        assert!(!prompt.contains("## Review Protocol")); // No embedded protocol in cached mode
    }

    #[tokio::test]
    async fn test_user_task_prompt_non_cached() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("review-core.md"), "Protocol content").unwrap();

        let registry = PromptRegistry::new(temp_dir.path().to_path_buf());
        let prompt = registry.get_user_task_prompt(false).await.unwrap();

        assert!(prompt.contains(SYSTEM_IDENTITY));
        assert!(prompt.contains("## Review Protocol"));
        assert!(prompt.contains("Protocol content"));
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
}
