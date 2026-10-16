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

//! Drives a notes build for one unit (design section 7): survey, history,
//! explore, evidence check, verify, synthesize, write. Each step runs as
//! its own workflow and saves its result in the unit's work directory, so
//! an interrupted build resumes from the last finished step, and every LLM
//! interaction is kept there for audit.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{info, warn};

use crate::ai::AiRole;
use crate::git_cmd;
use crate::notes::agenda::Agenda;
use crate::notes::agenda::Topic;
use crate::notes::evidence::RevisionFiles;
use crate::notes::facts::{FactVerdict, ReviewValue, VerdictKind, VerifiedFact};
use crate::notes::history::{HistoryCase, unit_history};
use crate::notes::output::{ManifestFile, ManifestUnit, UnitFacts, write_unit};
use crate::notes::plan::UnitPlan;
use crate::notes::profile::HistoryConfig;
use crate::notes::rule_comments::rule_comments;
use crate::workflow::output::parse_json_from_text;
use crate::workflow::{Workflow, WorkflowEngine, WorkflowEnv, WorkflowOutcome};
use crate::workflows::code_notes::{
    Applicability, CheckedFact, ComponentInput, EXPLORE_STAGE_NAMES, ExploreOutput, ExploreRecord,
    FileInput, HISTORY_BATCH_SIZE, HISTORY_STAGE_NAMES, HISTORY_TOPIC_ID, HistoryInput,
    HistoryLesson, LessonKind, MAX_FACTS_PER_STAGE, NotesBuildState, PlacedLesson, SurveyOutput,
    VERIFY_BATCH_SIZE, VERIFY_STAGE_NAMES, check_fact, dedup_stage, explore_stage, ground_symbols,
    history_stage, history_topic, keep_representatives, lessons_for, neutralize_directives,
    parallel_workflow, renumber_facts, single_stage_workflow, survey_stage, synthesize_stage,
    verdicts_by_id, verify_stage,
};

/// Rule comments shown to the explorers of one component.
const MAX_RULE_COMMENTS: usize = 80;

/// Length of the commit IDs past fixes are shown and cited by.
const FIX_ID_LEN: usize = 12;

/// Settings shared by every unit of one build.
#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub project: String,
    pub set_id: String,
    pub revision: String,
    pub set_dir: PathBuf,
    pub work_dir: PathBuf,
    pub resume: bool,
    pub max_turns: usize,
    pub temperature: f32,
    pub model: String,
    pub note_bytes: usize,
    pub history: HistoryConfig,
}

/// Token use of one step or a whole unit.
#[derive(Debug, Clone, Copy, Default, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
}

impl Usage {
    pub(crate) fn add(&mut self, outcome: &WorkflowOutcome) {
        self.tokens_in += u64::from(outcome.tokens_in);
        self.tokens_out += u64::from(outcome.tokens_out);
        self.tokens_cached += u64::from(outcome.tokens_cached);
    }
}

/// What building one unit produced.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct UnitBuildSummary {
    pub unit: String,
    pub slug: String,
    /// Past fixes classified, and those read as code bugs.
    pub history_cases: usize,
    pub history_lessons: usize,
    pub explore_stages: usize,
    /// Explore stages that failed validation but kept their valid facts.
    pub explore_salvaged: usize,
    pub facts_proposed: usize,
    pub evidence_failures: usize,
    /// Facts dropped as restatements of another fact's rule.
    pub duplicates: usize,
    pub confirmed: usize,
    pub refuted: usize,
    pub unverifiable: usize,
    /// Confirmed facts kept out of the note because the verifier judged
    /// them not to be patch rules.
    pub not_patch_rules: usize,
    pub note_bytes: usize,
    /// The note was rendered deterministically because synthesis failed.
    pub synthesis_fallback: bool,
    pub usage: Usage,
}

