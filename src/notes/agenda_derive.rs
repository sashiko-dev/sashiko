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

//! Derives a project's agenda from its analyzed fix corpus (design section
//! 9.1): map knowledge items to themes, merge themes level by level, and
//! write agenda modules weighted by how many fixes needed each theme. Each
//! level is saved in the work directory, so a rerun resumes after the last
//! finished level.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::notes::agenda::parse_agenda_module;
use crate::notes::build::Usage;
use crate::notes::corpus::{CorpusMeta, CorpusRecord};
use crate::workflow::{Workflow, WorkflowEngine, WorkflowEnv};
use crate::workflows::code_notes::LessonKind;
use crate::workflows::notes_agenda::{
    AGENDA_STAGE_NAMES, AgendaState, KnowledgeItem, MAP_BATCH_SIZE, MAX_FINAL_THEMES,
    MERGE_BATCH_SIZE, ModuleSpec, THEME_EXAMPLES, Theme, ThemesOutput, agenda_workflow,
    group_stage, render_items, render_module, render_themes, write_stage,
};

/// Merge levels after which the reduction stops even if many themes
/// remain; the write stage then sees the largest of them.
const MAX_MERGE_LEVELS: usize = 8;

/// Settings of one agenda derivation.
#[derive(Debug, Clone)]
pub struct DeriveConfig {
    pub project: String,
    pub temperature: f32,
    pub modules: Vec<ModuleSpec>,
    pub out_dir: PathBuf,
    pub work_dir: PathBuf,
}

/// What a derivation produced.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeriveSummary {
    pub fixes: usize,
    pub code_bugs: usize,
    pub themes: usize,
    pub written: Vec<String>,
    pub usage: Usage,
}

/// Every record of the corpus files in `dir`, once per fix, with the
/// corpus metadata.
pub fn read_corpus(dir: &Path) -> Result<(CorpusMeta, Vec<CorpusRecord>)> {
    let meta = CorpusMeta::load(dir)?.with_context(|| {
        format!(
            "{} has no corpus metadata; run sashiko notes corpus first",
            dir.display()
        )
    })?;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to list {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("corpus-") && n.ends_with(".jsonl"))
        })
        .collect();
    paths.sort();
    let mut seen = std::collections::HashSet::new();
    let mut records = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<CorpusRecord>(line) {
                Ok(record) if seen.insert(record.fix.clone()) => records.push(record),
                Ok(_) => {}
                Err(err) => warn!("{}: skipping unreadable record: {err}", path.display()),
            }
        }
    }
    Ok((meta, records))
}

/// Knowledge items of the code bugs, in a seeded order that mixes areas
/// of the project within every batch.
pub fn knowledge_items(records: &[CorpusRecord]) -> Vec<KnowledgeItem> {
    let mut keyed: Vec<([u8; 32], KnowledgeItem)> = records
        .iter()
        .filter(|r| r.analysis.kind == LessonKind::CodeBug)
        .map(|r| {
            (
                Sha256::digest(r.fix.as_bytes()).into(),
                KnowledgeItem {
                    category: r.analysis.category.trim().to_string(),
                    rule: r.analysis.violated_rule.trim().to_string(),
                    knowledge: r.analysis.knowledge_needed.trim().to_string(),
                },
            )
        })
        .collect();
    keyed.sort_by_key(|entry| entry.0);
    keyed.into_iter().map(|(_, item)| item).collect()
}

