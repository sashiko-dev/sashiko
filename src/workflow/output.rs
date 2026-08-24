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

//! Output formats, schema validation, and feedback formatting for workflow stages.

use serde::de::DeserializeOwned;
use serde_json::Value;

/// Validation function for strongly-typed JSON outputs against current workflow state.
pub type JsonValidator<S, T> = Box<dyn Fn(&T, &S) -> Result<(), String> + Send + Sync>;

/// Validation function for plaintext outputs against current workflow state.
pub type TextValidator<S> = Box<dyn Fn(&str, &S) -> Result<(), String> + Send + Sync>;

/// Formatter function for generating retry prompt feedback from raw invalid output.
pub type FeedbackFormatter = Box<dyn Fn(&str) -> String + Send + Sync>;

/// Defines the expected output format of a stage, its parsing logic, and validation rules.
pub enum OutputFormat<S, T> {
    /// Strongly-typed JSON output.
    Json {
        schema: Option<Value>,
        validator: Option<JsonValidator<S, T>>,
        feedback_formatter: Option<FeedbackFormatter>,
    },
    /// Plaintext output validated by a custom function.
    Text {
        validator: TextValidator<S>,
        feedback_formatter: FeedbackFormatter,
    },
}

impl<S, T> OutputFormat<S, T>
where
    T: DeserializeOwned + Send + 'static,
{
    /// Creates a typed JSON output specification.
    pub fn json() -> Self {
        Self::Json {
            schema: None,
            validator: None,
            feedback_formatter: None,
        }
    }

    /// Creates a typed JSON output specification with a given JSON schema.
    pub fn json_with_schema(schema: Value) -> Self {
        Self::Json {
            schema: Some(schema),
            validator: None,
            feedback_formatter: None,
        }
    }

    /// Attaches a custom semantic validation predicate to the parsed JSON.
    pub fn with_validator<F>(mut self, validator_fn: F) -> Self
    where
        F: Fn(&T, &S) -> Result<(), String> + Send + Sync + 'static,
    {
        if let Self::Json {
            ref mut validator, ..
        } = self
        {
            *validator = Some(Box::new(validator_fn));
        }
        self
    }

    /// Attaches a custom feedback formatter when validation fails.
    pub fn with_feedback_formatter<F>(mut self, formatter: F) -> Self
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        if let Self::Json {
            ref mut feedback_formatter,
            ..
        } = self
        {
            *feedback_formatter = Some(Box::new(formatter));
        }
        self
    }
}

impl<S> OutputFormat<S, String> {
    /// Creates a plaintext output format with custom validation and feedback.
    pub fn text_with_validator<V, F>(validator: V, feedback_formatter: F) -> Self
    where
        V: Fn(&str, &S) -> Result<(), String> + Send + Sync + 'static,
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        Self::Text {
            validator: Box::new(validator),
            feedback_formatter: Box::new(feedback_formatter),
        }
    }

    /// Creates a plaintext output format that accepts any text.
    pub fn text() -> Self {
        Self::Text {
            validator: Box::new(|_, _| Ok(())),
            feedback_formatter: Box::new(|v| {
                format!(
                    "Previous attempt was rejected: {}. Please correct your output format.",
                    v
                )
            }),
        }
    }
}

