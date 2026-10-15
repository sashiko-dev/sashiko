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
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{Arc, RwLock};
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
    /// `anchor` is the literal directory prefix before the first wildcard
    /// (`arch/` for `arch/*/include/*`, empty for `*/net/*`); every matching
    /// path starts with it.
    Glob {
        regex: Regex,
        depth: usize,
        anchor: String,
    },
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
            let literal_end = pattern.find(['*', '?']).unwrap_or(pattern.len());
            let anchor_end = pattern[..literal_end].rfind('/').map_or(0, |i| i + 1);
            CompiledPattern::Glob {
                regex,
                depth,
                anchor: pattern[..anchor_end].to_string(),
            }
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
    /// Whether a file pattern claims the whole tree, which in the current file
    /// means `F: *` or `F: */` and only occurs in THE REST.
    ///
    /// This cannot be recovered from the compiled patterns: `*` compiles to a
    /// glob that matches only top-level files, and `*/` to a prefix that
    /// matches nothing at all. Both are plainly meant as "everything", so the
    /// intent is recorded while the raw text is still in hand.
    pub catch_all: bool,
}

/// Extracts the address from a MAINTAINERS entry such as
/// `Linus Torvalds <torvalds@linux-foundation.org>`.
///
/// Entries are conventionally a display name followed by an address in angle
/// brackets, but a bare address also occurs, so the brackets are optional. The
/// result is lowercased because the addresses are later compared against an
/// address a person typed into a sign-in form.
///
/// Anything that does not look like an address yields None rather than a
/// best-effort string: an entry that cannot be parsed must not become a
/// principal that some other entry could collide with.
pub fn maintainer_address(entry: &str) -> Option<String> {
    let candidate = match (entry.find('<'), entry.rfind('>')) {
        (Some(open), Some(close)) if close > open + 1 => &entry[open + 1..close],
        _ => entry,
    };
    let address = candidate
        .trim()
        .trim_matches('"')
        .trim()
        .to_ascii_lowercase();
    let (local, domain) = address.split_once('@')?;
    let looks_like_an_address = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !address.contains(char::is_whitespace)
        && address.matches('@').count() == 1;
    looks_like_an_address.then_some(address)
}

impl MaintainerSection {
    /// The addresses of everyone listed on an `M:` or `R:` line.
    ///
    /// Reviewers are deliberately not distinguished from maintainers: both are
    /// people the kernel already trusts with the subsystem.
    pub fn maintainer_addresses(&self) -> impl Iterator<Item = String> + '_ {
        self.maintainers
            .iter()
            .filter_map(|m| maintainer_address(m))
    }

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

/// `(section index, pattern depth)` pairs sharing one lookup key.
type SectionHits = Vec<(usize, usize)>;

/// Answers [`MaintainersIndex::match_file`] for many paths at once.
///
/// `match_file` tests every pattern of every section, which is fine for the
/// handful of files in a patch but takes minutes for a whole kernel tree
/// (some 90,000 files against roughly 10,000 `F:` patterns). This matcher
/// buckets patterns by the directory they are anchored to, so a file is only
/// compared with patterns anchored at one of its ancestor directories. Its
/// results are identical to `match_file`.
pub struct FileMatcher<'a> {
    sections: &'a [MaintainerSection],
    exact: HashMap<&'a str, SectionHits>,
    prefix_dirs: HashMap<&'a str, SectionHits>,
    single_level: HashMap<&'a str, SectionHits>,
    globs: HashMap<&'a str, Vec<(usize, &'a Regex, usize)>>,
    with_regexes: Vec<usize>,
}

impl<'a> FileMatcher<'a> {
    fn new(sections: &'a [MaintainerSection]) -> Self {
        let mut matcher = Self {
            sections,
            exact: HashMap::new(),
            prefix_dirs: HashMap::new(),
            single_level: HashMap::new(),
            globs: HashMap::new(),
            with_regexes: Vec::new(),
        };
        for (idx, section) in sections.iter().enumerate() {
            if !section.regexes.is_empty() {
                matcher.with_regexes.push(idx);
            }
            for pattern in &section.files {
                let depth = pattern.depth();
                match pattern {
                    CompiledPattern::ExactFile { path, .. } => {
                        matcher.exact.entry(path).or_default().push((idx, depth));
                    }
                    CompiledPattern::PrefixDir { prefix, .. } => {
                        matcher
                            .prefix_dirs
                            .entry(prefix)
                            .or_default()
                            .push((idx, depth));
                    }
                    CompiledPattern::DirSingleLevel { dir_prefix, .. } => {
                        matcher
                            .single_level
                            .entry(dir_prefix)
                            .or_default()
                            .push((idx, depth));
                    }
                    CompiledPattern::Glob { regex, anchor, .. } => {
                        matcher
                            .globs
                            .entry(anchor)
                            .or_default()
                            .push((idx, regex, depth));
                    }
                }
            }
        }
        matcher
    }

