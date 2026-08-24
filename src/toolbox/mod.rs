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

use crate::ai::AiTool;
use crate::toolbox::framework::ToolRegistry;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub mod framework;
pub mod utils;

pub mod git_blame;
pub mod git_diff;
pub mod git_find_files;
pub mod git_grep;
pub mod git_log;
pub mod git_ls;
pub mod git_read_files;
pub mod git_show;
pub mod read_prompt;

/// The Sashiko-specific context passed to LLM tools.
///
/// It encapsulates the active worktree, currently reviewed files, the virtual head commit,
/// and a shared cache to avoid redundant command executions across tool runs.
pub struct SashikoToolContext {
    pub worktree_path: PathBuf,
    pub prompts_path: Option<PathBuf>,
    pub active_patch_files: RwLock<Vec<String>>,
    pub virtual_head: RwLock<Option<String>>,
    pub(crate) cache: Arc<RwLock<std::collections::HashMap<String, Value>>>,
}

impl SashikoToolContext {
    /// Replaces occurrences of `HEAD` in a reference string with the virtualized head commit SHA.
    pub fn virtualize_ref(&self, r: &str) -> String {
        let vhead_lock = self.virtual_head.read().unwrap();
        let Some(ref vhead) = *vhead_lock else {
            return r.to_string();
        };
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"(^|[^/])\bHEAD($|[~^:.@])").unwrap());
        re.replace_all(r, format!("${{1}}{}${{2}}", vhead))
            .into_owned()
    }
}

/// A backward-compatible adapter that coordinates Sashiko's LLM tools.
///
/// It wraps the generic `ToolRegistry` and manages the shared execution context and caching.
pub struct ToolBox {
    context: SashikoToolContext,
    registry: ToolRegistry<SashikoToolContext>,
    /// Thread-safe cache of tool invocation results.
    /// Shared with the execution context so that tools can access it internally.
    pub(crate) cache: Arc<RwLock<std::collections::HashMap<String, Value>>>,
}

impl ToolBox {
    /// Creates a new `ToolBox` configured for the given worktree and optional prompt registry.
    pub fn new(worktree_path: PathBuf, prompts_path: Option<PathBuf>) -> Self {
        let cache = Arc::new(RwLock::new(std::collections::HashMap::new()));

        let context = SashikoToolContext {
            worktree_path,
            prompts_path,
            active_patch_files: RwLock::new(Vec::new()),
            virtual_head: RwLock::new(None),
            cache: cache.clone(),
        };

        let mut registry = ToolRegistry::new();
        registry.register(git_read_files::GitReadFilesTool);
        registry.register(git_blame::GitBlameTool);
        registry.register(git_diff::GitDiffTool);
        registry.register(git_show::GitShowTool);
        registry.register(git_log::GitLogTool);
        registry.register(git_ls::GitLsTool);
        registry.register(git_grep::GitGrepTool);
        registry.register(git_find_files::GitFindFilesTool);

        if context.prompts_path.is_some() {
            registry.register(read_prompt::ReadPromptTool);
        }

        Self {
            context,
            registry,
            cache,
        }
    }

    /// Registers an extra tool. Test-only: the review toolbox is fixed, and
    /// this exists so a test can drive the tool loop with a tool of its own.
    #[cfg(test)]
    pub(crate) fn register_tool(
        &mut self,
        tool: impl framework::LlmTool<SashikoToolContext> + 'static,
    ) {
        self.registry.register(tool);
    }

    /// Sets the virtual head commit SHA for the current review session.
    pub fn set_virtual_head(&mut self, sha: String) {
        let mut vhead = self.context.virtual_head.write().unwrap();
        *vhead = Some(sha);
    }

    /// Sets the list of files modified by the patch currently under review.
    pub fn set_active_patch_files(&mut self, files: Vec<String>) {
        let mut active = self.context.active_patch_files.write().unwrap();
        *active = files;
    }

    /// Replaces occurrences of HEAD in a reference string with the virtualized head commit SHA.
    pub fn virtualize_ref(&self, r: &str) -> String {
        self.context.virtualize_ref(r)
    }

    /// Returns the absolute path to the worktree where tools are executed.
    pub fn get_worktree_path(&self) -> &Path {
        &self.context.worktree_path
    }

    /// Generates LLM-facing declarations for all registered tools.
    pub fn get_declarations_generic(&self) -> Vec<AiTool> {
        self.registry
            .declarations()
            .into_iter()
            .map(|decl| AiTool {
                name: decl["name"].as_str().unwrap().to_string(),
                description: decl["description"].as_str().unwrap().to_string(),
                parameters: decl["parameters"].clone(),
            })
            .collect()
    }

    /// Invokes a tool by name with the given JSON arguments.
    ///
    /// It handles argument normalization, caching of final results, and dispatches
    /// the execution to the corresponding tool struct.
    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let name_normalized = name.trim().to_lowercase();
        let should_cache = name_normalized != "todowrite";

        let normalized_args = self.registry.normalize_tool_args(&name_normalized, &args);

        let key = if should_cache {
            let k = format!(
                "{}:{}",
                name_normalized,
                serde_json::to_string(&normalized_args)?
            );
            {
                let cache = self.cache.read().unwrap();
                if let Some(val) = cache.get(&k) {
                    return Ok(val.clone());
                }
            }
            Some(k)
        } else {
            None
        };

        let res = self
            .registry
            .call(&name_normalized, args, &self.context)
            .await?;

        if let Some(k) = key {
            let mut cache = self.cache.write().unwrap();
            cache.insert(k, res.clone());
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tools_test;
