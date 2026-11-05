use super::ProtectedActions;
use futures::future::BoxFuture;
use pa_types::session::FileEntry;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct ClmJournalView {
    pub generation: String,
    pub session_id: String,
    pub leaf_id: Option<String>,
    pub branch: Vec<FileEntry>,
    pub protected: ProtectedActions,
}
#[derive(Debug, Clone, PartialEq)]
pub struct ClmCommit {
    pub expected: ClmJournalView,
    pub row: FileEntry,
    pub allow_child_usage_suffix: bool,
}
// The frozen journal ABI carries owned rows and views, rather than boxed fields.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ClmJournalOp {
    ReadAfterEvents,
    Commit(ClmCommit),
}
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ClmJournalReply {
    View(ClmJournalView),
    Committed {
        row: FileEntry,
        view: ClmJournalView,
    },
}
pub type ClmJournalFn =
    Arc<dyn Fn(ClmJournalOp) -> BoxFuture<'static, anyhow::Result<ClmJournalReply>> + Send + Sync>;