impl<S, T> OutputFormat<S, T>
where
    T: DeserializeOwned + Send + 'static,
{
    /// Returns the optional JSON schema to provide to the model in `response_format`.
    pub fn schema(&self) -> Option<&Value> {
        match self {
            Self::Json { schema, .. } => schema.as_ref(),
            Self::Text { .. } => None,
        }
    }

    /// Formats feedback for the LLM when output fails validation.
    pub fn format_feedback(&self, violation: &str) -> String {
        match self {
            Self::Json {
                feedback_formatter: Some(f),
                ..
            } => f(violation),
            Self::Text {
                feedback_formatter, ..
            } => feedback_formatter(violation),
            _ => format!(
                "Previous attempt was rejected: {}. Please correct your output format.",
                violation
            ),
        }
    }

    /// Validates raw model output text and parses it into `T`.
    pub fn validate(&self, raw_text: &str, state: &S) -> Result<T, String> {
        match self {
            Self::Json { validator, .. } => {
                let parsed = parse_json_from_text::<T>(raw_text)?;
                if let Some(v) = validator {
                    v(&parsed, state)?;
                }
                Ok(parsed)
            }
            Self::Text { validator, .. } => {
                validator(raw_text, state)?;
                // Safe cast since Self::Text is only constructed when T = String
                let boxed_any: Box<dyn std::any::Any> = Box::new(raw_text.to_string());
                match boxed_any.downcast::<T>() {
                    Ok(val) => Ok(*val),
                    Err(_) => Err("Failed to downcast text output".to_string()),
                }
            }
        }
    }
}

/// Parses JSON from text with fallback extraction of embedded JSON objects.
pub fn parse_json_from_text<T: DeserializeOwned>(raw_text: &str) -> Result<T, String> {
    let cleaned = crate::utils::clean_json_string(raw_text);
    if let Ok(val) = serde_json::from_str::<T>(&cleaned) {
        return Ok(val);
    }
    if let Ok(val) = serde_json::from_str::<T>(raw_text) {
        return Ok(val);
    }

    // Try finding JSON objects in text (e.g. within ```json ``` blocks or braces)
    let candidates = find_json_candidates(raw_text);
    for cand in candidates.into_iter().rev() {
        if let Ok(val) = serde_json::from_value::<T>(cand) {
            return Ok(val);
        }
    }

    Err(format!(
        "Failed to parse JSON from output: {}",
        crate::utils::utf8_prefix(raw_text, 200)
    ))
}

fn find_json_candidates(text: &str) -> Vec<Value> {
    let mut candidates = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '{'
            && let Some(end) = find_matching_brace(&chars, i)
        {
            let candidate: String = chars[i..=end].iter().collect();
            let clean_candidate = crate::utils::clean_json_string(&candidate);
            if let Ok(v) =
                serde_json::from_str(&clean_candidate).or_else(|_| serde_json::from_str(&candidate))
            {
                candidates.push(v);
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    candidates
}

fn find_matching_brace(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;

    for (i, c) in chars.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if *c == '\\' {
                escape = true;
            } else if *c == '"' {
                in_string = false;
            }
        } else if *c == '"' {
            in_string = true;
        } else if *c == '{' {
            depth += 1;
        } else if *c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, Debug, PartialEq)]
    struct DummyOutput {
        name: String,
        count: u32,
    }

    #[test]
    fn test_json_output_format_valid() {
        let fmt = OutputFormat::<(), DummyOutput>::json();
        let raw = r#"{"name": "test", "count": 10}"#;
        let res = fmt.validate(raw, &()).unwrap();
        assert_eq!(
            res,
            DummyOutput {
                name: "test".to_string(),
                count: 10
            }
        );
    }

    #[test]
    fn test_json_output_format_with_validator_rejection() {
        let fmt = OutputFormat::<(), DummyOutput>::json().with_validator(|out, _| {
            if out.count < 5 {
                Err("count must be >= 5".to_string())
            } else {
                Ok(())
            }
        });

        let raw = r#"{"name": "test", "count": 2}"#;
        let err = fmt.validate(raw, &()).unwrap_err();
        assert_eq!(err, "count must be >= 5");
    }

    #[test]
    fn test_json_output_format_markdown_wrapped() {
        let fmt = OutputFormat::<(), DummyOutput>::json();
        let raw = "Here is the result:\n```json\n{\"name\": \"wrapped\", \"count\": 7}\n```";
        let res = fmt.validate(raw, &()).unwrap();
        assert_eq!(
            res,
            DummyOutput {
                name: "wrapped".to_string(),
                count: 7
            }
        );
    }
}
