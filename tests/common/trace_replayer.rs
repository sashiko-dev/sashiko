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
#![allow(dead_code)]

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[allow(dead_code)]
pub struct TraceReplayer {
    pub interactions: Vec<(Value, Value)>,
}

impl TraceReplayer {
    pub fn new() -> Self {
        let mut interactions = Vec::new();
        let traces_dir = PathBuf::from("tests/data/traces");
        let paths = fs::read_dir(&traces_dir).expect("Failed to read traces directory");
        let mut req_files: Vec<_> = paths
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name().to_str().unwrap().starts_with("trace_")
                    && e.file_name().to_str().unwrap().ends_with("_req.json")
            })
            .collect();

        // Sort by name (timestamp)
        req_files.sort_by_key(|a| a.file_name());

        for req_path in req_files {
            let req_os_name = req_path.file_name();
            let req_name = req_os_name.to_str().unwrap();
            let resp_name = req_name.replace("_req.json", "_resp.json");
            let resp_path = traces_dir.join(resp_name);

            if resp_path.exists() {
                let req_json: Value =
                    serde_json::from_str(&fs::read_to_string(req_path.path()).unwrap()).unwrap();
                let resp_json: Value =
                    serde_json::from_str(&fs::read_to_string(resp_path).unwrap()).unwrap();
                interactions.push((req_json, resp_json));
            }
        }

        Self { interactions }
    }

    pub async fn mount_all(&self, server: &MockServer) {
        for (req, resp) in &self.interactions {
            let matcher = GeminiMatcher::new(req.clone());
            Mock::given(method("POST"))
                .and(path_regex(r"^/v1beta/models/.*:generateContent"))
                .and(matcher)
                .respond_with(ResponseTemplate::new(200).set_body_json(resp.clone()))
                .mount(server)
                .await;
        }
    }
}

struct GeminiMatcher {
    expected_req: Value,
}

impl GeminiMatcher {
    fn new(expected_req: Value) -> Self {
        Self { expected_req }
    }
}

impl wiremock::Match for GeminiMatcher {
    fn matches(&self, request: &Request) -> bool {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(b) => b,
            Err(_) => return false,
        };

        let expected_contents = self.expected_req["contents"].as_array();
        let actual_contents = body["contents"].as_array();

        if expected_contents.is_none() || actual_contents.is_none() {
            return false;
        }

        let expected_contents = expected_contents.unwrap();
        let actual_contents = actual_contents.unwrap();

        // 1. Verify exact history length to distinguish between stages.
        if expected_contents.len() != actual_contents.len() {
            return false;
        }

        // 2. Verify the last message role matches.
        let expected_last = expected_contents.last().unwrap();
        let actual_last = actual_contents.last().unwrap();
        if expected_last["role"] != actual_last["role"] {
            return false;
        }

        // 3. Verify function interaction names in history match exactly.
        let get_func_names = |contents: &[Value]| -> Vec<String> {
            let mut names = Vec::new();
            for msg in contents {
                if let Some(parts) = msg["parts"].as_array() {
                    for part in parts {
                        if let Some(call) = part["functionCall"].as_object() {
                            names.push(format!("call:{}", call["name"].as_str().unwrap_or("")));
                        } else if let Some(resp) = part["functionResponse"].as_object() {
                            names.push(format!("resp:{}", resp["name"].as_str().unwrap_or("")));
                        }
                    }
                }
            }
            names
        };

        let expected_func_names = get_func_names(expected_contents);
        let actual_func_names = get_func_names(actual_contents);

        if expected_func_names != actual_func_names {
            return false;
        }

        // 4. Verify last message structure (parts length).
        let expected_parts = expected_last["parts"].as_array().unwrap();
        let actual_parts = actual_last["parts"].as_array().unwrap();
        if expected_parts.len() != actual_parts.len() {
            return false;
        }

        true
    }
}
