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

// build.rs includes this file with include!, so it must hold nothing but
// plain items: no inner doc comments and no crate paths.

/// The prompt bundle namespace each `projects/<dir>/prompts/` tree is
/// bundled under, keyed by the `projects/` directory name.
///
/// A project reads its prompts from the namespace named by
/// `ProjectId::prompt_dir()`, which is not always its selector: Linux reads
/// `kernel/`, the name of the vendored upstream tree. Without this mapping,
/// first-party Linux prompts in `projects/linux/prompts/` would be bundled
/// under `linux/`, where nothing reads them. A `projects/` directory missing
/// here is bundled under its own name. A test in `project.rs` keeps this
/// table in step with `ProjectId::prompt_dir()`.
pub const PROJECT_PROMPT_NAMESPACES: &[(&str, &str)] = &[
    ("linux", "kernel"),
    ("sashiko", "sashiko"),
    ("gcc", "gcc"),
    ("qemu", "qemu"),
];
