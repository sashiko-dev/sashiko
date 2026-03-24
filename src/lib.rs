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

pub mod ai;
pub mod api;
pub mod baseline;
pub mod db;
pub mod email_policy;
pub mod email_router;
pub mod events;
pub mod fetcher;
pub mod gerrit;
pub mod git_ops;
pub mod ingestor;
pub mod inspector;
pub mod nntp;
pub mod patch;
pub mod patchwork;
pub mod reviewer;
pub mod settings;
pub mod utils;
pub mod worker;

use std::fmt;
use std::str::FromStr;

/// Represents the current status of a patchset review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStatus {
    /// The patchset is not yet fully received (e.g., waiting for more parts).
    Incomplete,
    /// The patchset is complete and waiting for review.
    Pending,
    /// The patchset is currently under AI review.
    InReview,
    /// The review process was cancelled.
    Cancelled,
    /// The review was skipped (e.g., due to configuration or filtering).
    Skipped,
    /// The review process completed successfully.
    Reviewed,
    /// The review process failed due to an error.
    Failed,
    /// The patchset failed to apply to the baseline.
    FailedToApply,
}

impl fmt::Display for ReviewStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReviewStatus::Incomplete => write!(f, "Incomplete"),
            ReviewStatus::Pending => write!(f, "Pending"),
            ReviewStatus::InReview => write!(f, "In Review"),
            ReviewStatus::Cancelled => write!(f, "Cancelled"),
            ReviewStatus::Skipped => write!(f, "Skipped"),
            ReviewStatus::Reviewed => write!(f, "Reviewed"),
            ReviewStatus::Failed => write!(f, "Failed"),
            ReviewStatus::FailedToApply => write!(f, "Failed To Apply"),
        }
    }
}

impl FromStr for ReviewStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Incomplete" => Ok(ReviewStatus::Incomplete),
            "Pending" => Ok(ReviewStatus::Pending),
            "In Review" => Ok(ReviewStatus::InReview),
            "Cancelled" => Ok(ReviewStatus::Cancelled),
            "Skipped" => Ok(ReviewStatus::Skipped),
            "Reviewed" => Ok(ReviewStatus::Reviewed),
            "Failed" => Ok(ReviewStatus::Failed),
            "Failed To Apply" => Ok(ReviewStatus::FailedToApply),
            _ => Err(()),
        }
    }
}

impl ReviewStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewStatus::Incomplete => "Incomplete",
            ReviewStatus::Pending => "Pending",
            ReviewStatus::InReview => "In Review",
            ReviewStatus::Cancelled => "Cancelled",
            ReviewStatus::Skipped => "Skipped",
            ReviewStatus::Reviewed => "Reviewed",
            ReviewStatus::Failed => "Failed",
            ReviewStatus::FailedToApply => "Failed To Apply",
        }
    }
}
pub mod logging;