/// Builds the notes of `unit` and writes them into the set.
pub async fn build_unit(
    repo: &Path,
    env: &WorkflowEnv<'_>,
    agenda: &Agenda,
    unit: &UnitPlan,
    cfg: &BuildConfig,
) -> Result<UnitBuildSummary> {
    let work = cfg.work_dir.join(&unit.slug);
    std::fs::create_dir_all(&work)
        .with_context(|| format!("failed to create {}", work.display()))?;
    let mut summary = UnitBuildSummary {
        unit: unit.name.clone(),
        slug: unit.slug.clone(),
        ..Default::default()
    };
    let mut state = NotesBuildState {
        project: cfg.project.clone(),
        unit: unit.name.clone(),
        revision: cfg.revision.clone(),
        components: component_inputs(unit),
        topics: agenda.topics.clone(),
        ..Default::default()
    };

    // A resumed build may name components the saved steps do not cover;
    // those are surveyed and explored, and every later step reruns over the
    // whole unit, reusing the verdicts of facts already verified.
    let mut extended = false;

    // 1. Survey.
    let survey: SurveyOutput = match load_step::<SurveyOutput>(&work, "survey", cfg.resume)? {
        Some(mut saved) => {
            let missing: Vec<ComponentInput> = state
                .components
                .iter()
                .filter(|c| !saved.components.iter().any(|s| s.name == c.name))
                .cloned()
                .collect();
            if !missing.is_empty() {
                info!(
                    "{}: surveying {} component(s) new to the saved build",
                    unit.name,
                    missing.len()
                );
                let mut partial = NotesBuildState {
                    components: missing,
                    ..state.clone()
                };
                let workflow = single_stage_workflow(
                    "notes_survey",
                    survey_stage(cfg.max_turns, cfg.temperature),
                );
                run_step(
                    &workflow,
                    env,
                    &mut partial,
                    &work,
                    "survey-extend",
                    &mut summary.usage,
                )
                .await?;
                let added = partial
                    .survey
                    .context("the survey stage produced no survey")?;
                saved.components.extend(added.components);
                save_step(&work, "survey", &saved)?;
                extended = true;
            }
            saved
        }
        None => {
            let workflow =
                single_stage_workflow("notes_survey", survey_stage(cfg.max_turns, cfg.temperature));
            run_step(
                &workflow,
                env,
                &mut state,
                &work,
                "survey",
                &mut summary.usage,
            )
            .await?;
            let survey = state
                .survey
                .clone()
                .context("the survey stage produced no survey")?;
            save_step(&work, "survey", &survey)?;
            survey
        }
    };
    state.survey = Some(survey.clone());

    // 2. Read the unit's past fixes into lessons for the explorers.
    let lessons: Vec<PlacedLesson> = match load_step(&work, "history", cfg.resume)? {
        Some(saved) => saved,
        None => {
            let lessons =
                classify_history(repo, env, &mut state, unit, cfg, &work, &mut summary).await?;
            save_step(&work, "history", &lessons)?;
            lessons
        }
    };
    summary.history_lessons = lessons.len();

    // 3. Explore every component for every topic that may apply, plus the
    //    history topic where past fixes fit no agenda topic. Explorers get
    //    the component's rule comments up front.
    for component in &mut state.components {
        let files: Vec<String> = component.files.iter().map(|f| f.path.clone()).collect();
        component.rule_comments =
            rule_comments(repo, &cfg.revision, &files, MAX_RULE_COMMENTS).await?;
    }
    let saved_explore: Option<Vec<ExploreRecord>> = load_step(&work, "explore", cfg.resume)?;
    let explored: Vec<ExploreRecord> = match saved_explore {
        Some(saved)
            if state
                .components
                .iter()
                .all(|c| saved.iter().any(|r| r.component == c.name)) =>
        {
            saved
        }
        saved => {
            if saved.is_some() {
                extended = true;
            }
            let previous = saved.unwrap_or_default();
            state.explored = previous.clone();
            let mut stages = Vec::new();
            for component in state
                .components
                .iter()
                .filter(|c| !previous.iter().any(|r| r.component == c.name))
            {
                let component_survey = survey
                    .components
                    .iter()
                    .find(|c| c.name == component.name)
                    .cloned();
                for topic in &state.topics {
                    let applies = component_survey
                        .as_ref()
                        .and_then(|s| s.topics.iter().find(|t| t.topic == topic.id))
                        .is_none_or(|t| t.applicability != Applicability::DoesNotApply);
                    if applies {
                        stages.push((component.clone(), topic.clone(), component_survey.clone()));
                    }
                }
                if !lessons_for(&lessons, &component.name, HISTORY_TOPIC_ID).is_empty() {
                    stages.push((component.clone(), history_topic(), component_survey.clone()));
                }
            }
            for (round, chunk) in stages.chunks(EXPLORE_STAGE_NAMES.len()).enumerate() {
                let built = chunk
                    .iter()
                    .zip(EXPLORE_STAGE_NAMES)
                    .map(|((component, topic, survey), name)| {
                        explore_stage(
                            name,
                            component.clone(),
                            topic.clone(),
                            survey.clone(),
                            lessons_for(&lessons, &component.name, &topic.id)
                                .into_iter()
                                .cloned()
                                .collect(),
                            cfg.max_turns,
                            cfg.temperature,
                        )
                    })
                    .collect();
                let workflow = parallel_workflow("notes_explore", built);
                let step = format!("explore-round-{}", round + 1);
                let outcome =
                    run_step(&workflow, env, &mut state, &work, &step, &mut summary.usage).await?;
                // A stage that ran out of validation attempts still wrote
                // facts; keep the ones that pass the rules on their own.
                for ((component, topic, _), name) in chunk.iter().zip(EXPLORE_STAGE_NAMES) {
                    let explored = state
                        .explored
                        .iter()
                        .any(|r| r.component == component.name && r.topic == topic.id);
                    if explored {
                        continue;
                    }
                    let fix_ids = lessons_for(&lessons, &component.name, &topic.id)
                        .into_iter()
                        .map(|l| l.fix.clone())
                        .collect();
                    if let Some(record) =
                        salvage_explore(&outcome, name, component, topic, &fix_ids)
                    {
                        summary.explore_salvaged += 1;
                        state.explored.push(record);
                    }
                }
            }
            summary.explore_stages = stages.len();
            let produced = state.explored.len() - previous.len();
            if produced < stages.len() {
                warn!(
                    "{}: {} of {} explore stages failed",
                    unit.name,
                    stages.len() - produced,
                    stages.len()
                );
            }
            save_step(&work, "explore", &state.explored)?;
            state.explored.clone()
        }
    };
    state.explored = explored;

    // 4. Unit-wide IDs and the deterministic evidence check.
    let saved_checked: Option<Vec<CheckedFact>> = load_step(&work, "checked", cfg.resume)?;
    let checked: Vec<CheckedFact> = match saved_checked.filter(|_| !extended) {
        Some(saved) => saved,
        None => {
            let mut files = RevisionFiles::new(repo, &cfg.revision);
            let mut checked = Vec::new();
            for (component, topic, fact) in renumber_facts(&state.explored) {
                let mut evidence_checks = Vec::new();
                for evidence in &fact.evidence {
                    evidence_checks.push(files.check(evidence).await?);
                }
                checked.push(CheckedFact {
                    component,
                    topic,
                    fact,
                    evidence_checks,
                });
            }
            save_step(&work, "checked", &checked)?;
            checked
        }
    };
    summary.facts_proposed = checked.len();
    summary.evidence_failures = checked
        .iter()
        .filter(|f| !f.evidence_checks.iter().any(|c| c.is_ok()))
        .count();

    // Facts from different topics often restate one rule; verify each
    // rule once. A failed dedup keeps every fact.
    let saved_groups: Option<Vec<Vec<String>>> = load_step(&work, "dedup", cfg.resume)?;
    let groups: Vec<Vec<String>> = match saved_groups.filter(|_| !extended) {
        Some(saved) => saved,
        None if checked.len() < 2 => Vec::new(),
        None => {
            let workflow =
                single_stage_workflow("notes_dedup", dedup_stage(&checked, cfg.temperature));
            let groups = match run_step(
                &workflow,
                env,
                &mut state,
                &work,
                "dedup",
                &mut summary.usage,
            )
            .await
            {
                Ok(_) => state.dedup.take().map(|d| d.groups).unwrap_or_default(),
                Err(err) => {
                    warn!("{}: dedup failed, verifying every fact: {err:#}", unit.name);
                    Vec::new()
                }
            };
            save_step(&work, "dedup", &groups)?;
            groups
        }
    };
    let checked = keep_representatives(checked, &groups);
    summary.duplicates = summary.facts_proposed - checked.len();

    // 5. Independent verification, then the same deterministic check on
    //    the verifier's own evidence.
    let saved_verdicts: Option<Vec<FactVerdict>> = load_step(&work, "verdicts", cfg.resume)?;
    let verdicts: Vec<FactVerdict> = match saved_verdicts {
        Some(saved) if !extended => saved,
        saved => {
            // An extended build verifies only the facts without a verdict.
            let previous = saved.unwrap_or_default();
            let pending: Vec<CheckedFact> = checked
                .iter()
                .filter(|c| !previous.iter().any(|v| v.fact_id == c.fact.id))
                .cloned()
                .collect();
            let batches: Vec<Vec<CheckedFact>> = pending
                .chunks(VERIFY_BATCH_SIZE)
                .map(<[_]>::to_vec)
                .collect();
            for (round, chunk) in batches.chunks(VERIFY_STAGE_NAMES.len()).enumerate() {
                let built = chunk
                    .iter()
                    .zip(VERIFY_STAGE_NAMES)
                    .map(|(batch, name)| {
                        verify_stage(name, batch.clone(), cfg.max_turns, cfg.temperature)
                    })
                    .collect();
                let workflow = parallel_workflow("notes_verify", built);
                let step = format!("verify-round-{}", round + 1);
                run_step(&workflow, env, &mut state, &work, &step, &mut summary.usage).await?;
            }
            let mut verdicts = previous;
            verdicts.extend(check_verifier_evidence(repo, &cfg.revision, &state.verdicts).await?);
            save_step(&work, "verdicts", &verdicts)?;
            verdicts
        }
    };
    state.verdicts = verdicts;

    let verified = verified_facts(&checked, &state.verdicts);
    for v in &verified {
        match v.verdict.verdict {
            VerdictKind::Confirmed => summary.confirmed += 1,
            VerdictKind::Refuted => summary.refuted += 1,
            VerdictKind::Unverifiable => summary.unverifiable += 1,
        }
    }
    let confirmed = noted_facts(&verified);
    summary.not_patch_rules = summary.confirmed - confirmed.len();

    // 6. Synthesize the note.
    let saved_note = load_text(&work, "note.md", cfg.resume)?;
    let note = match saved_note.filter(|_| !extended) {
        Some(saved) => saved,
        None => {
            let note = if confirmed.is_empty() {
                format!(
                    "# {}\n\nNo verified facts were established for this unit at {}.\n",
                    unit.name, cfg.revision
                )
            } else {
                let (facts_json, ids) = synthesis_input(&confirmed);
                let workflow = single_stage_workflow(
                    "notes_synthesize",
                    synthesize_stage(
                        facts_json,
                        survey_summaries(&survey),
                        ids,
                        cfg.note_bytes,
                        cfg.temperature,
                    ),
                );
                match run_step(
                    &workflow,
                    env,
                    &mut state,
                    &work,
                    "synthesize",
                    &mut summary.usage,
                )
                .await
                {
                    Ok(_) => state
                        .note
                        .clone()
                        .context("the synthesize stage produced no note")?,
                    Err(err) => {
                        warn!(
                            "{}: synthesis failed, writing the fallback note: {err:#}",
                            unit.name
                        );
                        summary.synthesis_fallback = true;
                        fallback_note(&unit.name, &survey, &confirmed, cfg.note_bytes)
                    }
                }
            };
            let note = format!("{}\n", neutralize_directives(note.trim_end()));
            std::fs::write(work.join("note.md"), &note)?;
            note
        }
    };
    summary.note_bytes = note.len();

    // 7. Write the unit into the set.
    let symbols: BTreeSet<String> = confirmed
        .iter()
        .flat_map(|v| v.fact.scope.symbols.iter().cloned())
        .collect();
    let files = blob_ids(repo, &cfg.revision, unit).await?;
    let entry = ManifestUnit {
        name: unit.name.clone(),
        note: format!("units/{}.md", unit.slug),
        facts: format!("units/{}.facts.json", unit.slug),
        agenda_version: agenda.version.clone(),
        model: cfg.model.clone(),
        built_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        note_bytes: note.len(),
        confirmed_facts: confirmed.len(),
        files,
        symbols: symbols.into_iter().collect(),
    };
    let unit_facts = UnitFacts {
        unit: unit.name.clone(),
        revision: cfg.revision.clone(),
        facts: verified.clone(),
        dismissed_hypotheses: state
            .explored
            .iter()
            .flat_map(|r| r.output.dismissed_hypotheses.iter().cloned())
            .collect(),
        open_questions: state
            .explored
            .iter()
            .flat_map(|r| r.output.open_questions.iter().cloned())
            .collect(),
    };
    write_unit(
        &cfg.set_dir,
        (&cfg.project, &cfg.set_id, &cfg.revision),
        &unit.slug,
        &note,
        &unit_facts,
        entry,
    )?;
    save_step(&work, "summary", &summary)?;
    info!(
        "{}: {} facts proposed, {} confirmed, {} refuted, {} unverifiable; note {} bytes; {} tokens in",
        unit.name,
        summary.facts_proposed,
        summary.confirmed,
        summary.refuted,
        summary.unverifiable,
        summary.note_bytes,
        summary.usage.tokens_in
    );
    Ok(summary)
}

