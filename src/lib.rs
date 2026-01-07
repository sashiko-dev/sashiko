pub mod ai;
pub mod api;
pub mod baseline;
pub mod db;
pub mod events;
pub mod git_ops;
pub mod ingestor;
pub mod inspector;
pub mod nntp;
pub mod patch;
pub mod reviewer;
pub mod settings;
pub mod worker;

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStatus {
    Incomplete,
    Pending,
    Applying,
    InReview,
    Cancelled,
    Reviewed,
    Failed,
}

impl fmt::Display for ReviewStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReviewStatus::Incomplete => write!(f, "Incomplete"),
            ReviewStatus::Pending => write!(f, "Pending"),
            ReviewStatus::Applying => write!(f, "Applying"),
            ReviewStatus::InReview => write!(f, "In Review"),
            ReviewStatus::Cancelled => write!(f, "Cancelled"),
            ReviewStatus::Reviewed => write!(f, "Reviewed"),
            ReviewStatus::Failed => write!(f, "Failed"),
        }
    }
}

impl FromStr for ReviewStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Incomplete" => Ok(ReviewStatus::Incomplete),
            "Pending" => Ok(ReviewStatus::Pending),
            "Applying" => Ok(ReviewStatus::Applying),
            "In Review" => Ok(ReviewStatus::InReview),
            "Cancelled" => Ok(ReviewStatus::Cancelled),
            "Reviewed" => Ok(ReviewStatus::Reviewed),
            "Failed" => Ok(ReviewStatus::Failed),
            _ => Err(()),
        }
    }
}

impl ReviewStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewStatus::Incomplete => "Incomplete",
            ReviewStatus::Pending => "Pending",
            ReviewStatus::Applying => "Applying",
            ReviewStatus::InReview => "In Review",
            ReviewStatus::Cancelled => "Cancelled",
            ReviewStatus::Reviewed => "Reviewed",
            ReviewStatus::Failed => "Failed",
        }
    }
}
