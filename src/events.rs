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

use crate::patch::{Patch, PatchsetMetadata};

#[derive(Debug)]
#[allow(dead_code)]
pub enum Event {
    ArticleFetched {
        group: String,
        article_id: String,
        content: Vec<String>,
        raw: Option<Vec<u8>>,
        baseline: Option<String>,
    },
    PatchSubmitted {
        group: String,
        article_id: String,
        message_id: String,
        subject: String,
        author: String,
        message: String,
        diff: String,
        base_commit: Option<String>,
        timestamp: i64,
        index: u32,
        total: u32,
        mr_url: Option<String>,
        mr_title: Option<String>,
        mr_number: Option<i64>,
    },
    RawMboxSubmitted {
        raw: String,
        group: String,
        baseline: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    IngestionFailed {
        article_id: String,
        error: String,
    },
}

#[derive(Debug)]
pub struct ParsedArticle {
    pub group: String,
    pub article_id: String,
    pub metadata: Option<PatchsetMetadata>,
    pub patch: Option<Patch>,
    pub baseline: Option<String>,
    pub failed_error: Option<String>,
    pub skip_filters: Option<Vec<String>>,
    pub only_filters: Option<Vec<String>>,
    pub mr_url: Option<String>,
    pub mr_title: Option<String>,
    pub mr_number: Option<i64>,
}
