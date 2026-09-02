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

//! Project identification and review pipeline dispatch.
//!
//! Sashiko supports automated code review pipelines for multiple open-source
//! systems software projects: the Linux kernel, QEMU, and LLVM.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::prompt_bundle;

/// Target project under review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Project {
    #[default]
    Kernel,
    Qemu,
    Llvm,
}

impl Project {
    /// Canonical project identifier.
    pub fn as_str(&self) -> &'static str {
        match self {
            Project::Kernel => "kernel",
            Project::Qemu => "qemu",
            Project::Llvm => "llvm",
        }
    }

    /// Default bundled prompts directory for this project.
    pub fn default_prompts_path(&self) -> Result<PathBuf> {
        prompt_bundle::default_prompts_path_for_project(self.as_str())
    }

    /// Auto-detect target project by inspecting repository or worktree root structure.
    pub fn detect_from_path(path: &Path) -> Option<Project> {
        if !path.exists() {
            return None;
        }

        // QEMU markers
        if path.join("qemu-options.hx").exists()
            || path.join("include/hw/qdev-core.h").exists()
            || path.join("qapi/qmp-dispatch.c").exists()
            || path.join("include/qemu/osdep.h").exists()
        {
            return Some(Project::Qemu);
        }

        // Check meson.build for QEMU project declaration
        let meson_path = path.join("meson.build");
        if meson_path.exists()
            && std::fs::read_to_string(&meson_path)
                .map(|s| s.contains("project('qemu'"))
                .unwrap_or(false)
        {
            return Some(Project::Qemu);
        }

        // LLVM markers (monorepo or subproject)
        if path.join("llvm/include/llvm/IR/Instructions.h").exists()
            || path.join("llvm/CMakeLists.txt").exists()
            || path.join("include/llvm/IR/Instructions.h").exists()
            || path.join("clang/include/clang/AST/ASTContext.h").exists()
        {
            return Some(Project::Llvm);
        }

        // Linux kernel markers
        if path.join("include/linux/kernel.h").exists() || path.join("Kconfig").exists() {
            return Some(Project::Kernel);
        }

        let makefile_path = path.join("Makefile");
        if makefile_path.exists()
            && std::fs::read_to_string(&makefile_path)
                .map(|s| s.contains("VERSION = ") && s.contains("PATCHLEVEL = "))
                .unwrap_or(false)
        {
            return Some(Project::Kernel);
        }

        None
    }

    /// Resolves active project using precedence:
    /// 1. Explicit CLI argument
    /// 2. Configured settings project name (if set and not default "Sashiko")
    /// 3. Repository or worktree heuristics
    /// 4. Fallback to Linux kernel
    pub fn resolve(
        explicit: Option<Project>,
        repo_path: Option<&Path>,
        settings_project: Option<&str>,
    ) -> Project {
        if let Some(p) = explicit {
            return p;
        }

        if let Some(s) = settings_project {
            let trimmed = s.trim();
            if !trimmed.is_empty()
                && !trimmed.eq_ignore_ascii_case("sashiko")
                && let Ok(p) = trimmed.parse::<Project>()
            {
                return p;
            }
        }

        if let Some(detected) = repo_path.and_then(Self::detect_from_path) {
            return detected;
        }

        Project::Kernel
    }
}

impl fmt::Display for Project {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for Project {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "kernel" | "linux" => Ok(Project::Kernel),
            "qemu" => Ok(Project::Qemu),
            "llvm" | "clang" | "llvm-project" => Ok(Project::Llvm),
            other => anyhow::bail!(
                "Unknown project '{}'. Supported projects: kernel, qemu, llvm",
                other
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_parse() {
        assert_eq!("kernel".parse::<Project>().unwrap(), Project::Kernel);
        assert_eq!("linux".parse::<Project>().unwrap(), Project::Kernel);
        assert_eq!("qemu".parse::<Project>().unwrap(), Project::Qemu);
        assert_eq!("llvm".parse::<Project>().unwrap(), Project::Llvm);
        assert_eq!("clang".parse::<Project>().unwrap(), Project::Llvm);
        assert_eq!("llvm-project".parse::<Project>().unwrap(), Project::Llvm);
        assert!("unknown".parse::<Project>().is_err());
    }

    #[test]
    fn test_project_display() {
        assert_eq!(Project::Kernel.to_string(), "kernel");
        assert_eq!(Project::Qemu.to_string(), "qemu");
        assert_eq!(Project::Llvm.to_string(), "llvm");
    }

    #[test]
    fn test_detect_from_path_qemu() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("qemu-options.hx"), "").unwrap();
        assert_eq!(Project::detect_from_path(temp.path()), Some(Project::Qemu));
    }

    #[test]
    fn test_detect_from_path_llvm() {
        let temp = tempfile::tempdir().unwrap();
        let llvm_dir = temp.path().join("llvm");
        std::fs::create_dir_all(&llvm_dir).unwrap();
        std::fs::write(llvm_dir.join("CMakeLists.txt"), "").unwrap();
        assert_eq!(Project::detect_from_path(temp.path()), Some(Project::Llvm));
    }

    #[test]
    fn test_detect_from_path_kernel() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Kconfig"), "").unwrap();
        assert_eq!(
            Project::detect_from_path(temp.path()),
            Some(Project::Kernel)
        );
    }

    #[test]
    fn test_resolve_precedence() {
        let temp_qemu = tempfile::tempdir().unwrap();
        std::fs::write(temp_qemu.path().join("qemu-options.hx"), "").unwrap();

        // 1. Explicit overrides everything
        assert_eq!(
            Project::resolve(Some(Project::Llvm), Some(temp_qemu.path()), Some("kernel")),
            Project::Llvm
        );

        // 2. Settings project overrides path detection
        assert_eq!(
            Project::resolve(None, Some(temp_qemu.path()), Some("llvm")),
            Project::Llvm
        );

        // 3. Path detection works when settings is default "Sashiko"
        assert_eq!(
            Project::resolve(None, Some(temp_qemu.path()), Some("Sashiko")),
            Project::Qemu
        );

        // 4. Fallback is Kernel
        let temp_empty = tempfile::tempdir().unwrap();
        assert_eq!(
            Project::resolve(None, Some(temp_empty.path()), None),
            Project::Kernel
        );
    }
}