/// Mines the unit's past fixes and classifies them in parallel batches.
/// Returns the code-bug lessons placed on the components their fixes
/// touched, newest first.
async fn classify_history(
    repo: &Path,
    env: &WorkflowEnv<'_>,
    state: &mut NotesBuildState,
    unit: &UnitPlan,
    cfg: &BuildConfig,
    work: &Path,
    summary: &mut UnitBuildSummary,
) -> Result<Vec<PlacedLesson>> {
    let files: Vec<String> = unit
        .components
        .iter()
        .flat_map(|c| c.files.iter().map(|f| f.path.clone()))
        .collect();
    let cases = unit_history(
        repo,
        &cfg.revision,
        &files,
        &cfg.history.fix_patterns,
        cfg.history.max_cases,
    )
    .await?;
    summary.history_cases = cases.len();
    if cases.is_empty() {
        return Ok(Vec::new());
    }
    save_step(work, "history-cases", &cases)?;
    let inputs: Vec<HistoryInput> = cases
        .iter()
        .map(|c| HistoryInput {
            id: fix_id(&c.fix),
            subject: c.subject.clone(),
            message: c.message.clone(),
            diff: c.diff.clone(),
        })
        .collect();
    let per_round = HISTORY_BATCH_SIZE * HISTORY_STAGE_NAMES.len();
    for (round, chunk) in inputs.chunks(per_round).enumerate() {
        let built = chunk
            .chunks(HISTORY_BATCH_SIZE)
            .zip(HISTORY_STAGE_NAMES)
            .map(|(batch, name)| history_stage(name, batch.to_vec(), cfg.temperature))
            .collect();
        let workflow = parallel_workflow("notes_history", built);
        let step = format!("history-round-{}", round + 1);
        run_step(&workflow, env, state, work, &step, &mut summary.usage).await?;
    }
    Ok(place_lessons(&cases, &state.lessons, unit))
}

