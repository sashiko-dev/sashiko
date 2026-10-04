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

//! Helpers for tests in more than one module.

use std::path::Path;

/// Writes `content` to `path` as an executable script for a test to run.
///
/// Not with std::fs::write: while this process holds the file open for
/// writing, a child that another test thread forks in that moment inherits
/// the descriptor until it execs, and Linux refuses to execute a file that any
/// process holds open for writing. Running the script then fails with "Text
/// file busy" (ETXTBSY), on whichever run the timing lines up.
///
/// So the content goes to a sibling file that is never executed, and a child
/// process installs the script from it. That child's descriptor closes when
/// it exits, and this process never holds one on the script to be inherited.
pub(crate) fn write_executable_script(path: &Path, content: &str) {
    let source = path.with_extension("source");
    std::fs::write(&source, content).expect("write the script's source");
    let status = std::process::Command::new("install")
        .arg("-m")
        .arg("755")
        .arg(&source)
        .arg(path)
        .status()
        .expect("run install");
    assert!(status.success(), "install {}: {status}", path.display());
}
