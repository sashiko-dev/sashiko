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

//! Cherry-pick review: context type, finding filter, stage prompts, and workflow.

pub mod context;
pub mod filter;
pub mod prompts;
pub mod synthesis;
pub mod workflow;

pub use context::{CherryPickContext, CherryPickReviewContext};
pub use filter::filter_cherry_pick_findings;
pub use prompts::*;
pub use workflow::{CherryPickReviewWorkflow, CherryPickWorkflowEnv, execute_workflow};
