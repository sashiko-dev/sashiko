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

//! A generic, composable, and declarative AI workflow framework in Rust.

pub mod engine;
pub mod events;
pub mod graph;
pub mod output;
pub mod policy;
pub mod prompt;
pub mod stage;

pub use engine::{WorkflowEngine, WorkflowOutcome};
pub use events::WorkflowEvent;
pub use graph::{Workflow, WorkflowBuilder, WorkflowStep};
pub use output::OutputFormat;
pub use policy::{ParallelPolicy, RecitationPolicy, StagePolicy, ToolScope};
pub use prompt::{InclusionDirective, PromptTemplate};
pub use stage::{ExecutableStage, Stage, StageBuilder, StageOutcome, StateMutation, WorkflowEnv};
