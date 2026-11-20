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

//! Runtime loader and prompt formatters for the upstream Linux kernel threat
//! model (`Documentation/process/threat-model.rst`).
//!
//! Rather than vendoring a static copy that drifts from mainline, Sashiko reads
//! the document directly from Linus's mainline branch in the local kernel git
//! repository (`{torvalds_remote}/master`, falling back to `origin/master`,
//! `master`, and `HEAD`). When the repository predates the document or has no
//! matching ref, loading returns `None` and callers fall back cleanly.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tracing::{debug, warn};

/// In-tree path of the Linux kernel threat model document on mainline.
pub const THREAT_MODEL_PATH: &str = "Documentation/process/threat-model.rst";

/// Upper bound on the size of `threat-model.rst` accepted into prompts.
///
/// The upstream file is ~12 KiB; 64 KiB leaves ample room for upstream growth
/// while preventing unbounded prompt expansion if a repository ref is corrupt.
pub const MAX_THREAT_MODEL_BYTES: usize = 64 * 1024;

const GIT_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolved mainline git reference and commit SHA in a Linux kernel repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainlineRef {
    pub git_ref: String,
    pub commit: String,
}

/// Verbatim Linux kernel threat model loaded from a mainline git commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelThreatModel {
    pub text: String,
    pub commit: String,
    pub git_ref: String,
}

async fn run_git_capped(
    repo_path: &Path,
    args: &[&str],
    max_stdout: usize,
) -> Result<crate::toolbox::command::CappedOutput> {
    let mut cmd = crate::git_cmd::in_dir_async(repo_path);
    cmd.env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "safe.bareRepository=all"])
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(
        GIT_QUERY_TIMEOUT,
        crate::toolbox::command::capped_output_with_limit(&mut cmd, max_stdout),
    )
    .await
    .with_context(|| {
        format!(
            "git command {:?} timed out after {:?} in {}",
            args,
            GIT_QUERY_TIMEOUT,
            repo_path.display()
        )
    })?
    .with_context(|| {
        format!(
            "failed to execute git command {:?} in {}",
            args,
            repo_path.display()
        )
    })
}

/// Resolves the upstream Linux mainline ref and commit SHA in `repo_path`.
///
/// Prefers `{remote}/master` for any remote whose URL contains
/// `torvalds/linux`, falling back to `origin/master`, `master`, and `HEAD`.
pub async fn resolve_linux_mainline_ref(repo_path: &Path) -> Option<MainlineRef> {
    let mut candidate_refs = Vec::new();
    match run_git_capped(repo_path, &["remote", "-v"], 64 * 1024).await {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                let mut parts = line.split_whitespace();
                if let (Some(name), Some(url)) = (parts.next(), parts.next())
                    && url.contains("torvalds/linux")
                {
                    let r = format!("{name}/master");
                    if !candidate_refs.contains(&r) {
                        candidate_refs.push(r);
                    }
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            debug!(
                "Failed to query git remotes for Linux mainline ref in {}: {}",
                repo_path.display(),
                e
            );
        }
    }

    for fallback in ["origin/master", "master", "HEAD"] {
        let candidate = fallback.to_string();
        if !candidate_refs.contains(&candidate) {
            candidate_refs.push(candidate);
        }
    }

    for git_ref in candidate_refs {
        if git_ref.starts_with('-') {
            continue;
        }
        let rev = format!("{git_ref}^{{commit}}");
        match run_git_capped(
            repo_path,
            &["rev-parse", "--verify", "--quiet", "--end-of-options", &rev],
            1024,
        )
        .await
        {
            Ok(out) if out.status.success() => {
                let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(MainlineRef {
                        git_ref,
                        commit: sha,
                    });
                }
            }
            Ok(_) => {}
            Err(e) => {
                debug!(
                    "Failed to resolve candidate Linux mainline ref '{}' in {}: {}",
                    git_ref,
                    repo_path.display(),
                    e
                );
            }
        }
    }

    None
}

impl KernelThreatModel {
    /// Loads `Documentation/process/threat-model.rst` from the resolved Linux
    /// mainline ref of `repo_path`.
    pub async fn load(repo_path: &Path) -> Option<Self> {
        let mainline = resolve_linux_mainline_ref(repo_path).await?;
        Self::load_from_ref(repo_path, &mainline).await
    }

