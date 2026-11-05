//! CLM policy, acknowledged journal contract and disposable mirror lifecycle.
mod budget;
mod checksum;
mod context_document;
pub mod controller;
mod import;
pub mod journal;
mod reconstruction;
mod usage;
pub use usage::{clm_usage_receipt, get_clm_measured_usage, is_observed_clm_usage};

pub use budget::estimate_clm_request_tokens;
pub use checksum::snapshot_checksum;
pub use context_document::{parse_context_document, render_context_document, validate_protection};
pub use import::{validate_clm_import, ValidatedClmImport};
pub use reconstruction::{
    project_clm_branch, validate_child_usage_suffix, validate_clm_branch,
    validate_clm_snapshot_admission, ClmProjection,
};

use pa_types::clm::ClmBlock;
use std::collections::BTreeMap;

pub type ProtectedActions = BTreeMap<String, Vec<ClmBlock>>;
pub const MAX_EDITABLE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CUMULATIVE_SNAPSHOT_BYTES: usize = 512 * 1024 * 1024;
pub const CONTEXT_WARNING_RATIO: f64 = 0.75;
pub const CONTEXT_TARGET_RATIO: f64 = 0.60;

#[derive(Debug, thiserror::Error)]
pub enum ClmError {
    #[error("invalid CLM document: {0}")]
    Document(String),
    #[error("CLM protection violation: {0}")]
    Protection(String),
    #[error("CLM request budget exceeded: {0}")]
    Budget(String),
    #[error("CLM journal conflict: {0}")]
    Journal(String),
    #[error("CLM writer unresolved or unknown: {0}")]
    Writer(String),
    #[error("CLM persistence uncertain: {0}")]
    Persistence(String),
}
impl ClmError {
    fn document(msg: impl Into<String>) -> Self {
        Self::Document(msg.into())
    }
    fn journal(msg: impl Into<String>) -> Self {
        Self::Journal(msg.into())
    }
}