fn fix_id(sha: &str) -> String {
    sha.chars().take(FIX_ID_LEN).collect()
}

/// Code-bug lessons in the cases' order (newest first), each on the
/// components holding a file its fix touched, or on every component when
/// none does.
fn place_lessons(
    cases: &[HistoryCase],
    lessons: &[HistoryLesson],
    unit: &UnitPlan,
) -> Vec<PlacedLesson> {
    let mut placed = Vec::new();
    for case in cases {
        let id = fix_id(&case.fix);
        let Some(lesson) = lessons
            .iter()
            .find(|l| l.fix == id && l.kind == LessonKind::CodeBug)
        else {
            continue;
        };
        let mut components: Vec<String> = unit
            .components
            .iter()
            .filter(|c| c.files.iter().any(|f| case.files.contains(&f.path)))
            .map(|c| c.name.clone())
            .collect();
        if components.is_empty() {
            components = unit.components.iter().map(|c| c.name.clone()).collect();
        }
        placed.push(PlacedLesson {
            fix: id,
            subject: case.subject.clone(),
            topic: lesson.topic.clone(),
            violated_rule: lesson.violated_rule.trim().to_string(),
            components,
        });
    }
    placed
}

fn component_inputs(unit: &UnitPlan) -> Vec<ComponentInput> {
    unit.components
        .iter()
        .map(|c| ComponentInput {
            name: c.name.clone(),
            files: c
                .files
                .iter()
                .map(|f| FileInput {
                    path: f.path.clone(),
                    lines: f.lines,
                    test_lines: f.test_lines,
                })
                .collect(),
            entry_points: c
                .entry_points
                .iter()
                .map(|e| format!("{:?} {} ({}:{})", e.kind, e.name, e.path, e.line))
                .collect(),
            rule_comments: Vec::new(),
        })
        .collect()
}

