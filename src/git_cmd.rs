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

//! Construction of git commands whose target repository cannot be hijacked.
//!
//! Git looks at its environment before it looks at the working directory when
//! it decides which repository a command operates on. A command that reads as
//! if it were anchored, for example one built with current_dir(worktree), is
//! therefore silently redirected whenever the process that spawned it exports
//! GIT_DIR or one of its relatives.
//!
//! Git itself is such a process: a command started from a linked worktree by
//! rebase with the exec option, by bisect run, or by a hook inherits a GIT_DIR
//! pointing at the worktree metadata of the surrounding repository. Running
//! the test suite that way once let fixtures that create throwaway
//! repositories in temporary directories reinitialise the repository the
//! suite itself was running in, which rewrote its configuration and hid every
//! branch.
//!
//! Every git command in this crate is built here so that the directory handed
//! to the constructor, and nothing inherited from the outside, decides which
//! repository is touched.
//!
//! The same holds for the language git speaks. Callers match on its messages,
//! such as "already exists" from `git remote add`, and a translated message
//! never matches, so every command runs in the C locale.

use std::path::Path;

/// Environment variables through which a parent process can point git at a
/// repository, an index, an object store, or extra configuration.
///
/// They are removed from every command this module builds. Whatever a call
/// site genuinely needs is passed as an argument or set on the returned
/// command, where it is visible in the code.
const INHERITED_LOCATION_VARS: [&str; 11] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_WORK_TREE",
];

/// Builds a git command of the given type with the inherited location
/// variables removed and the C locale set, optionally anchored to a directory.
macro_rules! git_command {
    ($command:ty, $directory:expr) => {{
        let mut command = <$command>::new("git");
        for variable in INHERITED_LOCATION_VARS {
            command.env_remove(variable);
        }
        command.env("LC_ALL", "C");
        if let Some(directory) = $directory {
            command.current_dir(directory);
        }
        command
    }};
}

/// Builds a blocking git command that runs in `directory`.
pub fn in_dir(directory: impl AsRef<Path>) -> std::process::Command {
    git_command!(std::process::Command, Some(directory.as_ref()))
}

/// Builds an asynchronous git command that runs in `directory`.
///
/// `kill_on_drop(true)` is enabled by default so that cancelling or timing out
/// an awaiting future terminates the underlying `git` child process instead of
/// leaking an orphan (matching the explicit `kill_on_drop(true)` already used
/// by `git_ops` fetches and `worker::prefetch`). Callers that need a child to
/// outlive the handle may override this with `command.kill_on_drop(false)`.
pub fn in_dir_async(directory: impl AsRef<Path>) -> tokio::process::Command {
    let mut command = git_command!(tokio::process::Command, Some(directory.as_ref()));
    command.kill_on_drop(true);
    command
}