    /// Loads `Documentation/process/threat-model.rst` from a specific
    /// [`MainlineRef`].
    pub async fn load_from_ref(repo_path: &Path, mainline: &MainlineRef) -> Option<Self> {
        if mainline.commit.starts_with('-')
            || !mainline.commit.chars().all(|c| c.is_ascii_hexdigit())
        {
            return None;
        }
        let spec = format!("{}:{}", mainline.commit, THREAT_MODEL_PATH);
        let out = match run_git_capped(
            repo_path,
            &["show", "--end-of-options", &spec],
            MAX_THREAT_MODEL_BYTES,
        )
        .await
        {
            Ok(out) => out,
            Err(e) => {
                warn!(
                    "Failed to read {} from {} ({}): {}",
                    THREAT_MODEL_PATH,
                    mainline.git_ref,
                    &mainline.commit[..mainline.commit.len().min(12)],
                    e
                );
                return None;
            }
        };

        if out.stdout_capped {
            warn!(
                "Ignoring {}:{} because it exceeds {} bytes",
                mainline.git_ref, THREAT_MODEL_PATH, MAX_THREAT_MODEL_BYTES
            );
            return None;
        }

        if !out.status.success() {
            debug!(
                "{} not present at {} ({})",
                THREAT_MODEL_PATH,
                mainline.git_ref,
                &mainline.commit[..mainline.commit.len().min(12)]
            );
            return None;
        }

        let raw = match String::from_utf8(out.stdout) {
            Ok(s) => s,
            Err(_) => {
                warn!(
                    "Ignoring {}:{} because its contents are not valid UTF-8",
                    mainline.git_ref, THREAT_MODEL_PATH
                );
                return None;
            }
        };

        if raw.trim().is_empty() {
            debug!("Ignoring empty {}:{}", mainline.git_ref, THREAT_MODEL_PATH);
            return None;
        }

        // Prevent a literal closing delimiter inside the document from
        // terminating the prompt wrapper early.
        let text = raw.replace("</kernel_threat_model>", "< /kernel_threat_model>");
        let model = Self {
            text,
            commit: mainline.commit.clone(),
            git_ref: mainline.git_ref.clone(),
        };
        debug!(
            "Loaded {} from {} ({}, {} bytes)",
            THREAT_MODEL_PATH,
            model.git_ref,
            model.short_commit(),
            model.text.len()
        );
        Some(model)
    }

    /// Returns at most the first 12 hex characters of the source commit SHA.
    pub fn short_commit(&self) -> &str {
        &self.commit[..self.commit.len().min(12)]
    }

    /// Wraps the verbatim threat model text in a tagged `<kernel_threat_model>`
    /// block recording its mainline ref and commit.
    pub fn render_xml_block(&self) -> String {
        format!(
            "<kernel_threat_model source=\"{path} @ {git_ref} ({short_sha})\">\n{body}\n</kernel_threat_model>",
            path = THREAT_MODEL_PATH,
            git_ref = self.git_ref,
            short_sha = self.short_commit(),
            body = self.text.trim_end(),
        )
    }

    /// Renders the threat model block and severity-calibration instructions,
    /// returning an empty string when `model` is `None`.
    pub fn render_for_severity(model: Option<&Self>) -> String {
        let Some(m) = model else {
            return String::new();
        };
        format!(
            "{block}\n\n\
Applying the Linux kernel threat model to severity calibration:\n\
- Treat a defect as a kernel security vulnerability ONLY when an actor can cross a boundary the threat model above guarantees (or a guest-to-host/hypervisor isolation boundary in virtualization code).\n\
- Classes listed under \"What classes of problems are not considered vulnerabilities\" (including actions requiring root or elevated capabilities such as CAP_SYS_ADMIN, CAP_NET_ADMIN, CAP_SYS_RAWIO, or CAP_SYS_MODULE; debugfs, staging, experimental, or debug-only Kconfigs; administrator-mounted corrupted or crafted filesystem images; physically modified or emulated non-conforming hardware; lab-only probabilistic attacks; hardening bypasses with no exploit path; and random padding or pointer leaks) are regular bugs, NOT security vulnerabilities, and MUST NOT be escalated to Critical on security grounds.\n\
- Regular bugs outside a security boundary are never dropped; calibrate their severity by concrete production consequence, required privileges, and trigger likelihood per the severity guidelines.",
            block = m.render_xml_block(),
        )
    }

    /// Renders the threat model block for the related pre-existing bug
    /// public-disclosure check, returning an empty string when `model` is `None`.
    pub fn render_for_disclosure_check(model: Option<&Self>) -> String {
        match model {
            Some(m) => m.render_xml_block(),
            None => String::new(),
        }
    }