/// The facts of a failed explore stage's last answer that pass the fact
/// rules on their own, or None when nothing usable is left.
fn salvage_explore(
    outcome: &WorkflowOutcome,
    stage: &str,
    component: &ComponentInput,
    topic: &Topic,
    fix_ids: &HashSet<String>,
) -> Option<ExploreRecord> {
    let run = outcome.stage_runs.iter().find(|r| r.name == stage)?;
    let messages = outcome.history.get(run.history_start..run.history_end)?;
    let text = messages
        .iter()
        .rev()
        .filter(|m| m.role == AiRole::Assistant)
        .find_map(|m| m.content.as_deref().filter(|c| !c.trim().is_empty()))?;
    let mut output: ExploreOutput = parse_json_from_text(text).ok()?;
    let question_ids: HashSet<String> = topic.questions.iter().map(|q| q.id.clone()).collect();
    let mut ids = HashSet::new();
    output
        .facts
        .retain(|f| check_fact(f, &question_ids, fix_ids).is_ok() && ids.insert(f.id.clone()));
    output.facts.truncate(MAX_FACTS_PER_STAGE);
    if output.facts.is_empty() {
        return None;
    }
    output.facts.iter_mut().for_each(ground_symbols);
    output
        .dismissed_hypotheses
        .retain(|d| !d.statement.trim().is_empty() && !d.reason.trim().is_empty());
    Some(ExploreRecord {
        component: component.name.clone(),
        topic: topic.id.clone(),
        output,
    })
}

/// Runs one step's workflow, keeps its full LLM history for audit, and
/// returns its outcome.
async fn run_step(
    workflow: &Workflow<NotesBuildState>,
    env: &WorkflowEnv<'_>,
    state: &mut NotesBuildState,
    work: &Path,
    step: &str,
    usage: &mut Usage,
) -> Result<WorkflowOutcome> {
    info!("{}: running {step}", state.unit);
    let result = WorkflowEngine::execute(workflow, env, state, None).await;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            if let Some(failure) = err.downcast_ref::<crate::workflow::WorkflowFailure>() {
                let _ = save_step(work, &format!("{step}.history"), &failure.outcome.history);
            }
            return Err(err.context(format!("{} {step} failed", state.unit)));
        }
    };
    usage.add(&outcome);
    save_step(work, &format!("{step}.history"), &outcome.history)?;
    save_step(work, &format!("{step}.stages"), &outcome.stage_runs)?;
    Ok(outcome)
}

/// Confirmed verdicts whose own evidence all fails the deterministic check
/// are downgraded to unverifiable: the verifier did not show the code it
/// relied on.
async fn check_verifier_evidence(
    repo: &Path,
    revision: &str,
    verdicts: &[FactVerdict],
) -> Result<Vec<FactVerdict>> {
    let mut files = RevisionFiles::new(repo, revision);
    let mut out = Vec::new();
    for verdict in verdicts {
        let mut verdict = verdict.clone();
        if verdict.verdict == VerdictKind::Confirmed {
            let mut any_ok = false;
            for evidence in &verdict.evidence {
                if files.check(evidence).await?.is_ok() {
                    any_ok = true;
                    break;
                }
            }
            if !any_ok {
                verdict.verdict = VerdictKind::Unverifiable;
                verdict.reasoning.push_str(
                    " [Downgraded: none of the verifier's evidence matches the code at the pinned revision.]",
                );
            }
        }
        out.push(verdict);
    }
    Ok(out)
}

