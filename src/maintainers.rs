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

//! Linux kernel MAINTAINERS file parsing and subsystem identification.
//!
//! Directly inspired by `scripts/get_maintainer.pl` from the Linux kernel source,
//! this module parses the MAINTAINERS file and matches modified file paths
//! against section inclusion (`F:`), exclusion (`X:`), and regex (`N:`) patterns.

use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use tracing::info;

/// Represents a compiled file or directory pattern from an `F:` or `X:` line.
#[derive(Debug, Clone)]
pub enum CompiledPattern {
    /// Trailing slash `dir/`: matches all files in and below `dir/`.
    PrefixDir { prefix: String, depth: usize },
    /// Directory single level `dir/*`: matches all files directly in `dir/`, not subdirectories.
    DirSingleLevel { dir_prefix: String, depth: usize },
    /// Exact file path match `dir/file.c`.
    ExactFile { path: String, depth: usize },
    /// General glob pattern converted to regex (e.g. `*/net/*` or `arch/*/include/*`).
    Glob { regex: Regex, depth: usize },
}

impl CompiledPattern {
    /// Compiles a pattern string from an `F:` or `X:` line into a `CompiledPattern`.
    pub fn compile(raw_pattern: &str) -> Self {
        let pattern = raw_pattern.trim().trim_start_matches("./");
        let depth = pattern.matches('/').count();

        if pattern.ends_with("/*") {
            let dir_prefix = pattern.strip_suffix('*').unwrap().to_string();
            CompiledPattern::DirSingleLevel { dir_prefix, depth }
        } else if pattern.ends_with('/') {
            CompiledPattern::PrefixDir {
                prefix: pattern.to_string(),
                depth,
            }
        } else if !pattern.contains('*') && !pattern.contains('?') {
            CompiledPattern::ExactFile {
                path: pattern.to_string(),
                depth,
            }
        } else {
            let re_str = glob_to_regex(pattern);
            let regex = Regex::new(&re_str).unwrap_or_else(|_| Regex::new("a^").unwrap());
            CompiledPattern::Glob { regex, depth }
        }
    }

    /// Tests if a file path matches this compiled pattern.
    pub fn is_match(&self, file_path: &str) -> bool {
        let path = file_path.trim_start_matches("./");
        match self {
            CompiledPattern::PrefixDir { prefix, .. } => path.starts_with(prefix),
            CompiledPattern::DirSingleLevel { dir_prefix, .. } => {
                if let Some(rest) = path.strip_prefix(dir_prefix) {
                    !rest.contains('/') && !rest.is_empty()
                } else {
                    false
                }
            }
            CompiledPattern::ExactFile {
                path: expected_path,
                ..
            } => path == expected_path,
            CompiledPattern::Glob { regex, .. } => regex.is_match(path),
        }
    }

    /// Returns the directory depth/specificity of the pattern.
    pub fn depth(&self) -> usize {
        match self {
            CompiledPattern::PrefixDir { depth, .. } => *depth,
            CompiledPattern::DirSingleLevel { depth, .. } => *depth,
            CompiledPattern::ExactFile { depth, .. } => *depth + 1,
            CompiledPattern::Glob { depth, .. } => *depth,
        }
    }
}

fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    re.push_str(".*");
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    re
}

/// A parsed section from the Linux kernel MAINTAINERS file.
#[derive(Debug, Clone)]
pub struct MaintainerSection {
    /// The section title / subsystem name (e.g. `NETWORKING DRIVERS`, `BTRFS FILE SYSTEM`).
    pub name: String,
    /// `F:` file inclusion patterns.
    pub files: Vec<CompiledPattern>,
    /// `X:` file exclusion patterns.
    pub excludes: Vec<CompiledPattern>,
    /// `N:` regex patterns.
    pub regexes: Vec<Regex>,
    /// `L:` mailing lists.
    pub mailing_lists: Vec<String>,
    /// `M:` / `R:` maintainers and reviewers.
    pub maintainers: Vec<String>,
    /// `T:` SCM source trees.
    pub trees: Vec<(String, Option<String>)>,
}

impl MaintainerSection {
    /// Checks if a file path matches this section.
    /// Returns `Some(depth)` with the matched pattern depth if matched, or `None` if not matched or excluded.
    pub fn match_file(&self, file_path: &str) -> Option<usize> {
        let normalized = file_path.trim_start_matches("./");

        // 1. Check exclusions (X: lines take precedence)
        for exclude in &self.excludes {
            if exclude.is_match(normalized) {
                return None;
            }
        }

        // 2. Check inclusions (F: lines)
        let mut best_depth: Option<usize> = None;
        for pattern in &self.files {
            if pattern.is_match(normalized) {
                let d = pattern.depth();
                best_depth = Some(best_depth.map_or(d, |curr: usize| curr.max(d)));
            }
        }

        // 3. Check regexes (N: lines)
        if best_depth.is_none() {
            for re in &self.regexes {
                if re.is_match(normalized) {
                    best_depth = Some(0);
                    break;
                }
            }
        }

        best_depth
    }
}

