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

//! Facts: atomic, verifiable statements about the code with evidence at the
//! pinned revision, and the verdicts that decide which ones reach a note.
//! The kinds are a fixed set shared by every project (design section 3.5);
//! domain detail belongs in the statement.

use serde::{Deserialize, Serialize};

/// What kind of rule a fact states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FactKind {
    Invariant,
    LockingRule,
    LifetimeRule,
    ApiContract,
    ErrorHandling,
    Concurrency,
    ExternalContract,
    Pitfall,
    Other,
}

impl FactKind {
    /// The note heading a fact of this kind is grouped under.
    pub fn heading(self) -> &'static str {
        match self {
            FactKind::Invariant => "Architecture & Data Flow",
            FactKind::LockingRule | FactKind::Concurrency => "Synchronization",
            FactKind::LifetimeRule => "Object Lifetimes",
            FactKind::ApiContract => "API Contracts",
            FactKind::ErrorHandling => "Error Handling",
            FactKind::ExternalContract => "External Contracts",
            FactKind::Pitfall => "Known Pitfalls",
            FactKind::Other => "Other",
        }
    }
}

/// A code location backing a fact: a line range at the pinned revision and
/// the code it contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub snippet: String,
}

/// Files and symbols a fact is about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactScope {
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
}

/// One fact proposed by an explore stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub id: String,
    pub kind: FactKind,
    pub statement: String,
    /// The shape of a patch that would break the rule.
    #[serde(default)]
    pub violated_by: String,
    #[serde(default)]
    pub scope: FactScope,
    pub evidence: Vec<Evidence>,
    /// Fixing commits whose violated rule this fact restates.
    #[serde(default)]
    pub from_history: Vec<String>,
    /// Agenda question IDs the fact answers.
    #[serde(default)]
    pub questions: Vec<String>,
}

/// A suspected rule the explorer investigated and found false, kept so it
/// is not investigated again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DismissedHypothesis {
    pub statement: String,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

/// The verifier's decision on one fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Confirmed,
    Refuted,
    Unverifiable,
}

/// A verdict with the verifier's reasoning and its own evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactVerdict {
    pub fact_id: String,
    pub verdict: VerdictKind,
    pub reasoning: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

/// A fact after verification, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedFact {
    pub component: String,
    pub topic: String,
    pub fact: Fact,
    pub verdict: FactVerdict,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fact_round_trip_and_defaults() {
        let json = r#"{
            "id": "F1",
            "kind": "LockingRule",
            "statement": "The cache lock is never held across an await point.",
            "evidence": [{"path": "src/a.rs", "start_line": 3, "end_line": 4, "snippet": "let g = lock();"}]
        }"#;
        let fact: Fact = serde_json::from_str(json).unwrap();
        assert_eq!(fact.kind, FactKind::LockingRule);
        assert!(fact.scope.symbols.is_empty());
        assert!(fact.from_history.is_empty());
        assert_eq!(fact.kind.heading(), "Synchronization");
        let back: Fact = serde_json::from_str(&serde_json::to_string(&fact).unwrap()).unwrap();
        assert_eq!(back, fact);
        assert!(serde_json::from_str::<Fact>(&json.replace("LockingRule", "Bogus")).is_err());
    }

    #[test]
    fn test_verdict_kind_names() {
        let v: FactVerdict = serde_json::from_str(
            r#"{"fact_id": "F1", "verdict": "unverifiable", "reasoning": "no caller found"}"#,
        )
        .unwrap();
        assert_eq!(v.verdict, VerdictKind::Unverifiable);
    }
}
