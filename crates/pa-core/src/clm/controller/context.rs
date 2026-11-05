use super::super::{ClmError, ProtectedActions};
use super::{
    mirror, parse_context_document, project_clm_branch, render_context_document, ClmJournalView,
};
use pa_types::{
    clm::{ClmBlock, ClmBoundary, ClmMode, ClmSnapshot},
    session::FileEntry,
};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone)]
pub(super) struct Context {
    pub snapshot: ClmSnapshot,
    pub pending: Vec<ClmBlock>,
    pub watermark: Option<String>,
}
pub(super) fn boundary(row: &FileEntry) -> Option<&ClmBoundary> {
    match row {
        FileEntry::ClmOn { payload, .. } => Some(&payload.boundary),
        FileEntry::ClmRevision { payload, .. } => Some(&payload.boundary),
        FileEntry::ClmOffBoundary { payload, .. } => Some(&payload.boundary),
        _ => None,
    }
}
pub(super) fn snapshot_blocks(snapshot: &ClmSnapshot) -> Result<Vec<ClmBlock>, ClmError> {
    serde_json::from_value(snapshot.as_value()["blocks"].clone())
        .map_err(|e| ClmError::Document(e.to_string()))
}
pub(super) fn current_watermark(branch: &[FileEntry]) -> Option<String> {
    branch
        .iter()
        .rev()
        .find(|row| {
            matches!(
                row,
                FileEntry::Message { .. }
                    | FileEntry::CustomMessage { .. }
                    | FileEntry::Compaction { .. }
                    | FileEntry::BranchSummary { .. }
            )
        })
        .and_then(FileEntry::id)
        .map(str::to_owned)
}
pub(super) fn protected_ids(
    snapshot: &ClmSnapshot,
    actions: &ProtectedActions,
) -> Result<HashSet<String>, ClmError> {
    if actions
        .iter()
        .any(|(id, blocks)| id.is_empty() || blocks.is_empty())
    {
        return Err(ClmError::Protection(
            "protected action has no identity or original blocks".into(),
        ));
    }
    super::super::validate_protection(snapshot, actions)?;
    Ok(actions.values().flatten().map(|b| b.id.clone()).collect())
}
pub(super) fn accepted_context(
    view: &ClmJournalView,
    enforce_limit: bool,
) -> Result<Context, ClmError> {
    build_context(view, enforce_limit, None)
}
pub(super) fn failed_provider_context(
    view: &ClmJournalView,
    failed_entry: &ClmBlock,
) -> Result<Context, ClmError> {
    build_context(view, true, Some(failed_entry))
}
fn build_context(
    view: &ClmJournalView,
    enforce_limit: bool,
    failed_entry: Option<&ClmBlock>,
) -> Result<Context, ClmError> {
    let projection = project_clm_branch(&view.branch)?
        .ok_or_else(|| ClmError::Journal("CLM requires an accepted on snapshot".into()))?;
    if projection.state.mode != ClmMode::On {
        return Err(ClmError::Journal("CLM is off".into()));
    }
    let accepted = projection
        .state
        .snapshot
        .as_ref()
        .ok_or_else(|| ClmError::Journal("missing accepted snapshot".into()))?;
    let accepted_blocks = snapshot_blocks(accepted)?;
    let accepted_count = accepted_blocks.len();
    let mut blocks = projection.blocks;
    if let Some(failed) = failed_entry {
        if blocks.len() <= accepted_count
            || blocks.last() != Some(failed)
            || failed.message["role"] != "assistant"
            || failed.message["stopReason"] != "error"
            || accepted_blocks.iter().any(|block| block.id == failed.id)
        {
            return Err(ClmError::Journal(
                "failed provider cleanup requires the exact last uncovered failed entry".into(),
            ));
        }
        blocks.pop();
    }
    if blocks[accepted_count..]
        .iter()
        .any(|block| block.message["role"] == "assistant" && block.message["stopReason"] == "error")
    {
        return Err(ClmError::Journal(
            "uncovered failed provider entry requires owned cleanup".into(),
        ));
    }
    let mut complete = blocks.len();
    if let Some(index) = blocks
        .iter()
        .rposition(|b| b.message["role"] == "assistant")
    {
        if index >= accepted_count {
            let calls: Vec<_> = blocks[index].message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v["type"] == "toolCall")
                .collect();
            let tail = &blocks[index + 1..];
            if calls.len() > tail.len() && tail.iter().all(|b| b.message["role"] == "toolResult") {
                let ids: HashSet<_> = calls.iter().map(|c| c["id"].as_str()).collect();
                if ids.contains(&None)
                    || ids.len() != calls.len()
                    || tail.iter().zip(&calls).any(|(b, c)| {
                        b.message["toolCallId"] != c["id"] || b.message["toolName"] != c["name"]
                    })
                {
                    return Err(ClmError::Document(
                        "pending tool group is mismatched or reordered".into(),
                    ));
                }
                complete = index;
            }
        }
    }
    // Retain raw accepted block fields and ordering; only uncovered audit blocks
    // originate in the typed owner projection.
    let mut raw = accepted.as_value().clone();
    let raw_blocks = raw
        .get_mut("blocks")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ClmError::Document("snapshot blocks missing".into()))?;
    for block in &blocks[accepted_count..complete] {
        raw_blocks
            .push(serde_json::to_value(block).map_err(|e| ClmError::Document(e.to_string()))?);
    }
    let snapshot = ClmSnapshot::from_value(raw);
    let rendered = render_context_document(&snapshot, &HashSet::new())?;
    if enforce_limit {
        mirror::check_size(&rendered)?;
    }
    parse_context_document(&rendered, &snapshot)?;
    let watermark = failed_entry.map(|failed| failed.id.clone()).or_else(|| {
        blocks[accepted_count..complete]
            .last()
            .map(|b| b.id.clone())
            .or(projection.state.audit_watermark)
    });
    Ok(Context {
        snapshot,
        pending: blocks[complete..].to_vec(),
        watermark,
    })
}
