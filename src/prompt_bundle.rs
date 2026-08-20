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

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/prompts_generated.rs"));

const COMPLETE_MARKER: &str = ".sashiko-prompts-complete";

pub fn default_kernel_prompts_path() -> Result<PathBuf> {
    let root = install_prompt_bundle(false)?;
    Ok(root.join("kernel"))
}

pub(crate) fn resolve_review_prompts_path(configured: Option<&Path>) -> Result<PathBuf> {
    resolve_review_prompts_path_with(configured, default_kernel_prompts_path)
}

fn resolve_review_prompts_path_with<F>(
    configured: Option<&Path>,
    default_path: F,
) -> Result<PathBuf>
where
    F: FnOnce() -> Result<PathBuf>,
{
    let (path, source) = match configured {
        Some(path) => (path.to_path_buf(), "configured"),
        None => (default_path()?, "bundled kernel"),
    };

    if !path.is_dir() {
        bail!(
            "{} prompts path is not a directory: {}",
            source,
            path.display()
        );
    }

    let review_core = path.join("review-core.md");
    if !review_core.is_file() {
        bail!(
            "{} prompts path is missing review-core.md: {}",
            source,
            path.display()
        );
    }

    Ok(path)
}

pub fn install_prompt_bundle(force: bool) -> Result<PathBuf> {
    let root = prompt_bundle_root()?;
    let marker = root.join(COMPLETE_MARKER);

    if !force && marker.exists() {
        return Ok(root);
    }

    if force && root.exists() {
        std::fs::remove_dir_all(&root)
            .with_context(|| format!("failed to remove {}", root.display()))?;
    }

    for (relative, content) in PROMPT_BUNDLE_FILES {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    std::fs::write(&marker, PROMPT_BUNDLE_REVISION)
        .with_context(|| format!("failed to write {}", marker.display()))?;

    Ok(root)
}

pub fn prompt_bundle_root() -> Result<PathBuf> {
    Ok(data_home()?
        .join("sashiko/prompts")
        .join(PROMPT_BUNDLE_REVISION))
}

fn data_home() -> Result<PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data_home));
    }

    if let Some(home) = std::env::var_os("HOME") {
        return Ok(Path::new(&home).join(".local/share"));
    }

    Ok(std::env::current_dir()?.join(".local/share"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_prompt_profile(root: &Path) -> PathBuf {
        let path = root.join("profile");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("review-core.md"), "# Review\n").unwrap();
        path
    }

    #[test]
    fn test_prompt_bundle_contains_kernel_review_core() {
        assert!(
            PROMPT_BUNDLE_FILES
                .iter()
                .any(|(path, _)| *path == "kernel/review-core.md")
        );
    }

    #[test]
    fn test_default_review_prompts_resolve_to_kernel_profile() {
        let temp = tempfile::tempdir().unwrap();
        let kernel = create_prompt_profile(temp.path());

        let resolved = resolve_review_prompts_path_with(None, || Ok(kernel.clone())).unwrap();

        assert_eq!(resolved, kernel);
    }

    #[test]
    fn test_configured_review_prompts_are_used_exactly() {
        let temp = tempfile::tempdir().unwrap();
        let configured = create_prompt_profile(temp.path());

        let resolved = resolve_review_prompts_path_with(Some(&configured), || {
            panic!("explicit prompt configuration must not use the kernel fallback")
        })
        .unwrap();

        assert_eq!(resolved, configured);
    }

    #[test]
    fn test_missing_configured_review_prompts_return_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");

        let error = resolve_review_prompts_path_with(Some(&missing), || {
            panic!("explicit prompt configuration must not use the kernel fallback")
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("configured prompts path is not a directory")
        );
        assert!(error.to_string().contains(&missing.display().to_string()));
    }

    #[test]
    fn test_malformed_configured_review_prompts_return_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let malformed = temp.path().join("profile");
        std::fs::create_dir(&malformed).unwrap();

        let error = resolve_review_prompts_path_with(Some(&malformed), || {
            panic!("explicit prompt configuration must not use the kernel fallback")
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("configured prompts path is missing review-core.md")
        );
        assert!(error.to_string().contains(&malformed.display().to_string()));
    }

    #[test]
    fn test_prompt_bundle_root_uses_xdg_data_home() {
        let temp = tempfile::tempdir().unwrap();
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", temp.path());
        }

        assert_eq!(
            prompt_bundle_root().unwrap(),
            temp.path()
                .join("sashiko/prompts")
                .join(PROMPT_BUNDLE_REVISION)
        );

        unsafe {
            if let Some(value) = old_xdg {
                std::env::set_var("XDG_DATA_HOME", value);
            } else {
                std::env::remove_var("XDG_DATA_HOME");
            }
        }
    }
}
