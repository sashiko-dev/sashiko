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

//! Workflow graph definitions, step types, and builder combinators.

use serde::de::DeserializeOwned;

use super::policy::ParallelPolicy;
use super::stage::{ExecutableStage, Stage};

/// A resolver function that inspects workflow state and constructs dynamic stages.
pub type StageResolver<S> = Box<dyn Fn(&S) -> Vec<Box<dyn ExecutableStage<S>>> + Send + Sync>;

/// A step in a workflow execution graph.
pub enum WorkflowStep<S: Send + Sync> {
    /// Execute a single stage to completion.
    Stage(Box<dyn ExecutableStage<S>>),

    /// Execute multiple stages concurrently.
    Parallel {
        stages: Vec<Box<dyn ExecutableStage<S>>>,
        policy: ParallelPolicy,
    },

    /// Run a dynamic planning stage, then resolve and execute stages concurrently.
    DynamicParallel {
        planner: Box<dyn ExecutableStage<S>>,
        resolver: StageResolver<S>,
        policy: ParallelPolicy,
    },

    /// Conditional branching.
    Branch {
        condition: Box<dyn Fn(&S) -> bool + Send + Sync>,
        then_flow: Workflow<S>,
        else_flow: Option<Workflow<S>>,
    },

    /// Early exit condition: stops workflow immediately if condition evaluates to true.
    EarlyExitIf {
        condition: Box<dyn Fn(&S) -> bool + Send + Sync>,
        reason: &'static str,
    },
}

/// A declarative workflow graph operating over state `S`.
pub struct Workflow<S: Send + Sync> {
    pub name: &'static str,
    pub steps: Vec<WorkflowStep<S>>,
}

impl<S: Send + Sync + 'static> Workflow<S> {
    /// Creates a builder for a workflow with the specified name.
    pub fn builder(name: &'static str) -> WorkflowBuilder<S> {
        WorkflowBuilder::new(name)
    }
}

/// Fluent builder for constructing a [`Workflow`].
pub struct WorkflowBuilder<S: Send + Sync> {
    name: &'static str,
    steps: Vec<WorkflowStep<S>>,
}

impl<S: Send + Sync + 'static> WorkflowBuilder<S> {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            steps: Vec::new(),
        }
    }

    /// Appends a stage to the workflow.
    pub fn stage<T: DeserializeOwned + Send + 'static>(mut self, stage: Stage<S, T>) -> Self
    where
        S: Send + Sync,
    {
        self.steps.push(WorkflowStep::Stage(Box::new(stage)));
        self
    }

    /// Appends an arbitrary executable stage to the workflow.
    pub fn executable_stage(mut self, stage: Box<dyn ExecutableStage<S>>) -> Self {
        self.steps.push(WorkflowStep::Stage(stage));
        self
    }

    /// Appends a parallel fan-out step running stages concurrently.
    pub fn parallel(
        mut self,
        stages: Vec<Box<dyn ExecutableStage<S>>>,
        policy: ParallelPolicy,
    ) -> Self {
        self.steps.push(WorkflowStep::Parallel { stages, policy });
        self
    }

    /// Appends a dynamic planning step that resolves stages to run concurrently.
    pub fn dynamic_parallel<P, R>(
        mut self,
        planner: Stage<S, P>,
        resolver: R,
        policy: ParallelPolicy,
    ) -> Self
    where
        S: Send + Sync,
        P: DeserializeOwned + Send + 'static,
        R: Fn(&S) -> Vec<Box<dyn ExecutableStage<S>>> + Send + Sync + 'static,
    {
        self.steps.push(WorkflowStep::DynamicParallel {
            planner: Box::new(planner),
            resolver: Box::new(resolver),
            policy,
        });
        self
    }

    /// Appends an early-exit check to the workflow.
    pub fn early_exit_if<F>(mut self, condition: F, reason: &'static str) -> Self
    where
        F: Fn(&S) -> bool + Send + Sync + 'static,
    {
        self.steps.push(WorkflowStep::EarlyExitIf {
            condition: Box::new(condition),
            reason,
        });
        self
    }

    /// Appends a conditional branch to the workflow.
    pub fn branch<C, T, E>(mut self, condition: C, then_branch: T, else_branch: Option<E>) -> Self
    where
        C: Fn(&S) -> bool + Send + Sync + 'static,
        T: FnOnce(WorkflowBuilder<S>) -> WorkflowBuilder<S>,
        E: FnOnce(WorkflowBuilder<S>) -> WorkflowBuilder<S>,
    {
        let then_builder = then_branch(WorkflowBuilder::new("then_branch"));
        let else_flow = else_branch.map(|eb| eb(WorkflowBuilder::new("else_branch")).build());

        self.steps.push(WorkflowStep::Branch {
            condition: Box::new(condition),
            then_flow: then_builder.build(),
            else_flow,
        });
        self
    }

    /// Finalizes and builds the [`Workflow`].
    pub fn build(self) -> Workflow<S> {
        Workflow {
            name: self.name,
            steps: self.steps,
        }
    }
}
