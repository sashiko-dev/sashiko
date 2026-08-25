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

//! Generic prompt templating engine with variable substitution and file inclusion directives.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;

/// Dynamic variable extractor mapping workflow state `S` to a string value.
pub type PromptVarExtractor<S> = Box<dyn Fn(&S) -> String + Send + Sync>;

/// Dynamic file inclusion resolver mapping workflow state `S` to a list of paths.
pub type DynamicInclusionResolver<S> = Box<dyn Fn(&S) -> Vec<PathBuf> + Send + Sync>;

/// Directory filter function.
pub type DirFilter = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// A directive to include external files or directories into a prompt.
pub enum InclusionDirective {
    /// Include a single file relative to the prompt base directory.
    File(PathBuf),
    /// Include all matching markdown files from a directory.
    Directory { path: PathBuf, filter: DirFilter },
}

/// A declarative prompt template parameterized over workflow state `S`.
pub struct PromptTemplate<S> {
    raw_template: String,
    vars: Vec<(String, PromptVarExtractor<S>)>,
    static_inclusions: Vec<InclusionDirective>,
    dynamic_inclusions: Vec<DynamicInclusionResolver<S>>,
}

/// Placeholder for files resolved from state, which carry no path in the
/// template and so cannot be named by an include directive.
const DYNAMIC_INCLUDE_MARKER: &str = "@includes";

fn include_directive(path: &Path) -> String {
    format!("@include(\"{}\")", path.display())
}

/// A rendered prompt in two kinds of piece. Directives are only ever matched
/// against `Template`, because a patch under review can contain a directive as
/// part of its own text and must not be able to place a file.
enum Segment {
    Template(String),
    Included(String),
}

/// Splits the first template segment holding `directive` and drops `block` in
/// its place. Returns false when no segment names it, leaving `block` for the
/// caller to append.
fn place(segments: &mut Vec<Segment>, directive: &str, block: String) -> bool {
    for i in 0..segments.len() {
        let Segment::Template(text) = &segments[i] else {
            continue;
        };
        let Some(pos) = text.find(directive) else {
            continue;
        };
        let before = text[..pos].to_string();
        let after = text[pos + directive.len()..].to_string();
        segments.splice(
            i..=i,
            [
                Segment::Template(before),
                Segment::Included(block),
                Segment::Template(after),
            ],
        );
        return true;
    }
    false
}