    /// Renders the threat model block and scoping instructions for the Linux
    /// `security` analysis stage, returning an empty string when `model` is `None`.
    pub fn render_for_security_stage(model: Option<&Self>) -> String {
        let Some(m) = model else {
            return String::new();
        };
        format!(
            "{block}\n\n\
Applying the Linux kernel threat model in this security audit:\n\
- Prioritize boundaries the threat model guarantees: unprivileged userspace syscalls/ioctls/IPC, unprivileged user namespaces vs. the initial namespace, remote network peers, virtualization guest-to-host/hypervisor boundaries, and IOMMU or driver paths explicitly documented as hardened against hostile hardware.\n\
- In every raised concern, explicitly state who can trigger the path, what capabilities or configuration it requires (e.g., unprivileged local user, userns, CAP_NET_ADMIN, CAP_SYS_ADMIN, debugfs, mounted filesystem image), and whether it crosses a guaranteed trust boundary.\n\
- NEVER dismiss a genuine memory-safety, bounds, integer-overflow, race, or information-leak defect merely because the threat model classifies it as a regular bug rather than a vulnerability (for example, on a root-only, debugfs, or mounted-image path). Report it as a concern and record its required privilege accurately so downstream verification calibrates its severity.",
            block = m.render_xml_block(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn run_git_ok(dir: &Path, args: &[&str]) {
        let out = crate::git_cmd::in_dir(dir)
            .args(args)
            .output()
            .expect("git command should spawn");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo_with_commit(files: &[(&str, &[u8])]) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        run_git_ok(path, &["init", "-b", "master"]);
        run_git_ok(path, &["config", "user.name", "Test Author"]);
        run_git_ok(path, &["config", "user.email", "test@example.com"]);
        for (rel, content) in files {
            let full = path.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full, content).unwrap();
        }
        run_git_ok(path, &["add", "."]);
        run_git_ok(path, &["commit", "-m", "initial"]);
        let out = crate::git_cmd::in_dir(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        (dir, sha)
    }

    #[tokio::test]
    async fn loads_from_torvalds_remote_before_origin() {
        let (dir, origin_sha) =
            init_repo_with_commit(&[(THREAT_MODEL_PATH, b"Origin threat model\n")]);
        let path = dir.path();
        run_git_ok(
            path,
            &[
                "remote",
                "add",
                "origin",
                "https://example.com/fork/linux.git",
            ],
        );
        run_git_ok(
            path,
            &["update-ref", "refs/remotes/origin/master", &origin_sha],
        );

        let full = path.join(THREAT_MODEL_PATH);
        fs::write(&full, b"Linus mainline threat model\n").unwrap();
        run_git_ok(path, &["add", THREAT_MODEL_PATH]);
        run_git_ok(path, &["commit", "-m", "linus update"]);
        let linus_sha = String::from_utf8(
            crate::git_cmd::in_dir(path)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        run_git_ok(
            path,
            &[
                "remote",
                "add",
                "linus",
                "git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git",
            ],
        );
        run_git_ok(
            path,
            &["update-ref", "refs/remotes/linus/master", &linus_sha],
        );
        // Move HEAD back to origin_sha so only linus/master has the updated text.
        run_git_ok(path, &["checkout", "--detach", &origin_sha]);

        let loaded = KernelThreatModel::load(path)
            .await
            .expect("should load from linus/master");
        assert_eq!(loaded.git_ref, "linus/master");
        assert_eq!(loaded.commit, linus_sha);
        assert_eq!(loaded.text, "Linus mainline threat model\n");

        let rendered = KernelThreatModel::render_for_severity(Some(&loaded));
        assert!(rendered.contains("Documentation/process/threat-model.rst @ linus/master"));
        assert!(rendered.contains(loaded.short_commit()));
        assert!(rendered.contains("Linus mainline threat model"));
    }

    #[tokio::test]
    async fn falls_back_to_head_and_returns_none_when_absent() {
        let (dir_present, sha) =
            init_repo_with_commit(&[(THREAT_MODEL_PATH, b"Fallback threat model\n")]);
        let loaded = KernelThreatModel::load(dir_present.path())
            .await
            .expect("should load from master/HEAD");
        assert_eq!(loaded.commit, sha);
        assert_eq!(loaded.text, "Fallback threat model\n");

        let (dir_absent, _) = init_repo_with_commit(&[("README", b"pre-7.3 tree\n")]);
        assert!(KernelThreatModel::load(dir_absent.path()).await.is_none());
        assert_eq!(KernelThreatModel::render_for_severity(None), "");
        assert_eq!(KernelThreatModel::render_for_disclosure_check(None), "");
        assert_eq!(KernelThreatModel::render_for_security_stage(None), "");
    }

    #[tokio::test]
    async fn rejects_oversized_and_invalid_utf8_documents() {
        let big = vec![b'a'; MAX_THREAT_MODEL_BYTES + 128];
        let (dir_big, _) = init_repo_with_commit(&[(THREAT_MODEL_PATH, &big)]);
        assert!(KernelThreatModel::load(dir_big.path()).await.is_none());

        let bad_utf8 = [0xff, 0xfe, 0xfd, b'\n'];
        let (dir_bad, _) = init_repo_with_commit(&[(THREAT_MODEL_PATH, &bad_utf8)]);
        assert!(KernelThreatModel::load(dir_bad.path()).await.is_none());
    }

    #[tokio::test]
    async fn neutralizes_embedded_closing_tag_and_formats_security_guidance() {
        let (dir, _) = init_repo_with_commit(&[(
            THREAT_MODEL_PATH,
            b"Body with </kernel_threat_model> inside\n",
        )]);
        let loaded = KernelThreatModel::load(dir.path()).await.unwrap();
        assert!(!loaded.text.contains("</kernel_threat_model>"));
        let sec = KernelThreatModel::render_for_security_stage(Some(&loaded));
        assert!(sec.contains("NEVER dismiss a genuine memory-safety"));
    }
}
