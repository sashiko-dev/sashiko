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

//! The codebase an instance of Sashiko reviews.
//!
//! Sashiko began as a Linux kernel reviewer, and the things that made it one
//! were spread across the tree as defaults rather than named as choices: which
//! prompt directory to load, whether a MAINTAINERS file is worth indexing,
//! which review workflow to run. Naming the project turns each of those from a
//! default into an answer, so a second codebase has something to attach to.
//!
//! One process serves one project. There is no per-request project and no
//! project column in the database; a second project is a second instance with
//! its own configuration, database and port.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The codebase under review.
///
/// Linux is the default so that an existing deployment, which names no project
/// anywhere, keeps behaving exactly as it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ProjectId {
    #[default]
    Linux,
    Sashiko,
    Gcc,
}

impl ProjectId {
    /// The name this project is selected by: on the command line, in the
    /// environment, and in the configuration file.
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectId::Linux => "linux",
            ProjectId::Sashiko => "sashiko",
            ProjectId::Gcc => "gcc",
        }
    }

    /// Human-readable display name for UI headers and titles.
    pub fn display_name(self) -> &'static str {
        match self {
            ProjectId::Linux => "Linux",
            ProjectId::Sashiko => "Sashiko",
            ProjectId::Gcc => "GCC",
        }
    }

    /// The subdirectory of the installed prompt bundle holding this project's
    /// prompts.
    ///
    /// This is not [`ProjectId::as_str`]. The kernel prompts are vendored from
    /// an upstream tree that calls the directory `kernel`, and renaming it
    /// would fork that tree for no reason. Keeping the two names apart is what
    /// lets a project's selector and its prompt directory disagree.
    pub fn prompt_dir(self) -> &'static str {
        match self {
            ProjectId::Linux => "kernel",
            ProjectId::Sashiko => "sashiko",
            ProjectId::Gcc => "gcc",
        }
    }

    /// Whether this project has a MAINTAINERS file worth indexing at startup.
    ///
    /// Indexing one is several seconds of work against a tree that may not have
    /// the file at all, and everything downstream of the index is Linux-only.
    pub fn uses_maintainers(self) -> bool {
        match self {
            ProjectId::Linux => true,
            ProjectId::Sashiko | ProjectId::Gcc => false,
        }
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProjectId {
    type Err = UnknownProject;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "linux" => Ok(ProjectId::Linux),
            "sashiko" => Ok(ProjectId::Sashiko),
            "gcc" => Ok(ProjectId::Gcc),
            _ => Err(UnknownProject(s.to_string())),
        }
    }
}

/// A project name that does not name a project.
///
/// This carries the rejected input because it is reported to whoever typed it,
/// and a bare "unknown project" gives them nothing to correct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownProject(pub String);

impl fmt::Display for UnknownProject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown project {:?}, expected one of: ", self.0)?;
        let mut first = true;
        for project in ProjectId::ALL {
            if !first {
                f.write_str(", ")?;
            }
            f.write_str(project.as_str())?;
            first = false;
        }
        Ok(())
    }
}

impl std::error::Error for UnknownProject {}

impl ProjectId {
    /// Every project, for error messages and for tests that must cover them
    /// all. Adding a variant without adding it here fails the exhaustiveness
    /// test below.
    pub const ALL: &'static [ProjectId] = &[ProjectId::Linux, ProjectId::Sashiko, ProjectId::Gcc];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_lists_every_project() {
        // A variant missing from ALL would silently drop out of the error
        // message and out of every test that iterates it, so the count is
        // checked against a match the compiler forces to stay exhaustive.
        for project in ProjectId::ALL {
            match project {
                ProjectId::Linux | ProjectId::Sashiko | ProjectId::Gcc => {}
            }
        }
        assert_eq!(ProjectId::ALL.len(), 3);
    }

    #[test]
    fn test_name_round_trips_through_parsing() {
        for project in ProjectId::ALL {
            assert_eq!(ProjectId::from_str(project.as_str()), Ok(*project));
            assert_eq!(project.to_string(), project.as_str());
        }
    }

    #[test]
    fn test_display_name() {
        assert_eq!(ProjectId::Linux.display_name(), "Linux");
        assert_eq!(ProjectId::Sashiko.display_name(), "Sashiko");
        assert_eq!(ProjectId::Gcc.display_name(), "GCC");
    }

    #[test]
    fn test_parsing_is_forgiving_about_case_and_padding() {
        assert_eq!(ProjectId::from_str("Linux"), Ok(ProjectId::Linux));
        assert_eq!(ProjectId::from_str("  SASHIKO "), Ok(ProjectId::Sashiko));
    }

    #[test]
    fn test_unknown_project_names_the_alternatives() {
        let err = ProjectId::from_str("freebsd").unwrap_err();
        assert_eq!(err, UnknownProject("freebsd".to_string()));
        let message = err.to_string();
        assert!(message.contains("freebsd"), "{message}");
        for project in ProjectId::ALL {
            assert!(message.contains(project.as_str()), "{message}");
        }
    }

    #[test]
    fn test_linux_is_the_default() {
        // An existing deployment names no project anywhere, and must keep
        // getting the kernel behaviour it has today.
        assert_eq!(ProjectId::default(), ProjectId::Linux);
    }

    #[test]
    fn test_prompt_dir_is_not_the_project_name() {
        // The vendored kernel prompts live under `kernel`, so the selector and
        // the directory genuinely differ for Linux. Asserting it keeps a later
        // "simplification" to as_str() from silently pointing at nothing.
        assert_eq!(ProjectId::Linux.prompt_dir(), "kernel");
        assert_eq!(ProjectId::Sashiko.prompt_dir(), "sashiko");
    }

    #[test]
    fn test_config_value_parses_as_lowercase_name() {
        #[derive(Debug, Deserialize)]
        struct Holder {
            kind: ProjectId,
        }
        let holder: Holder = toml::from_str("kind = \"sashiko\"").unwrap();
        assert_eq!(holder.kind, ProjectId::Sashiko);
        assert!(toml::from_str::<Holder>("kind = \"freebsd\"").is_err());
    }
}