/// Derives the agenda and writes its modules into `cfg.out_dir`.
pub async fn derive_agenda(
    env: &WorkflowEnv<'_>,
    meta: &CorpusMeta,
    records: &[CorpusRecord],
    cfg: &DeriveConfig,
) -> Result<DeriveSummary> {
    std::fs::create_dir_all(&cfg.work_dir)
        .with_context(|| format!("failed to create {}", cfg.work_dir.display()))?;
    let items = knowledge_items(records);
    if items.is_empty() {
        bail!("the corpus has no analyzed code bugs");
    }
    let mut summary = DeriveSummary {
        fixes: records.len(),
        code_bugs: items.len(),
        ..Default::default()
    };

    // Map: knowledge items to themes.
    let mut themes: Vec<Theme> = match load(&cfg.work_dir, "themes-0")? {
        Some(saved) => saved,
        None => {
            let batches: Vec<Vec<KnowledgeItem>> =
                items.chunks(MAP_BATCH_SIZE).map(<[_]>::to_vec).collect();
            let grouped = run_groups(env, cfg, false, &batches, &mut summary.usage, |b| {
                (render_items(b), b.len())
            })
            .await?;
            let mut themes = Vec::new();
            for (batch, out) in grouped {
                themes.extend(map_themes(&batches[batch], out));
            }
            save(&cfg.work_dir, "themes-0", &themes)?;
            themes
        }
    };
    info!("{} themes from {} code bugs", themes.len(), items.len());

    // Reduce: merge themes until few remain.
    for level in 1..=MAX_MERGE_LEVELS {
        if themes.len() <= MAX_FINAL_THEMES {
            break;
        }
        let name = format!("themes-{level}");
        themes = match load(&cfg.work_dir, &name)? {
            Some(saved) => saved,
            None => {
                themes.sort_by(|a, b| (a.scope as u8, &a.title).cmp(&(b.scope as u8, &b.title)));
                let batches: Vec<Vec<Theme>> =
                    themes.chunks(MERGE_BATCH_SIZE).map(<[_]>::to_vec).collect();
                let grouped = run_groups(env, cfg, true, &batches, &mut summary.usage, |b| {
                    (render_themes(b), b.len())
                })
                .await?;
                let mut merged = Vec::new();
                let mut done = vec![false; batches.len()];
                for (batch, out) in grouped {
                    done[batch] = true;
                    merged.extend(merge_themes(&batches[batch], out));
                }
                // A batch whose stage failed carries its themes over.
                for (batch, ok) in batches.iter().zip(done) {
                    if !ok {
                        merged.extend(batch.iter().cloned());
                    }
                }
                save(&cfg.work_dir, &name, &merged)?;
                merged
            }
        };
        info!("merge level {level}: {} themes", themes.len());
    }
    themes.sort_by(|a, b| b.count.cmp(&a.count).then(a.title.cmp(&b.title)));
    themes.truncate(MAX_FINAL_THEMES);
    summary.themes = themes.len();
    save(&cfg.work_dir, "themes-final", &themes)?;

    // Write the modules.
    let workflow = Workflow::builder("notes_agenda_write")
        .stage(write_stage(&themes, cfg.modules.clone(), cfg.temperature))
        .build();
    let mut state = AgendaState {
        project: cfg.project.clone(),
        ..Default::default()
    };
    let outcome = WorkflowEngine::execute(&workflow, env, &mut state, None).await?;
    summary.usage.add(&outcome);
    let draft = state.draft.context("the write stage produced no agenda")?;
    save(&cfg.work_dir, "draft", &draft)?;
    let provenance = format!(
        "Derived by `sashiko notes agenda` from {} fixes reachable from {} ({} code bugs, {} themes).",
        records.len(),
        meta.revision,
        items.len(),
        themes.len()
    );
    for module in &draft.modules {
        let text = render_module(module, &provenance);
        parse_agenda_module(&module.module, &text)
            .with_context(|| format!("rendered module {} does not parse", module.module))?;
        let path = cfg.out_dir.join(format!("{}.md", module.module));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, text)
            .with_context(|| format!("failed to write {}", path.display()))?;
        summary.written.push(path.display().to_string());
    }
    Ok(summary)
}

/// Runs one grouping stage per batch, in rounds of parallel stages, and
/// returns the outputs with their batch index. Failed batches are logged
/// and left out.
async fn run_groups<T>(
    env: &WorkflowEnv<'_>,
    cfg: &DeriveConfig,
    merge: bool,
    batches: &[Vec<T>],
    usage: &mut Usage,
    render: impl Fn(&[T]) -> (String, usize),
) -> Result<Vec<(usize, ThemesOutput)>> {
    let mut grouped = Vec::new();
    let indices: Vec<usize> = (0..batches.len()).collect();
    for round in indices.chunks(AGENDA_STAGE_NAMES.len()) {
        let stages = round
            .iter()
            .zip(AGENDA_STAGE_NAMES)
            .map(|(&batch, name)| {
                let (text, count) = render(&batches[batch]);
                group_stage(name, merge, batch, text, count, cfg.temperature)
            })
            .collect();
        let mut state = AgendaState {
            project: cfg.project.clone(),
            ..Default::default()
        };
        match WorkflowEngine::execute(&agenda_workflow(stages), env, &mut state, None).await {
            Ok(outcome) => usage.add(&outcome),
            Err(err) => warn!("agenda round failed: {err:#}"),
        }
        let failed = round.len() - state.grouped.len();
        if failed > 0 {
            warn!("{failed} of {} grouping stages failed", round.len());
        }
        grouped.extend(state.grouped);
    }
    Ok(grouped)
}