/// In-memory index of all parsed MAINTAINERS sections for high-performance matching.
#[derive(Debug, Clone, Default)]
pub struct MaintainersIndex {
    sections: Vec<MaintainerSection>,
}

impl MaintainersIndex {
    /// Creates a new empty index.
    pub fn new() -> Self {
        Self {
            sections: Vec::new(),
        }
    }

    /// Loads and parses MAINTAINERS from a file path.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)
            .with_context(|| format!("Failed to open MAINTAINERS file at {:?}", path))?;
        let reader = BufReader::new(file);
        Self::from_reader(reader)
    }

    /// Loads and parses MAINTAINERS from a Linux repository path.
    pub fn from_repo<P: AsRef<Path>>(repo_path: P) -> Result<Self> {
        let path = repo_path.as_ref().join("MAINTAINERS");
        Self::from_file(path)
    }

    /// Parses MAINTAINERS entries from a buffered reader.
    pub fn from_reader<R: BufRead>(reader: R) -> Result<Self> {
        let mut sections = Vec::new();
        let mut current_name = String::new();
        let mut current_files = Vec::new();
        let mut current_excludes = Vec::new();
        let mut current_regexes = Vec::new();
        let mut current_lists = Vec::new();
        let mut current_maintainers = Vec::new();
        let mut current_trees = Vec::new();

        let mut in_header = true;

        for line_res in reader.lines() {
            let line = line_res?;
            let trimmed = line.trim();

            if trimmed.is_empty() {
                if !current_name.is_empty()
                    && (!current_files.is_empty()
                        || !current_regexes.is_empty()
                        || !current_lists.is_empty()
                        || !current_trees.is_empty())
                {
                    sections.push(MaintainerSection {
                        name: current_name.clone(),
                        files: std::mem::take(&mut current_files),
                        excludes: std::mem::take(&mut current_excludes),
                        regexes: std::mem::take(&mut current_regexes),
                        mailing_lists: std::mem::take(&mut current_lists),
                        maintainers: std::mem::take(&mut current_maintainers),
                        trees: std::mem::take(&mut current_trees),
                    });
                }
                current_name.clear();
                continue;
            }

            // Skip leading comments and file introduction
            if trimmed.starts_with('#') {
                continue;
            }

            // Check for tag line: `[A-Z]: <value>`
            if let Some((tag, value)) = trimmed.split_once(':')
                && tag.len() == 1
                && tag.chars().next().unwrap().is_ascii_uppercase()
            {
                in_header = false;
                let val = value.trim();
                match tag {
                    "F" => {
                        current_files.push(CompiledPattern::compile(val));
                    }
                    "X" => {
                        current_excludes.push(CompiledPattern::compile(val));
                    }
                    "N" => {
                        if let Ok(re) = Regex::new(val) {
                            current_regexes.push(re);
                        }
                    }
                    "L" => {
                        // Extract email address from `L: list@vger.kernel.org (open list)`
                        let email = val
                            .split_whitespace()
                            .next()
                            .unwrap_or(val)
                            .trim_matches(['<', '>', '(', ')'])
                            .to_string();
                        if !email.is_empty() {
                            current_lists.push(email);
                        }
                    }
                    "M" | "R" => {
                        current_maintainers.push(val.to_string());
                    }
                    "T" => {
                        if let Some(rest) = val.strip_prefix("git ") {
                            let parts: Vec<&str> = rest.split_whitespace().collect();
                            if !parts.is_empty() {
                                let url = parts[0].to_string();
                                let branch = parts.get(1).map(|s| s.to_string());
                                current_trees.push((url, branch));
                            }
                        }
                    }
                    _ => {}
                }
            } else if current_name.is_empty()
                && !trimmed.starts_with("---")
                && !trimmed.starts_with("===")
            {
                // Section title (unless in introductory header)
                if in_header
                    && (trimmed.to_lowercase().contains("list of maintainers")
                        || trimmed
                            .to_lowercase()
                            .contains("descriptions of section entries"))
                {
                    continue;
                }
                in_header = false;
                current_name = trimmed.to_string();
            }
        }

        if !current_name.is_empty()
            && (!current_files.is_empty()
                || !current_regexes.is_empty()
                || !current_lists.is_empty()
                || !current_trees.is_empty())
        {
            sections.push(MaintainerSection {
                name: current_name,
                files: current_files,
                excludes: current_excludes,
                regexes: current_regexes,
                mailing_lists: current_lists,
                maintainers: current_maintainers,
                trees: current_trees,
            });
        }

        info!("Loaded and indexed {} MAINTAINERS sections", sections.len());
        Ok(Self { sections })
    }

    /// Matches a single file path against all MAINTAINERS sections.
    /// Returns matched section names ordered by pattern specificity (deepest match first).
    pub fn match_file(&self, file_path: &str) -> Vec<String> {
        let mut matches: Vec<(&MaintainerSection, usize)> = Vec::new();
        for section in &self.sections {
            if let Some(depth) = section.match_file(file_path) {
                matches.push((section, depth));
            }
        }

        // Sort by depth descending (most specific first), then alphabetically by name
        matches.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));

        matches
            .into_iter()
            .map(|(sec, _)| sec.name.clone())
            .collect()
    }

    /// Matches a collection of file paths against all MAINTAINERS sections.
    /// Returns the deduplicated union of all matched subsystem names.
    pub fn match_files<I, S>(&self, file_paths: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut matched_names = Vec::new();
        let mut seen = HashSet::new();

        for file_path in file_paths {
            let path = file_path.as_ref();
            for sub in self.match_file(path) {
                if seen.insert(sub.clone()) {
                    matched_names.push(sub);
                }
            }
        }

        matched_names
    }

    /// Returns all mailing list email addresses for the specified file paths.
    pub fn match_mailing_lists<I, S>(&self, file_paths: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut lists = Vec::new();
        let mut seen = HashSet::new();

        for file_path in file_paths {
            let path = file_path.as_ref();
            for section in &self.sections {
                if section.match_file(path).is_some() {
                    for list in &section.mailing_lists {
                        if seen.insert(list.clone()) {
                            lists.push(list.clone());
                        }
                    }
                }
            }
        }

        lists
    }

    /// Returns the number of parsed sections in the index.
    pub fn len(&self) -> usize {
        self.sections.len()
    }

    /// Returns true if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Returns a reference to all parsed sections.
    pub fn sections(&self) -> &[MaintainerSection] {
        &self.sections
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MAINTAINERS: &str = r#"
List of maintainers
===================

NETWORKING [GENERAL]
M:	David S. Miller <davem@davemloft.net>
L:	netdev@vger.kernel.org
S:	Maintained
F:	net/
F:	include/linux/net*
X:	net/ipv6/

NETWORKING [IPV6]
M:	Alexey Kuznetsov <kuznet@ms2.inr.ac.ru>
L:	netdev@vger.kernel.org
S:	Maintained
F:	net/ipv6/

INTEL E1000 NETWORK DRIVER
M:	Jesse Brandeburg <jesse.brandeburg@intel.com>
L:	netdev@vger.kernel.org
S:	Supported
F:	drivers/net/ethernet/intel/e1000/
X:	drivers/net/ethernet/intel/e1000/e1000_osdep.h

BTRFS FILE SYSTEM
M:	Chris Mason <clm@fb.com>
L:	linux-btrfs@vger.kernel.org
S:	Maintained
F:	fs/btrfs/
N:	btrfs

MEMORY MANAGEMENT
M:	Andrew Morton <akpm@linux-foundation.org>
L:	linux-mm@kvack.org
S:	Maintained
F:	mm/
F:	include/linux/mm*
"#;

    #[test]
    fn test_parse_maintainers() {
        let index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();
        assert_eq!(index.len(), 5);
    }

    #[test]
    fn test_match_single_file_subsystems() {
        let index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();

        // net/core/dev.c should match NETWORKING [GENERAL]
        let net_subs = index.match_file("net/core/dev.c");
        assert_eq!(net_subs, vec!["NETWORKING [GENERAL]"]);

        // net/ipv6/ip6_output.c should match NETWORKING [IPV6] (excluded from NETWORKING [GENERAL])
        let ipv6_subs = index.match_file("net/ipv6/ip6_output.c");
        assert_eq!(ipv6_subs, vec!["NETWORKING [IPV6]"]);

        // drivers/net/ethernet/intel/e1000/e1000_main.c should match INTEL E1000 NETWORK DRIVER
        let e1000_subs = index.match_file("drivers/net/ethernet/intel/e1000/e1000_main.c");
        assert_eq!(e1000_subs, vec!["INTEL E1000 NETWORK DRIVER"]);

        // Excluded file in e1000
        let e1000_osdep = index.match_file("drivers/net/ethernet/intel/e1000/e1000_osdep.h");
        assert!(e1000_osdep.is_empty());

        // fs/btrfs/inode.c should match BTRFS FILE SYSTEM
        let btrfs_subs = index.match_file("fs/btrfs/inode.c");
        assert_eq!(btrfs_subs, vec!["BTRFS FILE SYSTEM"]);
    }

    #[test]
    fn test_match_multiple_files_multi_subsystem() {
        let index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();

        let files = vec![
            "drivers/net/ethernet/intel/e1000/e1000_main.c",
            "fs/btrfs/inode.c",
            "mm/memory.c",
        ];

        let subs = index.match_files(&files);
        assert_eq!(subs.len(), 3);
        assert!(subs.contains(&"INTEL E1000 NETWORK DRIVER".to_string()));
        assert!(subs.contains(&"BTRFS FILE SYSTEM".to_string()));
        assert!(subs.contains(&"MEMORY MANAGEMENT".to_string()));
    }

    #[test]
    fn test_match_mailing_lists() {
        let index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();

        let files = vec![
            "drivers/net/ethernet/intel/e1000/e1000_main.c",
            "fs/btrfs/inode.c",
        ];

        let lists = index.match_mailing_lists(&files);
        assert!(lists.contains(&"netdev@vger.kernel.org".to_string()));
        assert!(lists.contains(&"linux-btrfs@vger.kernel.org".to_string()));
    }
}
