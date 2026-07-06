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

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=third_party/prompts");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let prompts_dir = manifest_dir.join("third_party/prompts");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let generated = out_dir.join("prompts_generated.rs");

    let mut files = Vec::new();
    collect_files(&prompts_dir, &prompts_dir, &mut files).unwrap();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let revision = fs::read_to_string(prompts_dir.join("REVISION"))
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();

    let mut output = fs::File::create(generated).unwrap();
    writeln!(
        output,
        "pub const PROMPT_BUNDLE_REVISION: &str = {:?};",
        revision
    )
    .unwrap();
    writeln!(
        output,
        "pub const PROMPT_BUNDLE_FILES: &[(&str, &[u8])] = &["
    )
    .unwrap();
    for (relative, absolute) in files {
        writeln!(
            output,
            "    ({:?}, include_bytes!({:?})),",
            relative,
            absolute.display().to_string()
        )
        .unwrap();
    }
    writeln!(output, "];").unwrap();
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();

        if name == ".git" {
            continue;
        }

        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            files.push((relative, path));
        }
    }

    Ok(())
}
