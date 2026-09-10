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

    track_git_changes();

    let git_hash = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string()));

    println!("cargo:rustc-env=GIT_HASH={}", git_hash);

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

    let mut generated_bytes = Vec::new();
    writeln!(
        generated_bytes,
        "pub const PROMPT_BUNDLE_REVISION: &str = {:?};",
        revision
    )
    .unwrap();
    writeln!(
        generated_bytes,
        "pub const PROMPT_BUNDLE_FILES: &[(&str, &[u8])] = &["
    )
    .unwrap();
    for (relative, absolute) in files {
        writeln!(
            generated_bytes,
            "    ({:?}, include_bytes!({:?})),",
            relative,
            absolute.display().to_string()
        )
        .unwrap();
    }
    writeln!(generated_bytes, "];").unwrap();

    let should_write = match fs::read(&generated) {
        Ok(existing) => existing != generated_bytes,
        Err(_) => true,
    };
    if should_write {
        fs::write(&generated, &generated_bytes).unwrap();
    }
}

fn track_git_changes() {
    let head_path = get_git_path("HEAD");
    if let Some(ref head) = head_path
        && head.exists()
    {
        println!("cargo:rerun-if-changed={}", head.display());
        if let Ok(head_content) = fs::read_to_string(head)
            && let Some(ref_name) = head_content.strip_prefix("ref: ")
        {
            let ref_name = ref_name.trim();
            if let Some(ref_path) = get_git_path(ref_name)
                && ref_path.exists()
            {
                println!("cargo:rerun-if-changed={}", ref_path.display());
            }
        }
    }
    if let Some(packed) = get_git_path("packed-refs")
        && packed.exists()
    {
        println!("cargo:rerun-if-changed={}", packed.display());
    }
    if let Some(reflog) = get_git_path("logs/HEAD")
        && reflog.exists()
    {
        println!("cargo:rerun-if-changed={}", reflog.display());
    }
    if let Some(reftable) = get_git_path("reftable/tables.list")
        && reftable.exists()
    {
        println!("cargo:rerun-if-changed={}", reftable.display());
    }
}

fn get_git_path(arg: &str) -> Option<PathBuf> {
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", arg])
        .output()
        && output.status.success()
        && let Ok(path_str) = String::from_utf8(output.stdout)
    {
        let path = PathBuf::from(path_str.trim());
        if path.exists() {
            return Some(path);
        }
    }

    // Fallback if git binary or rev-parse fails
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").ok()?);
    let git_item = manifest_dir.join(".git");
    let git_dir = if git_item.is_dir() {
        git_item
    } else if git_item.is_file() {
        let content = fs::read_to_string(&git_item).ok()?;
        let gitdir_str = content.trim().strip_prefix("gitdir:")?.trim();
        let p = PathBuf::from(gitdir_str);
        if p.is_absolute() {
            p
        } else {
            manifest_dir.join(p)
        }
    } else {
        return None;
    };

    let target = git_dir.join(arg);
    if target.exists() {
        return Some(target);
    }

    // In a git worktree, git_dir has a commondir file pointing to the main repo gitdir
    if let Ok(commondir_content) = fs::read_to_string(git_dir.join("commondir")) {
        let common = commondir_content.trim();
        let common_dir = git_dir.join(common);
        let common_target = common_dir.join(arg);
        if common_target.exists() {
            return Some(common_target);
        }
    }

    None
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