    /// Matched section names for `file_path`, most specific first, exactly
    /// as [`MaintainersIndex::match_file`] returns them.
    pub fn match_file(&self, file_path: &str) -> Vec<String> {
        let path = file_path.trim_start_matches("./");
        let mut best: HashMap<usize, usize> = HashMap::new();
        let mut record = |idx: usize, depth: usize| {
            best.entry(idx)
                .and_modify(|d| *d = (*d).max(depth))
                .or_insert(depth);
        };

        for &(idx, depth) in self.exact.get(path).into_iter().flatten() {
            record(idx, depth);
        }
        let parent_end = path.rfind('/').map_or(0, |i| i + 1);
        for &(idx, depth) in self
            .single_level
            .get(&path[..parent_end])
            .into_iter()
            .flatten()
        {
            record(idx, depth);
        }
        // Every ancestor directory prefix ("a/", "a/b/", ...), plus the
        // empty anchor of globs that start with a wildcard.
        let ancestors = std::iter::once(0)
            .chain(path.match_indices('/').map(|(i, _)| i + 1))
            .map(|end| &path[..end]);
        for prefix in ancestors {
            for &(idx, depth) in self.prefix_dirs.get(prefix).into_iter().flatten() {
                record(idx, depth);
            }
            for &(idx, regex, depth) in self.globs.get(prefix).into_iter().flatten() {
                if regex.is_match(path) {
                    record(idx, depth);
                }
            }
        }

        let excluded = |idx: usize| self.sections[idx].excludes.iter().any(|x| x.is_match(path));
        best.retain(|idx, _| !excluded(*idx));
        for &idx in &self.with_regexes {
            if !best.contains_key(&idx)
                && !excluded(idx)
                && self.sections[idx]
                    .regexes
                    .iter()
                    .any(|re| re.is_match(path))
            {
                best.insert(idx, 0);
            }
        }

        let mut matches: Vec<(&MaintainerSection, usize)> = best
            .into_iter()
            .map(|(idx, depth)| (&self.sections[idx], depth))
            .collect();
        matches.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
        matches
            .into_iter()
            .map(|(sec, _)| sec.name.clone())
            .collect()
    }
}

/// In-memory index of all parsed MAINTAINERS sections for high-performance matching.
#[derive(Debug, Clone, Default)]
pub struct MaintainersIndex {
    sections: Vec<MaintainerSection>,
    /// Every section title an address is listed against, keyed by lowercased
    /// address. Built once so that resolving a caller's authority costs one
    /// hash lookup rather than a scan of several thousand sections.
    subsystems_by_address: HashMap<String, BTreeSet<String>>,
    /// Addresses listed on a section that claims the whole tree.
    global_addresses: HashSet<String>,
}

impl MaintainersIndex {
    /// Creates a new empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the index, including the address lookups derived from it.
    fn from_sections(sections: Vec<MaintainerSection>) -> Self {
        let mut subsystems_by_address: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut global_addresses = HashSet::new();
        for section in &sections {
            for address in section.maintainer_addresses() {
                if section.catch_all {
                    global_addresses.insert(address.clone());
                }
                subsystems_by_address
                    .entry(address)
                    .or_default()
                    .insert(section.name.clone());
            }
        }
        Self {
            sections,
            subsystems_by_address,
            global_addresses,
        }
    }

    /// The section titles the given address is listed against.
    ///
    /// The address is matched case insensitively, since it arrives from a
    /// sign-in form rather than from the file.
    pub fn subsystems_for_address(&self, address: &str) -> Option<&BTreeSet<String>> {
        self.subsystems_by_address
            .get(&address.trim().to_ascii_lowercase())
    }