impl<S> PromptTemplate<S> {
    /// Creates a new prompt template from a raw string.
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            raw_template: template.into(),
            vars: Vec::new(),
            static_inclusions: Vec::new(),
            dynamic_inclusions: Vec::new(),
        }
    }

    /// Binds a variable placeholder `{{key}}` to an extractor function from `&S`.
    pub fn with_var<F>(mut self, key: &str, extractor: F) -> Self
    where
        F: Fn(&S) -> String + Send + Sync + 'static,
    {
        self.vars.push((key.to_string(), Box::new(extractor)));
        self
    }

    /// Adds a static file inclusion directive.
    pub fn include_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.static_inclusions
            .push(InclusionDirective::File(path.into()));
        self
    }

    /// Adds a static directory inclusion directive with a filename filter.
    pub fn include_dir<F>(mut self, dir: impl Into<PathBuf>, filter: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.static_inclusions.push(InclusionDirective::Directory {
            path: dir.into(),
            filter: Box::new(filter),
        });
        self
    }

    /// Adds dynamic file inclusions resolved from workflow state `S` at runtime.
    pub fn include_files_from_state<F>(mut self, resolver: F) -> Self
    where
        F: Fn(&S) -> Vec<PathBuf> + Send + Sync + 'static,
    {
        self.dynamic_inclusions.push(Box::new(resolver));
        self
    }

    /// Renders the expanded prompt for sending to the LLM (expanding file contents).
    pub async fn render_for_model(&self, state: &S, base_dir: &Path) -> Result<String> {
        let mut segments = vec![Segment::Template(self.raw_template.clone())];
        let mut trailing = String::new();

        for inclusion in &self.static_inclusions {
            let (path, block) = match inclusion {
                InclusionDirective::File(path) => {
                    // A directive naming a file that is not there resolves to
                    // nothing. It must still be consumed, or it reaches the
                    // model as literal text.
                    let full_path = base_dir.join(path);
                    let block = if full_path.exists() {
                        let content = fs::read_to_string(&full_path)
                            .await
                            .with_context(|| format!("Failed to read file: {:?}", full_path))?;
                        format!("\n\n# {}\n{}\n", path.display(), content)
                    } else {
                        String::new()
                    };
                    (path, block)
                }
                InclusionDirective::Directory { path, filter } => {
                    let dir_path = base_dir.join(path);
                    if !dir_path.exists() {
                        place(&mut segments, &include_directive(path), String::new());
                        continue;
                    }
                    let mut entries = fs::read_dir(&dir_path).await?;
                    let mut file_paths = Vec::new();
                    while let Some(entry) = entries.next_entry().await? {
                        let p = entry.path();
                        if p.extension().is_some_and(|ext| ext == "md")
                            && let Some(name) = p.file_name().and_then(|n| n.to_str())
                            && filter(name)
                        {
                            file_paths.push(p);
                        }
                    }
                    file_paths.sort();
                    let mut block = String::new();
                    for file_path in file_paths {
                        let name = file_path
                            .strip_prefix(base_dir)
                            .unwrap_or(&file_path)
                            .to_string_lossy();
                        let content = fs::read_to_string(&file_path).await?;
                        block.push_str(&format!("\n\n## {}\n{}\n", name, content));
                    }
                    (path, block)
                }
            };
            if !place(&mut segments, &include_directive(path), block.clone()) {
                trailing.push_str(&block);
            }
        }

        let mut dynamic = String::new();
        for dyn_inc in &self.dynamic_inclusions {
            for path in dyn_inc(state) {
                let full_path = base_dir.join(&path);
                if full_path.exists() {
                    let content = fs::read_to_string(&full_path)
                        .await
                        .with_context(|| format!("Failed to read dynamic file: {:?}", full_path))?;
                    dynamic.push_str(&format!("\n\n# {}\n{}\n", path.display(), content));
                }
            }
        }
        if !place(&mut segments, DYNAMIC_INCLUDE_MARKER, dynamic.clone()) {
            trailing.push_str(&dynamic);
        }

        Ok(self.assemble(segments, trailing, state))
    }

    /// Renders the compact prompt for storage in logs/database (keeping `@directives`).
    pub fn render_for_log(&self, state: &S) -> String {
        let mut segments = vec![Segment::Template(self.raw_template.clone())];
        let mut trailing = String::new();

        for inclusion in &self.static_inclusions {
            let (path, token) = match inclusion {
                InclusionDirective::File(path) => (path, format!("\n\n@{}\n", path.display())),
                InclusionDirective::Directory { path, .. } => {
                    (path, format!("\n\n@{}/\n", path.display()))
                }
            };
            if !place(&mut segments, &include_directive(path), token.clone()) {
                trailing.push_str(&token);
            }
        }

        let mut dynamic = String::new();
        for dyn_inc in &self.dynamic_inclusions {
            let files = dyn_inc(state);
            if !files.is_empty() {
                let tags: Vec<String> = files.iter().map(|p| format!("@{}", p.display())).collect();
                dynamic.push_str(&format!("\n\n{}\n", tags.join(", ")));
            }
        }
        if !place(&mut segments, DYNAMIC_INCLUDE_MARKER, dynamic.clone()) {
            trailing.push_str(&dynamic);
        }

        self.assemble(segments, trailing, state)
    }

    /// Substitutes variables into the template pieces only, then joins. An
    /// included file is inserted as it is, and a variable's value is never
    /// scanned for directives.
    fn assemble(&self, segments: Vec<Segment>, trailing: String, state: &S) -> String {
        let mut out = String::new();
        for segment in segments {
            match segment {
                Segment::Template(text) => out.push_str(&self.substitute_vars(&text, state)),
                Segment::Included(block) => out.push_str(&block),
            }
        }
        out.push_str(&trailing);
        out
    }

    fn substitute_vars(&self, text: &str, state: &S) -> String {
        let mut text = text.to_string();
        for (key, extractor) in &self.vars {
            let pattern = format!("{{{{{}}}}}", key);
            text = text.replace(&pattern, &extractor(state));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    struct TestState {
        diff: String,
        extra_guides: Vec<PathBuf>,
    }

    #[tokio::test]
    async fn test_prompt_template_variable_substitution() {
        let state = TestState {
            diff: "+int x = 42;".to_string(),
            extra_guides: vec![],
        };

        let template = PromptTemplate::<TestState>::new("Patch diff:\n{{diff}}")
            .with_var("diff", |s| s.diff.clone());

        let temp_dir = tempdir().unwrap();
        let rendered_model = template
            .render_for_model(&state, temp_dir.path())
            .await
            .unwrap();
        let rendered_log = template.render_for_log(&state);

        assert_eq!(rendered_model, "Patch diff:\n+int x = 42;");
        assert_eq!(rendered_log, "Patch diff:\n+int x = 42;");
    }

    #[tokio::test]
    async fn test_a_directive_for_a_missing_file_is_still_consumed() {
        // The pre-screen prompt names subsystem.md, which Phase 0 tolerated
        // being absent. An unexpanded directive would reach the model as text.
        let temp_dir = tempdir().unwrap();
        let state = TestState {
            diff: "foo".to_string(),
            extra_guides: vec![],
        };

        let template =
            PromptTemplate::<TestState>::new("<guides>\n@include(\"absent.md\")\n</guides>")
                .include_file("absent.md");

        let rendered = template
            .render_for_model(&state, temp_dir.path())
            .await
            .unwrap();

        assert!(
            !rendered.contains("@include"),
            "the directive is gone:\n{rendered}"
        );
        assert!(rendered.contains("<guides>") && rendered.contains("</guides>"));
    }

    #[tokio::test]
    async fn test_a_variable_value_cannot_place_a_file() {
        // Sashiko reviews arbitrary patches, so a diff can carry a directive as
        // its own text. Only the template may say where a file goes.
        let temp_dir = tempdir().unwrap();
        fs::write(temp_dir.path().join("locking.md"), "Locking rules")
            .await
            .unwrap();

        let state = TestState {
            diff: "+// @include(\"locking.md\")\n+@includes".to_string(),
            extra_guides: vec![],
        };

        let template = PromptTemplate::<TestState>::new("Patch: {{diff}}")
            .with_var("diff", |s| s.diff.clone())
            .include_file("locking.md");

        let rendered = template
            .render_for_model(&state, temp_dir.path())
            .await
            .unwrap();

        assert!(
            rendered.contains("+// @include(\"locking.md\")") && rendered.contains("+@includes"),
            "a directive in the diff stays text:\n{rendered}"
        );
        assert_eq!(
            rendered.matches("Locking rules").count(),
            1,
            "the file is placed once:\n{rendered}"
        );
        assert!(
            rendered.find("+@includes").unwrap() < rendered.find("Locking rules").unwrap(),
            "and appended, since the template names no place:\n{rendered}"
        );
    }

    #[tokio::test]
    async fn test_inclusions_render_where_the_template_names_them() {
        let temp_dir = tempdir().unwrap();
        fs::write(temp_dir.path().join("locking.md"), "Locking rules")
            .await
            .unwrap();
        fs::write(temp_dir.path().join("security.md"), "Security rules")
            .await
            .unwrap();

        let state = TestState {
            diff: "foo".to_string(),
            extra_guides: vec![PathBuf::from("security.md")],
        };

        let template = PromptTemplate::<TestState>::new(
            "<guides>\n@include(\"locking.md\")\n@includes\n</guides>\n\nPatch: {{diff}}",
        )
        .with_var("diff", |s| s.diff.clone())
        .include_file("locking.md")
        .include_files_from_state(|s| s.extra_guides.clone());

        let rendered = template
            .render_for_model(&state, temp_dir.path())
            .await
            .unwrap();

        let guides_end = rendered.find("</guides>").expect("closing tag survives");
        assert!(
            rendered.find("Locking rules").unwrap() < guides_end,
            "the named file lands inside the block it is named in:\n{rendered}"
        );
        assert!(
            rendered.find("Security rules").unwrap() < guides_end,
            "the state-resolved file lands at the marker:\n{rendered}"
        );
        assert!(
            !rendered.contains("@include(\"locking.md\")") && !rendered.contains("@includes"),
            "no directive survives into the prompt:\n{rendered}"
        );

        let logged = template.render_for_log(&state);
        assert!(logged.find("@locking.md").unwrap() < logged.find("</guides>").unwrap());
    }

    #[tokio::test]
    async fn test_prompt_template_static_and_dynamic_inclusions() {
        let temp_dir = tempdir().unwrap();
        let guide_path = temp_dir.path().join("locking.md");
        fs::write(&guide_path, "Locking rules").await.unwrap();

        let dyn_guide_path = temp_dir.path().join("security.md");
        fs::write(&dyn_guide_path, "Security rules").await.unwrap();

        let state = TestState {
            diff: "foo".to_string(),
            extra_guides: vec![PathBuf::from("security.md")],
        };

        let template = PromptTemplate::<TestState>::new("Review patch: {{diff}}")
            .with_var("diff", |s| s.diff.clone())
            .include_file("locking.md")
            .include_files_from_state(|s| s.extra_guides.clone());

        let rendered_model = template
            .render_for_model(&state, temp_dir.path())
            .await
            .unwrap();
        let rendered_log = template.render_for_log(&state);

        assert!(rendered_model.contains("Review patch: foo"));
        assert!(rendered_model.contains("# locking.md\nLocking rules"));
        assert!(rendered_model.contains("# security.md\nSecurity rules"));

        assert!(rendered_log.contains("Review patch: foo"));
        assert!(rendered_log.contains("@locking.md"));
        assert!(rendered_log.contains("@security.md"));
        assert!(!rendered_log.contains("Locking rules"));
    }
}
