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

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct Pagination {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub q: Option<String>,
    pub mailing_list: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PatchsetsResponse {
    pub items: Vec<crate::db::PatchsetRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Serialize, Deserialize)]
pub struct MessagesResponse {
    pub items: Vec<crate::db::MessageRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Deserialize)]
pub struct PatchQuery {
    pub id: String,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

#[derive(Deserialize)]
pub struct ReviewQuery {
    pub id: Option<i64>,
    pub patchset_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct BugQuery {
    pub id: Option<i64>,
    pub bugid: Option<String>,
    pub slug: Option<String>,
}

#[derive(Deserialize)]
pub struct BugListQuery {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub q: Option<String>,
    pub subsystem: Option<String>,
    pub subsystems: Option<String>,
    pub min_severity: Option<String>,
    pub severity: Option<String>,
    /// Triage state. Also accepts the historical `status` spelling.
    #[serde(alias = "status")]
    pub lifecycle_status: Option<String>,
    /// Analysis execution state.
    pub pipeline_state: Option<String>,
    /// Filters by assignee. The literal `none` selects unassigned bugs.
    pub assignee: Option<String>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
}

#[derive(Deserialize)]
pub struct BugSubsystemsQuery {
    #[serde(alias = "status")]
    pub lifecycle_status: Option<String>,
}

#[derive(Deserialize)]
pub struct RerunPatchQuery {
    pub patchset_id: i64,
    pub patch_id: i64,
}

#[derive(Deserialize)]
pub struct SubsystemQuery {
    pub subsystem_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct CancelQuery {
    pub id: i64,
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize)]
pub struct InjectRequest {
    pub raw: String,
    pub group: Option<String>,
    pub baseline: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubmitRequest {
    Inject {
        raw: String,
        base_commit: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    Remote {
        sha: String,
        repo: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    #[serde(rename = "remote-range")]
    RemoteRange {
        sha: String,
        repo: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    Thread {
        msgid: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct SubmitResponse {
    pub status: String,
    pub id: String,
}

#[derive(Deserialize)]
pub struct AnalyzeBugPayload {
    #[serde(flatten)]
    pub input: crate::workflows::linux_bug::BugInput,
    pub tool: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BugActionPayload {
    /// Client-declared provenance. Author always comes from authentication.
    pub tool: Option<String>,
    pub model: Option<String>,
    #[serde(flatten)]
    pub action: BugAction,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BugAction {
    Comment {
        content: String,
    },
    Close {
        reason: Option<String>,
    },
    Dismiss {
        reason: Option<String>,
    },
    MarkDuplicate {
        duplicate_of_id: Option<i64>,
        duplicate_of_bugid: Option<String>,
        reasoning: Option<String>,
    },
    /// Hands a bug to someone, or drops the assignment when the assignee is
    /// absent, empty, or null.
    Assign {
        assignee: Option<String>,
        reason: Option<String>,
    },
}

#[derive(Deserialize)]
pub struct RequestLinkRequest {
    #[serde(default)]
    pub email: String,
}

#[derive(Deserialize)]
pub struct VerifyLinkQuery {
    pub token: String,
}

#[derive(Deserialize)]
pub struct CreateApiTokenRequest {
    pub email: Option<String>,
    pub max_bug_access: Option<String>,
    pub expires_in_days: Option<u64>,
}