    /// Whether the address is listed on a section that claims the whole tree,
    /// which today means THE REST and therefore Linus Torvalds.
    ///
    /// Such a maintainer is responsible for every file, so scoping them to the
    /// handful of top-level paths their patterns literally match would be an
    /// accident of the pattern syntax rather than a decision.
    pub fn is_global_maintainer(&self, address: &str) -> bool {
        self.global_addresses
            .contains(&address.trim().to_ascii_lowercase())
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

    /// Loads and parses MAINTAINERS from the top-of-trunk of Linus's tree in the repository.
    /// Tries `origin/master:MAINTAINERS`, `master:MAINTAINERS`, `HEAD:MAINTAINERS`,
    /// and falls back to reading the `MAINTAINERS` file directly on disk.
    pub fn from_top_of_trunk<P: AsRef<Path>>(repo_path: P) -> Result<Self> {
        let repo = repo_path.as_ref();
        for git_ref in [
            "origin/master:MAINTAINERS",
            "master:MAINTAINERS",
            "HEAD:MAINTAINERS",
        ] {
            let output = crate::git_cmd::in_dir(repo)
                .args(["show", git_ref])
                .output();
            if let Ok(out) = output
                && out.status.success()
                && !out.stdout.is_empty()
            {
                info!("Loaded MAINTAINERS from top-of-trunk ref {}", git_ref);
                let reader = BufReader::new(&out.stdout[..]);
                return Self::from_reader(reader);
            }
        }

        // Fallback: Read file directly on disk
        let file_path = repo.join("MAINTAINERS");
        if file_path.exists() {
            info!(
                "Falling back to loading MAINTAINERS directly from {:?}",
                file_path
            );
            return Self::from_file(&file_path);
        }

        Err(anyhow::anyhow!(
            "Failed to locate or read MAINTAINERS from repository at {:?}",
            repo
        ))
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
        let mut current_catch_all = false;

        let mut in_header = true;

        for line_res in reader.lines() {
            let line = line_res?;
            let trimmed = line.trim();

            if in_header {
                if trimmed.eq_ignore_ascii_case("maintainers list") {
                    in_header = false;
                }
                continue; // Skip everything else in the header
            }

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
                        catch_all: std::mem::take(&mut current_catch_all),
                    });
                }
                current_name.clear();
                continue;
            }

            // Skip leading comments
            if trimmed.starts_with('#') {
                continue;
            }

            // Check for tag line: `[A-Z]: <value>`
            if let Some((tag, value)) = trimmed.split_once(':')
                && tag.len() == 1
                && tag.chars().next().unwrap().is_ascii_uppercase()
            {
                let val = value.trim();
                match tag {
                    "F" => {
                        // A pattern of "*" or "*/" is how the file spells "the
                        // whole tree"; neither survives compilation as such.
                        current_catch_all |= val == "*" || val == "*/";
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
                // Section title
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
                catch_all: current_catch_all,
            });
        }

        info!("Loaded and indexed {} MAINTAINERS sections", sections.len());
        Ok(Self::from_sections(sections))
    }

    /// Builds a [`FileMatcher`] for matching many paths against this index.
    pub fn file_matcher(&self) -> FileMatcher<'_> {
        FileMatcher::new(&self.sections)
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

static GLOBAL_MAINTAINERS: RwLock<Option<Arc<MaintainersIndex>>> = RwLock::new(None);

/// Initializes the global MAINTAINERS index loaded from the top-of-trunk of Linus's tree.
/// Can only be set once at startup and is never altered during normal work.
pub fn init_global_maintainers(index: Arc<MaintainersIndex>) {
    let mut guard = GLOBAL_MAINTAINERS.write().unwrap();
    if guard.is_none() {
        *guard = Some(index);
    }
}

/// Clears the global MAINTAINERS index. Intended for test cleanup.
pub fn clear_global_maintainers() {
    let mut guard = GLOBAL_MAINTAINERS.write().unwrap();
    *guard = None;
}

/// Returns a reference to the global immutable MAINTAINERS index.
pub fn get_global_maintainers() -> Option<Arc<MaintainersIndex>> {
    GLOBAL_MAINTAINERS.read().unwrap().clone()
}

