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
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::process::Command;

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
    /// Optional allowlist of enabled tool names (lowercase). `None` means all enabled.
    enabled_tools: Option<Vec<String>>,
    /// Custom tool definitions registered from settings.
    custom_tools: Vec<(AiTool, crate::settings::CustomToolDefinition)>,
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
            enabled_tools: None,
            custom_tools: Vec::new(),
        }
    }

    /// Creates a new `ToolBox` with tool filtering configuration from settings.
    pub fn with_config(
        worktree_path: PathBuf,
        prompts_path: Option<PathBuf>,
        tools_config: Option<&crate::settings::ToolsSettings>,
    ) -> Self {
        let enabled_tools = tools_config.map(|config| {
            if !config.enabled.is_empty() {
                // Allowlist mode: use enabled list, subtract disabled
                config
                    .enabled
                    .iter()
                    .filter(|name| !config.disabled.contains(name))
                    .map(|s| s.to_lowercase())
                    .collect()
            } else {
                // All tools enabled, subtract disabled
                let all_tools = vec![
                    "git_read_files",
                    "git_blame",
                    "git_diff",
                    "git_show",
                    "git_log",
                    "git_ls",
                    "git_grep",
                    "git_find_files",
                    "read_prompt",
                ];
                all_tools
                    .into_iter()
                    .filter(|name| !config.disabled.iter().any(|d| d.to_lowercase() == *name))
                    .map(|s| s.to_string())
                    .collect()
            }
        });

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

        let mut toolbox = Self {
            context,
            registry,
            cache,
            enabled_tools,
            custom_tools: Vec::new(),
        };

        // Register custom tools if provided
        if let Some(config) = tools_config
            && let Err(e) = toolbox.register_custom_tools(&config.custom)
        {
            tracing::warn!("Failed to register custom tools: {}", e);
        }

        toolbox
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

    /// Check if a tool is enabled based on configuration.
    fn is_tool_enabled(&self, tool_name: &str) -> bool {
        // Custom tools are always enabled (they have their own security validation)
        if self
            .custom_tools
            .iter()
            .any(|(ai_tool, _)| ai_tool.name == tool_name)
        {
            return true;
        }

        match &self.enabled_tools {
            Some(enabled) => enabled.contains(&tool_name.to_lowercase()),
            None => true, // All tools enabled by default
        }
    }

    /// Register custom tools from configuration.
    fn register_custom_tools(
        &mut self,
        custom_tool_defs: &[crate::settings::CustomToolDefinition],
    ) -> Result<()> {
        for tool_def in custom_tool_defs {
            self.validate_tool_security(tool_def)?;

            let schema: serde_json::Value =
                serde_json::from_str(&tool_def.parameters).map_err(|e| {
                    anyhow!(
                        "Invalid parameter schema for tool '{}': {}",
                        tool_def.name,
                        e
                    )
                })?;

            let ai_tool = AiTool {
                name: tool_def.name.clone(),
                description: tool_def.description.clone(),
                parameters: schema,
            };

            self.custom_tools.push((ai_tool, tool_def.clone()));
        }

        Ok(())
    }

    /// Validate custom tool security.
    fn validate_tool_security(
        &self,
        tool_def: &crate::settings::CustomToolDefinition,
    ) -> Result<()> {
        let dangerous_patterns = ["rm -rf", "sudo", "curl", "wget", "dd ", "mkfs"];
        for pattern in &dangerous_patterns {
            if tool_def.command.contains(pattern) {
                anyhow::bail!(
                    "Potentially dangerous command in custom tool '{}': contains '{}'",
                    tool_def.name,
                    pattern
                );
            }
        }

        if !tool_def.allowed_paths.is_empty() {
            let worktree = &self.context.worktree_path;
            for path in &tool_def.allowed_paths {
                let full_path = worktree.join(path);
                if !full_path.starts_with(worktree) {
                    anyhow::bail!(
                        "Custom tool '{}' path escapes worktree: {}",
                        tool_def.name,
                        path
                    );
                }
            }
        }

        Ok(())
    }

    /// Execute a custom tool command.
    async fn execute_custom_tool(
        &self,
        tool_def: &crate::settings::CustomToolDefinition,
        args: &serde_json::Value,
    ) -> Result<String> {
        let mut command = tool_def.command.clone();

        if let Some(obj) = args.as_object() {
            for (key, value) in obj {
                let placeholder = format!("{{{}}}", key);
                let value_str = match value {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(arr) => arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => value.to_string(),
                };
                command = command.replace(&placeholder, &value_str);
            }
        }

        // Validate path parameters against allowlist
        if !tool_def.allowed_paths.is_empty()
            && let Some(obj) = args.as_object()
        {
            for (key, value) in obj {
                if key.contains("path") || key.contains("file") {
                    let path_str = match value {
                        serde_json::Value::String(s) => s.as_str(),
                        _ => continue,
                    };

                    let is_allowed = tool_def
                        .allowed_paths
                        .iter()
                        .any(|allowed| path_str.starts_with(allowed));

                    if !is_allowed {
                        anyhow::bail!(
                            "Path '{}' not allowed for custom tool '{}'. Allowed paths: {:?}",
                            path_str,
                            tool_def.name,
                            tool_def.allowed_paths
                        );
                    }
                }
            }
        }

        let output = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .current_dir(&self.context.worktree_path)
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!(
                "Custom tool '{}' failed: {}",
                tool_def.name,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Generates LLM-facing declarations for all registered tools.
    pub fn get_declarations_generic(&self) -> Vec<AiTool> {
        let mut decls: Vec<AiTool> = self
            .registry
            .declarations()
            .into_iter()
            .map(|decl| AiTool {
                name: decl["name"].as_str().unwrap().to_string(),
                description: decl["description"].as_str().unwrap().to_string(),
                parameters: decl["parameters"].clone(),
            })
            .collect();

        // Add custom tools
        for (ai_tool, _) in &self.custom_tools {
            decls.push(ai_tool.clone());
        }

        // Filter declarations based on enabled_tools configuration
        decls
            .into_iter()
            .filter(|tool| self.is_tool_enabled(&tool.name))
            .collect()
    }

    /// Invokes a tool by name with the given JSON arguments.
    ///
    /// It handles argument normalization, caching of final results, and dispatches
    /// the execution to the corresponding tool struct.
    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let name_normalized = name.trim().to_lowercase();

        // Check if it's a custom tool first
        if let Some((_, tool_def)) = self
            .custom_tools
            .iter()
            .find(|(ai_tool, _)| ai_tool.name.to_lowercase() == name_normalized)
        {
            let result = self.execute_custom_tool(tool_def, &args).await?;
            return Ok(json!({ "output": result }));
        }

        // Check if tool is enabled
        if !self.is_tool_enabled(&name_normalized) {
            return Err(anyhow!("Tool '{}' is not enabled", name));
        }

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