/// Joins facts with their verdicts. A fact without a verdict (its verify
/// stage failed) is unverifiable.
fn verified_facts(checked: &[CheckedFact], verdicts: &[FactVerdict]) -> Vec<VerifiedFact> {
    let by_id = verdicts_by_id(verdicts);
    checked
        .iter()
        .map(|c| VerifiedFact {
            component: c.component.clone(),
            topic: c.topic.clone(),
            fact: c.fact.clone(),
            verdict: by_id.get(c.fact.id.as_str()).map_or_else(
                || FactVerdict {
                    fact_id: c.fact.id.clone(),
                    verdict: VerdictKind::Unverifiable,
                    review_value: ReviewValue::Unknown,
                    reasoning: "No verdict: the verify stage for this fact failed.".to_string(),
                    evidence: Vec::new(),
                },
                |v| (*v).clone(),
            ),
        })
        .collect()
}

/// The facts a note is written from: confirmed, and judged to be rules a
/// future patch must preserve.
fn noted_facts(verified: &[VerifiedFact]) -> Vec<&VerifiedFact> {
    verified
        .iter()
        .filter(|v| v.verdict.verdict == VerdictKind::Confirmed)
        .filter(|v| v.verdict.review_value == ReviewValue::PatchRule)
        .collect()
}

#[derive(Serialize)]
struct SynthesisFact<'a> {
    id: &'a str,
    heading: &'static str,
    statement: &'a str,
    violated_by: &'a str,
    symbols: &'a [String],
    from_history: &'a [String],
}

fn synthesis_input(confirmed: &[&VerifiedFact]) -> (String, Vec<String>) {
    let facts: Vec<SynthesisFact> = confirmed
        .iter()
        .map(|v| SynthesisFact {
            id: &v.fact.id,
            heading: v.fact.kind.heading(),
            statement: &v.fact.statement,
            violated_by: &v.fact.violated_by,
            symbols: &v.fact.scope.symbols,
            from_history: &v.fact.from_history,
        })
        .collect();
    let ids = confirmed.iter().map(|v| v.fact.id.clone()).collect();
    (
        serde_json::to_string_pretty(&facts).unwrap_or_default(),
        ids,
    )
}

/// A note rendered without an LLM: the survey summaries, then every
/// confirmed fact under its kind's heading in the fixed order, cut at the
/// byte budget. Used only when synthesis cannot satisfy the note rules, so
/// a unit is never lost to one stubborn stage.
fn fallback_note(
    unit: &str,
    survey: &SurveyOutput,
    confirmed: &[&VerifiedFact],
    max_bytes: usize,
) -> String {
    const HEADINGS: &[&str] = &[
        "Architecture & Data Flow",
        "Synchronization",
        "Object Lifetimes",
        "API Contracts",
        "Error Handling",
        "External Contracts",
        "Known Pitfalls",
        "Other",
    ];
    let mut note = format!("# {unit}\n\n");
    for component in &survey.components {
        note.push_str(&component.summary);
        note.push('\n');
    }
    for heading in HEADINGS {
        let facts: Vec<&&VerifiedFact> = confirmed
            .iter()
            .filter(|v| v.fact.kind.heading() == *heading)
            .collect();
        if facts.is_empty() {
            continue;
        }
        let section = format!("\n## {heading}\n\n");
        if note.len() + section.len() > max_bytes {
            break;
        }
        note.push_str(&section);
        for v in facts {
            let bullet = format!("- {} [{}]\n", v.fact.statement.trim(), v.fact.id);
            if note.len() + bullet.len() > max_bytes {
                return note;
            }
            note.push_str(&bullet);
        }
    }
    note
}

