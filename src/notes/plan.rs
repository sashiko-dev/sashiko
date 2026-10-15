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

//! `sashiko notes plan`: the deterministic scoping of a notes build, with a
//! rough cost estimate, before any LLM call is made.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::maintainers::MaintainersIndex;
use crate::notes::components::split_unit;
use crate::notes::entry_points::{EntryPoint, extract_entry_points};
use crate::notes::profile::NotesProfile;
use crate::notes::scope::list_source_files;
use crate::notes::units::{Unit, assign_units, unit_source};

/// Approximate tokens per source line (about 30 bytes per line at the
/// toolbox's 3 bytes per token).
const TOKENS_PER_LINE: usize = 10;
/// How many times a component's code is expected to pass through model
/// context across survey, explore, verify and synthesis. Session history
/// is never pruned, so re-reads accumulate. These bounds are uncalibrated
/// guesses until the first unit has been built and measured.
const CONTEXT_PASSES_LOW: usize = 4;
const CONTEXT_PASSES_HIGH: usize = 20;
/// Fixed per-component overhead: prompts, agenda, schemas, tool results
/// outside the component.
const COMPONENT_OVERHEAD_LOW: usize = 30_000;
const COMPONENT_OVERHEAD_HIGH: usize = 150_000;
/// Entry points listed per component in the text report.
const SAMPLE_ENTRY_POINTS: usize = 6;