/// Builds an asynchronous git command that is not anchored anywhere.
///
/// This is for the few operations that name every path they touch as an
/// argument, such as cloning into a directory that does not exist yet. Use
/// [`in_dir_async`] for anything that works on an existing repository.
///
/// `kill_on_drop(true)` is enabled by default so that cancelling or timing out
/// an awaiting future terminates the underlying `git` child process instead of
/// leaking an orphan. Callers that need a child to outlive the handle may
/// override this with `command.kill_on_drop(false)`.
pub fn detached_async() -> tokio::process::Command {
    let mut command = git_command!(tokio::process::Command, None::<&Path>);
    command.kill_on_drop(true);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    /// The marker that must only ever appear in this module.
    const RAW_CONSTRUCTOR: &str = "Command::new(\"git\")";

    fn removed_variables<'a, I>(envs: I) -> HashSet<String>
    where
        I: Iterator<Item = (&'a OsStr, Option<&'a OsStr>)>,
    {
        envs.filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn test_blocking_command_drops_inherited_locations() {
        let command = in_dir("/tmp");
        let removed = removed_variables(command.get_envs());
        for variable in INHERITED_LOCATION_VARS {
            assert!(removed.contains(variable), "{variable} is still inherited");
        }
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp")));
    }

    #[test]
    fn test_asynchronous_command_drops_inherited_locations() {
        let command = in_dir_async("/tmp");
        let removed = removed_variables(command.as_std().get_envs());
        for variable in INHERITED_LOCATION_VARS {
            assert!(removed.contains(variable), "{variable} is still inherited");
        }
        assert_eq!(command.as_std().get_current_dir(), Some(Path::new("/tmp")));
        assert!(command.get_kill_on_drop());
    }

    #[test]
    fn test_detached_command_drops_inherited_locations() {
        let command = detached_async();
        let removed = removed_variables(command.as_std().get_envs());
        for variable in INHERITED_LOCATION_VARS {
            assert!(removed.contains(variable), "{variable} is still inherited");
        }
        assert_eq!(command.as_std().get_current_dir(), None);
        assert!(command.get_kill_on_drop());
    }

    /// Messages that callers match on have to come out untranslated.
    #[test]
    fn test_every_command_runs_in_the_c_locale() {
        fn locale<'a, I>(mut envs: I) -> Option<&'a OsStr>
        where
            I: Iterator<Item = (&'a OsStr, Option<&'a OsStr>)>,
        {
            envs.find(|(name, _)| *name == "LC_ALL")
                .and_then(|(_, value)| value)
        }

        let c = Some(OsStr::new("C"));
        assert_eq!(locale(in_dir("/tmp").get_envs()), c);
        assert_eq!(locale(in_dir_async("/tmp").as_std().get_envs()), c);
        assert_eq!(locale(detached_async().as_std().get_envs()), c);
    }

    fn resolved_git_dir(command: &mut std::process::Command) -> PathBuf {
        let output = command
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .expect("git rev-parse");
        assert!(output.status.success(), "git rev-parse failed");
        let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
        std::fs::canonicalize(printed).expect("resolvable git directory")
    }

    /// The directory handed to the constructor, not the environment, has to
    /// decide which repository a command works on.
    #[test]
    fn test_the_handed_directory_decides_the_repository() {
        let anchor = tempfile::tempdir().expect("temporary anchor repository");
        let decoy = tempfile::tempdir().expect("temporary decoy repository");
        for repository in [anchor.path(), decoy.path()] {
            let status = in_dir(repository)
                .args(["init", "--quiet"])
                .status()
                .expect("git init");
            assert!(status.success(), "git init failed in {repository:?}");
        }
        let anchor_git_dir =
            std::fs::canonicalize(anchor.path().join(".git")).expect("anchor git directory");
        let decoy_git_dir =
            std::fs::canonicalize(decoy.path().join(".git")).expect("decoy git directory");

        // Putting the variable back reproduces what a git parent process
        // hands down, and shows that git obeys it over the directory.
        let mut hijacked = in_dir(anchor.path());
        hijacked.env("GIT_DIR", &decoy_git_dir);
        assert_eq!(resolved_git_dir(&mut hijacked), decoy_git_dir);

        // The constructor removes the variable, so the same command reaches
        // the repository it was pointed at.
        assert_eq!(resolved_git_dir(&mut in_dir(anchor.path())), anchor_git_dir);
    }

    /// The test suite must never run with a repository forced on it, because
    /// every fixture that creates a throwaway repository would then write into
    /// whatever that variable points at. Running the suite under rebase with
    /// the exec option from a linked worktree is the way this happens.
    #[test]
    fn test_the_test_process_is_not_pointed_at_a_repository() {
        for variable in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            assert!(
                std::env::var_os(variable).is_none(),
                "{variable} is set for the test process; do not run the suite \
                 from git rebase --exec, git bisect run, or a git hook"
            );
        }
    }

    fn rust_sources(directory: &Path, found: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(directory).expect("readable source directory");
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_sources(&path, found);
            } else if path.extension() == Some(OsStr::new("rs")) {
                found.push(path);
            }
        }
    }

    /// Every git spawn has to go through this module, so a new call site
    /// cannot reopen the hole that the sanitised constructors close.
    #[test]
    fn test_no_git_command_is_spawned_outside_this_module() {
        let sources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&sources, &mut files);
        assert!(!files.is_empty(), "no sources found under {sources:?}");

        let this_module = sources.join("git_cmd.rs");
        let offenders: Vec<String> = files
            .into_iter()
            .filter(|path| *path != this_module)
            .filter(|path| {
                std::fs::read_to_string(path)
                    .expect("readable source file")
                    .contains(RAW_CONSTRUCTOR)
            })
            .map(|path| path.display().to_string())
            .collect();

        assert!(
            offenders.is_empty(),
            "{RAW_CONSTRUCTOR} must only appear in git_cmd.rs, because git \
             honours an inherited GIT_DIR over the working directory. Use \
             git_cmd::in_dir, git_cmd::in_dir_async or git_cmd::detached_async \
             instead. Offending files: {}",
            offenders.join(", ")
        );
    }
}
