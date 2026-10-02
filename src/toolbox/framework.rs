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
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// A trait representing a single tool that can be exposed to an LLM.
///
/// The parameter `C` represents the execution context that is passed to the tool
/// during invocation (e.g., environment variables, file system paths, or project state).
#[async_trait]
pub trait LlmTool<C>: Send + Sync {
    /// The unique name of the tool (e.g., "git_grep").
    fn name(&self) -> &'static str;

    /// A detailed description of what the tool does, used by the LLM to understand when to call it.
    fn description(&self) -> &'static str;

    /// The JSON Schema defining the parameters this tool accepts.
    fn parameters(&self) -> Value;

    /// Normalizes the arguments passed by the LLM (e.g., filling in default values)
    /// to maximize cache hits. The default implementation returns the arguments unmodified.
    fn normalize_args(&self, args: &Value) -> Value {
        args.clone()
    }

    /// Executes the tool with the given arguments and context.
    async fn call(&self, args: Value, context: &C) -> Result<Value>;
}

/// A registry that manages a set of LLM tools and handles dynamic dispatching.
pub struct ToolRegistry<C> {
    tools: HashMap<&'static str, Arc<dyn LlmTool<C>>>,
}

impl<C> ToolRegistry<C> {
    /// Creates a new empty `ToolRegistry`.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Registers a tool in the registry.
    pub fn register(&mut self, tool: impl LlmTool<C> + 'static) {
        self.tools.insert(tool.name(), Arc::new(tool));
    }

    /// Registers a boxed tool in the registry.
    pub fn register_boxed(&mut self, tool: Box<dyn LlmTool<C>>) {
        self.tools.insert(tool.name(), Arc::from(tool));
    }

    /// Returns the list of tool declarations in a format suitable for the LLM API.
    ///
    /// The output is a vector of JSON objects, each containing:
    /// - `name`: The tool's name
    /// - `description`: Its description
    /// - `parameters`: Its parameter schema
    pub fn declarations(&self) -> Vec<Value> {
        // Sorted by name. The map's iteration order is random for every
        // registry, and this list goes into every request, so without the
        // sort two identical requests differ between runs and retries.
        let mut tools: Vec<_> = self.tools.values().collect();
        tools.sort_by_key(|t| t.name());
        tools
            .into_iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters(),
                })
            })
            .collect()
    }

    /// Normalizes the arguments for a specific tool by name.
    ///
    /// If the tool is registered, it delegates to the tool's `normalize_args` method.
    /// Otherwise, it returns the arguments unmodified.
    pub fn normalize_tool_args(&self, name: &str, args: &Value) -> Value {
        if let Some(tool) = self.tools.get(name) {
            tool.normalize_args(args)
        } else {
            args.clone()
        }
    }

    /// Dispatches a tool call to the registered tool with the given name.
    ///
    /// Returns an error if no tool is registered under the given name.
    pub async fn call(&self, name: &str, args: Value, context: &C) -> Result<Value> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Tool not found in registry: {}", name))?;

        tool.call(args, context).await
    }
}

impl<C> Default for ToolRegistry<C> {
    fn default() -> Self {
        Self::new()
    }
}