/// The full plan for one notes build.
#[derive(Debug, Clone, Serialize)]
pub struct PlanReport {
    pub revision: String,
    pub totals: PlanTotals,
    pub units: Vec<UnitPlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanTotals {
    pub units: usize,
    pub files: usize,
    pub lines: usize,
    pub components: usize,
    pub unassigned_files: usize,
    pub unassigned_lines: usize,
    pub estimated_input_tokens: TokenEstimate,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnitPlan {
    pub name: String,
    pub slug: String,
    pub files: usize,
    pub lines: usize,
    pub shared_files: usize,
    pub components: Vec<ComponentPlan>,
    pub estimated_input_tokens: TokenEstimate,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentPlan {
    pub name: String,
    pub files: Vec<String>,
    pub lines: usize,
    pub entry_points: Vec<EntryPoint>,
}

/// An uncalibrated input-token range.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TokenEstimate {
    pub low: usize,
    pub high: usize,
}

impl TokenEstimate {
    fn for_component(lines: usize) -> Self {
        let code = lines.saturating_mul(TOKENS_PER_LINE);
        Self {
            low: code
                .saturating_mul(CONTEXT_PASSES_LOW)
                .saturating_add(COMPONENT_OVERHEAD_LOW),
            high: code
                .saturating_mul(CONTEXT_PASSES_HIGH)
                .saturating_add(COMPONENT_OVERHEAD_HIGH),
        }
    }

    fn add(self, other: Self) -> Self {
        Self {
            low: self.low.saturating_add(other.low),
            high: self.high.saturating_add(other.high),
        }
    }
}

/// Builds the plan for commit `sha`. `unit_filter` selects units by name or
/// slug (case-insensitive); empty selects every unit.
pub async fn build_plan(
    repo: &Path,
    sha: &str,
    profile: &NotesProfile,
    maintainers: Option<&MaintainersIndex>,
    unit_filter: &[String],
) -> Result<PlanReport> {
    let files = list_source_files(repo, sha, &profile.compiled_languages()).await?;

    // Matching every file against the unit source and splitting units is
    // CPU-bound (tens of thousands of files for a kernel tree), so it runs
    // on the blocking pool.
    let units_config = profile.units.clone();
    let maintainers = maintainers.cloned();
    let filter = unit_filter.to_vec();
    let (units, unassigned) = tokio::task::spawn_blocking(move || -> Result<_> {
        let source = unit_source(&units_config, maintainers.as_ref())?;
        let assignment = assign_units(source.as_ref(), &files);
        Ok((
            select_units(assignment.units, &filter)?,
            assignment.unassigned,
        ))
    })
    .await
    .context("unit assignment panicked")??;

    let selected_files: Vec<_> = units.iter().flat_map(|u| u.files.clone()).collect();
    let entry_points = extract_entry_points(repo, sha, &selected_files).await?;

    let component_lines = profile.limits.component_lines;
    let unit_plans = tokio::task::spawn_blocking(move || {
        let mut entry_points_by_path: HashMap<&str, Vec<&EntryPoint>> = HashMap::new();
        for entry in &entry_points {
            entry_points_by_path
                .entry(entry.path.as_str())
                .or_default()
                .push(entry);
        }
        units
            .iter()
            .map(|unit| plan_unit(unit, component_lines, &entry_points_by_path))
            .collect::<Vec<UnitPlan>>()
    })
    .await
    .context("unit planning panicked")?;
    let totals = PlanTotals {
        units: unit_plans.len(),
        files: unit_plans.iter().map(|u| u.files).sum(),
        lines: unit_plans.iter().map(|u| u.lines).sum(),
        components: unit_plans.iter().map(|u| u.components.len()).sum(),
        unassigned_files: unassigned.len(),
        unassigned_lines: unassigned.iter().map(|f| f.lines).sum(),
        estimated_input_tokens: unit_plans.iter().fold(TokenEstimate::default(), |acc, u| {
            acc.add(u.estimated_input_tokens)
        }),
    };
    Ok(PlanReport {
        revision: sha.to_string(),
        totals,
        units: unit_plans,
    })
}

fn select_units(units: Vec<Unit>, filter: &[String]) -> Result<Vec<Unit>> {
    if filter.is_empty() {
        return Ok(units);
    }
    let mut selected = Vec::new();
    for wanted in filter {
        let found = units.iter().find(|u| {
            u.name.eq_ignore_ascii_case(wanted.trim()) || u.slug.eq_ignore_ascii_case(wanted.trim())
        });
        match found {
            Some(unit) if !selected.iter().any(|s: &Unit| s.slug == unit.slug) => {
                selected.push(unit.clone())
            }
            Some(_) => {}
            None => bail!(
                "no unit named {wanted:?}; run `sashiko notes plan` without --unit to list units"
            ),
        }
    }
    Ok(selected)
}

fn plan_unit(
    unit: &Unit,
    component_lines: usize,
    entry_points_by_path: &HashMap<&str, Vec<&EntryPoint>>,
) -> UnitPlan {
    let components: Vec<ComponentPlan> = split_unit(unit, component_lines)
        .into_iter()
        .map(|component| {
            let lines = component.lines();
            let files: Vec<String> = component.files.into_iter().map(|f| f.path).collect();
            let entry_points = files
                .iter()
                .flat_map(|f| entry_points_by_path.get(f.as_str()).into_iter().flatten())
                .map(|e| (*e).clone())
                .collect();
            ComponentPlan {
                name: component.name,
                files,
                lines,
                entry_points,
            }
        })
        .collect();
    let estimated_input_tokens = components.iter().fold(TokenEstimate::default(), |acc, c| {
        acc.add(TokenEstimate::for_component(c.lines))
    });
    UnitPlan {
        name: unit.name.clone(),
        slug: unit.slug.clone(),
        files: unit.files.len(),
        lines: unit.lines(),
        shared_files: unit.shared.len(),
        components,
        estimated_input_tokens,
    }
}

/// Human-readable plan report.
pub fn render_text(report: &PlanReport) -> String {
    let mut out = String::new();
    let t = &report.totals;
    let _ = writeln!(out, "Notes plan at {}", report.revision);
    let _ = writeln!(
        out,
        "{} units, {} components, {} files, {} lines; {} files ({} lines) unassigned",
        t.units, t.components, t.files, t.lines, t.unassigned_files, t.unassigned_lines
    );
    let _ = writeln!(
        out,
        "Estimated input tokens (uncalibrated): {} - {}",
        human_tokens(t.estimated_input_tokens.low),
        human_tokens(t.estimated_input_tokens.high)
    );
    for unit in &report.units {
        let _ = writeln!(
            out,
            "\n{} [{}]: {} files, {} lines, {} shared, ~{} - {} tokens",
            unit.name,
            unit.slug,
            unit.files,
            unit.lines,
            unit.shared_files,
            human_tokens(unit.estimated_input_tokens.low),
            human_tokens(unit.estimated_input_tokens.high)
        );
        for component in &unit.components {
            let sample: Vec<&str> = component
                .entry_points
                .iter()
                .take(SAMPLE_ENTRY_POINTS)
                .map(|e| e.name.as_str())
                .collect();
            let more = component.entry_points.len().saturating_sub(sample.len());
            let _ = write!(
                out,
                "  - {}: {} files, {} lines, {} entry points",
                component.name,
                component.files.len(),
                component.lines,
                component.entry_points.len()
            );
            if !sample.is_empty() {
                let _ = write!(out, " ({}", sample.join(", "));
                if more > 0 {
                    let _ = write!(out, ", +{more}");
                }
                let _ = write!(out, ")");
            }
            out.push('\n');
        }
    }
    out
}

fn human_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::scope::test_repo;

    const PROFILE: &str = r#"
[units]
source = "directories"
roots = ["src"]

[languages.globs]
rust = ["src/**.rs"]

[limits]
component_lines = 3
"#;

    #[tokio::test]
    async fn test_build_plan_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let sha = test_repo::commit_files(
            dir.path(),
            &[
                ("src/main.rs", "fn main() {}\n"),
                ("src/ai/a.rs", "pub fn a() {}\n// 2\n"),
                ("src/ai/b.rs", "pub struct B;\n// 2\n"),
                ("tests/t.rs", "fn t() {}\n"),
            ],
        );
        let profile = NotesProfile::from_toml_str(PROFILE).unwrap();
        let report = build_plan(dir.path(), &sha, &profile, None, &[])
            .await
            .unwrap();
        assert_eq!(report.totals.units, 2);
        assert_eq!(report.totals.files, 3);
        assert_eq!(report.totals.lines, 5);
        assert_eq!(report.totals.unassigned_files, 0);
        let ai = &report.units[1];
        assert_eq!(ai.name, "src/ai");
        // 4 lines over a 3-line limit: one component per file.
        assert_eq!(ai.components.len(), 2);
        assert_eq!(ai.components[0].entry_points[0].name, "a");
        assert!(ai.estimated_input_tokens.low < ai.estimated_input_tokens.high);

        let text = render_text(&report);
        assert!(text.contains("src/ai [src-ai]: 2 files, 4 lines"), "{text}");
        assert!(
            text.contains("src/ai/a.rs: 1 files, 2 lines, 1 entry points (a)"),
            "{text}"
        );

        let only_ai = build_plan(dir.path(), &sha, &profile, None, &["SRC-AI".to_string()])
            .await
            .unwrap();
        assert_eq!(only_ai.units.len(), 1);
        assert!(
            build_plan(dir.path(), &sha, &profile, None, &["nope".to_string()])
                .await
                .is_err()
        );
    }

    #[test]
    fn test_human_tokens() {
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(12_345), "12k");
        assert_eq!(human_tokens(2_500_000), "2.5M");
    }
}