/// Themes of one map batch: counts are member counts, examples the first
/// members' rules.
fn map_themes(items: &[KnowledgeItem], out: ThemesOutput) -> Vec<Theme> {
    out.themes
        .into_iter()
        .map(|t| Theme {
            count: t.members.len(),
            examples: t
                .members
                .iter()
                .take(THEME_EXAMPLES)
                .filter_map(|&m| items.get(m).map(|i| i.rule.clone()))
                .collect(),
            title: t.title.trim().to_string(),
            knowledge: t.knowledge.trim().to_string(),
            scope: t.scope,
        })
        .collect()
}

/// Themes of one merge batch: counts are summed, examples taken from the
/// largest members.
fn merge_themes(themes: &[Theme], out: ThemesOutput) -> Vec<Theme> {
    out.themes
        .into_iter()
        .map(|t| {
            let mut members: Vec<&Theme> =
                t.members.iter().filter_map(|&m| themes.get(m)).collect();
            members.sort_by_key(|m| std::cmp::Reverse(m.count));
            Theme {
                count: members.iter().map(|m| m.count).sum(),
                examples: members
                    .iter()
                    .flat_map(|m| m.examples.iter().cloned())
                    .take(THEME_EXAMPLES)
                    .collect(),
                title: t.title.trim().to_string(),
                knowledge: t.knowledge.trim().to_string(),
                scope: t.scope,
            }
        })
        .collect()
}

fn save<T: Serialize + ?Sized>(dir: &Path, name: &str, value: &T) -> Result<()> {
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn load<T: DeserializeOwned>(dir: &Path, name: &str) -> Result<Option<T>> {
    let path = dir.join(format!("{name}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
        format!("failed to parse {}", path.display())
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::notes_agenda::{ThemeDraft, ThemeScope};

    fn theme(title: &str, count: usize, examples: &[&str]) -> Theme {
        Theme {
            title: title.to_string(),
            knowledge: "k".to_string(),
            scope: ThemeScope::Universal,
            count,
            examples: examples.iter().map(|e| e.to_string()).collect(),
        }
    }

    fn out(members: &[&[usize]]) -> ThemesOutput {
        ThemesOutput {
            themes: members
                .iter()
                .map(|m| ThemeDraft {
                    title: " merged ".to_string(),
                    knowledge: "k".to_string(),
                    scope: ThemeScope::Domain,
                    members: m.to_vec(),
                })
                .collect(),
        }
    }

    #[test]
    fn test_map_and_merge_keep_counts_exact() {
        let items: Vec<KnowledgeItem> = (0..4)
            .map(|i| KnowledgeItem {
                category: "c".to_string(),
                rule: format!("rule {i}"),
                knowledge: "k".to_string(),
            })
            .collect();
        let mapped = map_themes(&items, out(&[&[0, 2, 3], &[1]]));
        assert_eq!(mapped[0].count, 3);
        assert_eq!(mapped[0].examples, vec!["rule 0", "rule 2", "rule 3"]);
        assert_eq!(mapped[0].title, "merged");

        let themes = vec![
            theme("a", 2, &["a1", "a2"]),
            theme("b", 5, &["b1", "b2"]),
            theme("c", 1, &["c1"]),
        ];
        let merged = merge_themes(&themes, out(&[&[0, 1], &[2]]));
        assert_eq!(merged[0].count, 7);
        // Examples come from the largest member first.
        assert_eq!(merged[0].examples, vec!["b1", "b2", "a1"]);
        assert_eq!(merged[1].count, 1);
    }
}