/// Serializes tests that depend on the global index.
///
/// The index is process wide and the unit tests share one process, so a test
/// that installs an index races both the test that clears it and any test that
/// merely reads it. Every test that touches [`init_global_maintainers`],
/// [`clear_global_maintainers`], or code that consults the index must hold
/// this for its whole body.
///
/// Async aware, because most of those tests await between installing the index
/// and asserting on what it produced. Blocking tests take it with
/// `blocking_lock`, which is sound only because they have no runtime of their
/// own to stall.
#[cfg(test)]
pub static GLOBAL_INDEX_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The MAINTAINERS sections that own the files a diff touches.
///
/// This is the one place that turns changed code into the people responsible
/// for it, so that every caller attributing a patchset agrees on the answer.
///
/// An empty result means nobody was identified, and callers must read it that
/// way. It is returned both when no index is loaded and when nothing matched,
/// and the two are deliberately indistinguishable: a caller that could tell
/// them apart would be tempted to fall back to something weaker, such as a
/// directory prefix, and a directory prefix names nobody. Attributing a series
/// to a guess is worse than attributing it to no one, because the guess would
/// silently hand the series' transcripts to whoever happened to match.
pub fn sections_for_diff(diff: &str) -> Vec<String> {
    let files = crate::baseline::extract_files_from_diff(diff);
    if files.is_empty() {
        return Vec::new();
    }
    match get_global_maintainers() {
        Some(index) => index.match_files(&files),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_matcher_agrees_with_match_file() {
        let maintainers = "\
Maintainers List
----------------

ARM PORT
M:\tA <a@example.com>
F:\tarch/arm/
F:\tarch/*/include/asm/io.h
X:\tarch/arm/boot/dts/

ARM DTS
M:\tB <b@example.com>
F:\tarch/arm/boot/dts/*
F:\tarch/arm/boot/dts/**/*.dtsi

NET
M:\tC <c@example.com>
F:\tnet/
F:\tinclude/net/*
F:\t*/net/*.h
F:\t./drivers/net/Kconfig
X:\tnet/sched/
N:\tnetdev

SCHED
M:\tD <d@example.com>
F:\tnet/sched/sch_?fq.c
N:\t[^a-z]sched[^a-z]

TOPLEVEL
M:\tE <e@example.com>
F:\tMakefile
F:\t*.txt

THE REST
M:\tF <f@example.com>
F:\t*
F:\t*/
";
        let index = MaintainersIndex::from_reader(maintainers.as_bytes()).unwrap();
        let matcher = index.file_matcher();
        let paths = [
            "arch/arm/kernel/setup.c",
            "arch/arm/boot/dts/foo.dts",
            "arch/arm/boot/dts/vendor/board.dtsi",
            "arch/x86/include/asm/io.h",
            "arch/x86/include/asm/io.c",
            "net/core/dev.c",
            "net/sched/sch_ffq.c",
            "net/sched/sch_fq.c",
            "net/sched/cls_api.c",
            "include/net/sock.h",
            "include/net/sub/x.h",
            "drivers/net/Kconfig",
            "./drivers/net/Kconfig",
            "drivers/infiniband/net/netdev_x.h",
            "tools/netdev/main.c",
            "Makefile",
            "README.txt",
            "Documentation/x.txt",
            "lib/sched/x.c",
            "kernel/fork.c",
        ];
        for path in paths {
            assert_eq!(
                matcher.match_file(path),
                index.match_file(path),
                "matchers disagree for {path}"
            );
        }
        assert!(
            matcher
                .match_file("net/sched/cls_api.c")
                .contains(&"SCHED".to_string())
        );
        assert!(
            !matcher
                .match_file("net/sched/cls_api.c")
                .contains(&"NET".to_string())
        );
    }

    const SAMPLE_MAINTAINERS: &str = r#"
Maintainers List
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
R:	Chris Mason <clm@fb.com>
L:	linux-mm@kvack.org
S:	Maintained
F:	mm/
F:	include/linux/mm*

THE REST
M:	Linus Torvalds <torvalds@linux-foundation.org>
S:	Buried alive in reporters
F:	*
F:	*/
"#;

    #[test]
    fn test_maintainer_address_handles_the_shapes_that_occur() {
        // Every shape below appears verbatim in the kernel MAINTAINERS file.
        assert_eq!(
            maintainer_address("Linus Torvalds <torvalds@linux-foundation.org>").as_deref(),
            Some("torvalds@linux-foundation.org")
        );
        assert_eq!(
            maintainer_address("\"Rafael J. Wysocki\" <rafael@kernel.org>").as_deref(),
            Some("rafael@kernel.org")
        );
        assert_eq!(
            maintainer_address("linux@roeck-us.net").as_deref(),
            Some("linux@roeck-us.net")
        );
        // The address is compared against whatever a person types into the
        // sign-in form, so case must not decide whether they get in.
        assert_eq!(
            maintainer_address("Guenter Roeck <LINUX@Roeck-us.NET>").as_deref(),
            Some("linux@roeck-us.net")
        );
    }

    #[test]
    fn test_maintainer_address_rejects_entries_that_are_not_addresses() {
        for entry in [
            "",
            "Just A Name",
            "<>",
            "Name <>",
            "Name <not an address>",
            "@no-local-part.org",
            "two@addresses@example.org",
            "no-dot@localhost",
        ] {
            assert_eq!(maintainer_address(entry), None, "entry: {entry:?}");
        }
    }

    #[test]
    fn test_section_maintainer_addresses_are_extracted() {
        let index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();
        let btrfs = index
            .sections()
            .iter()
            .find(|s| s.name == "BTRFS FILE SYSTEM")
            .unwrap();
        assert_eq!(
            btrfs.maintainer_addresses().collect::<Vec<_>>(),
            vec!["clm@fb.com".to_string()]
        );
    }

    #[test]
    fn test_parse_maintainers() {
        let index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();
        assert_eq!(index.len(), 6);
    }

    #[test]
    fn test_subsystems_for_address_covers_maintainers_and_reviewers() {
        let index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();
        // Listed once as a maintainer and once as a reviewer; both count.
        assert_eq!(
            index
                .subsystems_for_address("clm@fb.com")
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                "BTRFS FILE SYSTEM".to_string(),
                "MEMORY MANAGEMENT".to_string()
            ]
        );
        // The address arrives from a sign-in form, not from the file.
        assert!(index.subsystems_for_address("  CLM@FB.COM ").is_some());
        assert!(index.subsystems_for_address("nobody@example.org").is_none());
    }

    #[test]
    fn test_catch_all_section_confers_global_scope() {
        let index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();
        assert!(index.is_global_maintainer("torvalds@linux-foundation.org"));
        // A subsystem maintainer, however senior, is not global.
        assert!(!index.is_global_maintainer("akpm@linux-foundation.org"));
        assert!(!index.is_global_maintainer("nobody@example.org"));
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

    /// Also covers `sections_for_diff`, which reads the same global index.
    #[test]
    fn test_global_maintainers_lifecycle() {
        let _guard = GLOBAL_INDEX_TEST_LOCK.blocking_lock();
        // init only fills an empty slot, so start from one.
        clear_global_maintainers();

        let index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();
        let arc = Arc::new(index);
        init_global_maintainers(arc.clone());

        let retrieved = get_global_maintainers().expect("Expected global maintainers to be set");
        assert_eq!(retrieved.len(), 6);
        assert_eq!(
            retrieved.match_file("fs/btrfs/inode.c"),
            vec!["BTRFS FILE SYSTEM"]
        );

        // A diff is attributed to the sections owning every file it touches.
        let diff = "diff --git a/fs/btrfs/inode.c b/fs/btrfs/inode.c\n\
                    @@ -1 +1 @@\n\
                    diff --git a/net/core/dev.c b/net/core/dev.c\n\
                    @@ -1 +1 @@\n";
        let mut sections = sections_for_diff(diff);
        sections.sort();
        assert_eq!(
            sections,
            vec![
                "BTRFS FILE SYSTEM".to_string(),
                "NETWORKING [GENERAL]".to_string()
            ]
        );

        // A file no section claims attributes the diff to nobody rather than
        // to a directory prefix standing in for one.
        assert!(
            sections_for_diff("diff --git a/unclaimed/thing.c b/unclaimed/thing.c\n").is_empty()
        );

        clear_global_maintainers();
        assert!(get_global_maintainers().is_none());

        // Without an index nothing is attributed, so a series ingested before
        // MAINTAINERS loads stays closed rather than open.
        assert!(sections_for_diff(diff).is_empty());
    }

    /// Returns before the global index is consulted, so this cannot race the
    /// test above.
    #[test]
    fn test_sections_for_diff_without_file_names() {
        assert!(sections_for_diff("").is_empty());
        assert!(sections_for_diff("just a cover letter body\n").is_empty());
    }
}