fn survey_summaries(survey: &SurveyOutput) -> String {
    survey
        .components
        .iter()
        .map(|c| format!("{}: {}", c.name, c.summary))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Blob IDs of the unit's files at the revision, for refresh.
async fn blob_ids(repo: &Path, revision: &str, unit: &UnitPlan) -> Result<Vec<ManifestFile>> {
    let paths: Vec<&str> = unit
        .components
        .iter()
        .flat_map(|c| c.files.iter().map(|f| f.path.as_str()))
        .collect();
    let mut files = Vec::new();
    for chunk in paths.chunks(500) {
        let output = git_cmd::in_dir_async(repo)
            .args(["ls-tree", "-z", "--full-tree", revision, "--"])
            .args(chunk)
            .output()
            .await
            .context("failed to run git ls-tree")?;
        if !output.status.success() {
            bail!(
                "git ls-tree failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        for record in output.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let record = String::from_utf8_lossy(record);
            // "<mode> blob <sha>\t<path>"
            if let Some((meta, path)) = record.split_once('\t')
                && let Some(blob) = meta.split_whitespace().nth(2)
            {
                files.push(ManifestFile {
                    path: path.to_string(),
                    blob: blob.to_string(),
                });
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn save_step<T: Serialize + ?Sized>(work: &Path, name: &str, value: &T) -> Result<()> {
    let path = work.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(value)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn load_step<T: DeserializeOwned>(work: &Path, name: &str, resume: bool) -> Result<Option<T>> {
    let path = work.join(format!("{name}.json"));
    if !resume || !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse saved step {}", path.display()))?;
    info!("resuming from {}", path.display());
    Ok(Some(value))
}

fn load_text(work: &Path, name: &str, resume: bool) -> Result<Option<String>> {
    let path = work.join(name);
    if !resume || !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::evidence::EvidenceCheck;
    use crate::notes::facts::{Evidence, Fact, FactKind, FactScope};

    fn checked(id: &str) -> CheckedFact {
        CheckedFact {
            component: "c".to_string(),
            topic: "SYNC".to_string(),
            fact: Fact {
                id: id.to_string(),
                kind: FactKind::Invariant,
                statement: "s".to_string(),
                violated_by: "p".to_string(),
                basis: None,
                scope: FactScope::default(),
                evidence: vec![],
                from_history: vec![],
                questions: vec![],
            },
            evidence_checks: vec![EvidenceCheck::Ok],
        }
    }

    #[test]
    fn test_verified_facts_marks_missing_verdicts_unverifiable() {
        let verdicts = vec![FactVerdict {
            fact_id: "F1".to_string(),
            verdict: VerdictKind::Confirmed,
            review_value: ReviewValue::PatchRule,
            reasoning: "r".to_string(),
            evidence: vec![],
        }];
        let verified = verified_facts(&[checked("F1"), checked("F2")], &verdicts);
        assert_eq!(verified[0].verdict.verdict, VerdictKind::Confirmed);
        assert_eq!(verified[1].verdict.verdict, VerdictKind::Unverifiable);
    }

    #[tokio::test]
    async fn test_check_verifier_evidence_downgrades_unbacked_confirmations() {
        let dir = tempfile::tempdir().unwrap();
        let sha =
            crate::notes::scope::test_repo::commit_files(dir.path(), &[("a.rs", "fn a() {}\n")]);
        let verdict = |snippet: &str| FactVerdict {
            fact_id: "F1".to_string(),
            verdict: VerdictKind::Confirmed,
            review_value: ReviewValue::PatchRule,
            reasoning: "r".to_string(),
            evidence: vec![Evidence {
                path: "a.rs".to_string(),
                start_line: 1,
                end_line: 1,
                snippet: snippet.to_string(),
            }],
        };
        let out =
            check_verifier_evidence(dir.path(), &sha, &[verdict("fn a() {}"), verdict("fn b()")])
                .await
                .unwrap();
        assert_eq!(out[0].verdict, VerdictKind::Confirmed);
        assert_eq!(out[1].verdict, VerdictKind::Unverifiable);
        assert!(out[1].reasoning.contains("Downgraded"));
    }

    #[test]
    fn test_fallback_note_groups_and_respects_budget() {
        use crate::workflows::code_notes::ComponentSurvey;
        let survey = SurveyOutput {
            components: vec![ComponentSurvey {
                name: "c".to_string(),
                summary: "Does things.".to_string(),
                key_structures: vec![],
                concurrency_contexts: vec![],
                external_interfaces: vec![],
                topics: vec![],
            }],
        };
        let mut lock = checked("F1").fact;
        lock.kind = FactKind::LockingRule;
        lock.statement = "Hold A before B.".to_string();
        let mut inv = checked("F2").fact;
        inv.statement = "Ids are unique.".to_string();
        let verified: Vec<VerifiedFact> = [lock, inv]
            .into_iter()
            .map(|fact| VerifiedFact {
                component: "c".to_string(),
                topic: "SYNC".to_string(),
                verdict: FactVerdict {
                    fact_id: fact.id.clone(),
                    verdict: VerdictKind::Confirmed,
                    review_value: ReviewValue::PatchRule,
                    reasoning: "r".to_string(),
                    evidence: vec![],
                },
                fact,
            })
            .collect();
        let refs: Vec<&VerifiedFact> = verified.iter().collect();
        let note = fallback_note("u", &survey, &refs, 10_000);
        let arch = note.find("## Architecture & Data Flow").unwrap();
        let sync = note.find("## Synchronization").unwrap();
        assert!(arch < sync);
        assert!(note.contains("- Hold A before B. [F1]"));
        let short = fallback_note("u", &survey, &refs, 40);
        assert!(short.len() <= 40);
    }

    #[test]
    fn test_noted_facts_keep_confirmed_patch_rules() {
        let checked = vec![checked("F1"), checked("F2"), checked("F3")];
        let verdict = |id: &str, verdict: VerdictKind, review_value: ReviewValue| FactVerdict {
            fact_id: id.to_string(),
            verdict,
            review_value,
            reasoning: "r".to_string(),
            evidence: vec![],
        };
        let verified = verified_facts(
            &checked,
            &[
                verdict("F1", VerdictKind::Confirmed, ReviewValue::PatchRule),
                verdict("F2", VerdictKind::Confirmed, ReviewValue::ExistingWeakness),
                verdict("F3", VerdictKind::Refuted, ReviewValue::PatchRule),
            ],
        );
        let noted: Vec<&str> = noted_facts(&verified)
            .iter()
            .map(|v| v.fact.id.as_str())
            .collect();
        assert_eq!(noted, vec!["F1"]);
    }

    #[test]
    fn test_salvage_explore_keeps_valid_facts() {
        use crate::ai::AiMessage;
        use crate::notes::agenda::Question;
        use crate::workflow::StageRunRecord;
        let fact = |id: &str, violated_by: &str| {
            serde_json::json!({
                "id": id, "kind": "Invariant", "statement": "s", "violated_by": violated_by,
                "basis": {"kind": "comment", "detail": "d"},
                "scope": {"files": [], "symbols": ["ghost"]},
                "evidence": [{"path": "a.c", "start_line": 1, "end_line": 2, "snippet": "x"}],
                "questions": ["SYNC-1"]
            })
        };
        let answer = serde_json::json!({"facts": [fact("F1", "p"), fact("F2", "")]});
        let message = |role: AiRole, content: &str| AiMessage {
            role,
            content: Some(content.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let outcome = WorkflowOutcome {
            history: vec![
                message(AiRole::User, "explore"),
                message(AiRole::Assistant, &answer.to_string()),
                message(AiRole::User, "rejected"),
            ],
            stage_runs: vec![StageRunRecord {
                name: "explore-2".to_string(),
                skipped: false,
                turns: 2,
                tokens_in: 0,
                tokens_out: 0,
                tokens_cached: 0,
                prompts_read: vec![],
                history_start: 0,
                history_end: 3,
            }],
            ..Default::default()
        };
        let component = ComponentInput {
            name: "c".to_string(),
            files: vec![],
            entry_points: vec![],
            rule_comments: vec![],
        };
        let topic = Topic {
            id: "SYNC".to_string(),
            title: "sync".to_string(),
            module: "universal".to_string(),
            questions: vec![Question {
                id: "SYNC-1".to_string(),
                text: "q".to_string(),
            }],
        };
        let record =
            salvage_explore(&outcome, "explore-2", &component, &topic, &HashSet::new()).unwrap();
        assert_eq!(record.output.facts.len(), 1);
        assert_eq!(record.output.facts[0].id, "F1");
        assert!(record.output.facts[0].scope.symbols.is_empty());
        assert!(
            salvage_explore(&outcome, "explore-1", &component, &topic, &HashSet::new()).is_none()
        );
    }

    #[test]
    fn test_place_lessons_by_touched_files() {
        use crate::notes::plan::{ComponentPlan, PlannedFile, TokenEstimate};
        let component = |name: &str, path: &str| ComponentPlan {
            name: name.to_string(),
            files: vec![PlannedFile {
                path: path.to_string(),
                lines: 10,
                test_lines: 0,
            }],
            lines: 10,
            entry_points: vec![],
        };
        let unit = UnitPlan {
            name: "net".to_string(),
            slug: "net".to_string(),
            files: 2,
            lines: 20,
            shared_files: 0,
            components: vec![component("a", "net/a.c"), component("b", "net/b.c")],
            estimated_input_tokens: TokenEstimate::default(),
        };
        let case = |sha: &str, files: &[&str]| HistoryCase {
            fix: sha.to_string(),
            time: 0,
            subject: format!("fix {sha}"),
            files: files.iter().map(|f| f.to_string()).collect(),
            message: String::new(),
            diff: String::new(),
        };
        let lesson = |sha: &str, kind: LessonKind| HistoryLesson {
            fix: fix_id(sha),
            kind,
            topic: "SYNC".to_string(),
            violated_rule: " rule ".to_string(),
        };
        let newer = "1".repeat(40);
        let older = "2".repeat(40);
        let docs = "3".repeat(40);
        let cases = vec![
            case(&newer, &["net/b.c"]),
            case(&docs, &["net/a.c"]),
            case(&older, &[]),
        ];
        // Lessons arrive in any order; placement follows the cases.
        let lessons = vec![
            lesson(&older, LessonKind::CodeBug),
            lesson(&docs, LessonKind::NotCodeBug),
            lesson(&newer, LessonKind::CodeBug),
        ];
        let placed = place_lessons(&cases, &lessons, &unit);
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].fix, "1".repeat(FIX_ID_LEN));
        assert_eq!(placed[0].components, vec!["b"]);
        assert_eq!(placed[0].violated_rule, "rule");
        assert_eq!(placed[1].components, vec!["a", "b"]);
    }

    #[test]
    fn test_steps_resume_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        save_step(dir.path(), "survey", &vec![1, 2, 3]).unwrap();
        assert_eq!(
            load_step::<Vec<u8>>(dir.path(), "survey", false).unwrap(),
            None
        );
        assert_eq!(
            load_step::<Vec<u8>>(dir.path(), "survey", true).unwrap(),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            load_step::<Vec<u8>>(dir.path(), "missing", true).unwrap(),
            None
        );
    }
}
