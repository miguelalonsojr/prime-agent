use super::{checksum::snapshot_checksum, ClmError, MAX_CUMULATIVE_SNAPSHOT_BYTES};
use pa_types::{
    clm::{ClmBlock, ClmBoundary, ClmBranchState, ClmMode, ClmSnapshot},
    session::{AgentMessage, BranchSummaryMessage, CompactionSummaryMessage, FileEntry},
};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
pub struct ClmProjection {
    pub state: ClmBranchState,
    pub blocks: Vec<ClmBlock>,
    pub audit_watermark: Option<String>,
}
fn boundary(entry: &FileEntry) -> Option<(&ClmBoundary, ClmMode)> {
    match entry {
        FileEntry::ClmOn { payload, .. } => Some((&payload.boundary, ClmMode::On)),
        FileEntry::ClmRevision { payload, .. } => Some((&payload.boundary, ClmMode::On)),
        FileEntry::ClmOffBoundary { payload, .. } => Some((&payload.boundary, ClmMode::Off)),
        _ => None,
    }
}
fn blocks(snapshot: &ClmSnapshot) -> Result<Vec<ClmBlock>, ClmError> {
    let values = snapshot
        .as_value()
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or_else(|| ClmError::journal("missing blocks"))?;
    let mut ids = HashSet::new();
    values
        .iter()
        .map(|v| {
            let block: ClmBlock =
                serde_json::from_value(v.clone()).map_err(|e| ClmError::journal(e.to_string()))?;
            if block.id.is_empty()
                || !ids.insert(block.id.clone())
                || !block.message.get("role").is_some_and(Value::is_string)
            {
                return Err(ClmError::journal("invalid snapshot block identity or role"));
            }
            Ok(block)
        })
        .collect()
}
fn watermark_index(
    branch: &[FileEntry],
    index: usize,
    watermark: Option<&str>,
) -> Result<usize, ClmError> {
    let Some(watermark) = watermark else {
        return Ok(index);
    };
    branch[..index]
        .iter()
        .position(|entry| {
            entry.id() == Some(watermark)
                && matches!(
                    entry,
                    FileEntry::Message { .. }
                        | FileEntry::CustomMessage { .. }
                        | FileEntry::BranchSummary { .. }
                        | FileEntry::Compaction { .. }
                )
        })
        .ok_or_else(|| ClmError::journal("audit watermark missing or invalid"))
}
fn add_snapshot_bytes(total: usize, added: usize, cap: usize) -> Result<usize, ClmError> {
    let total = total
        .checked_add(added)
        .ok_or_else(|| ClmError::Budget("CLM cumulative snapshot cap exceeded".into()))?;
    if total > cap {
        return Err(ClmError::Budget(
            "CLM cumulative snapshot cap exceeded".into(),
        ));
    }
    Ok(total)
}
/// Admit a new snapshot against all stored boundary snapshots in the session.
/// Headers, ordinary audit entries and boundary envelopes do not count.
/// # Errors
/// Returns `Budget` when the cumulative JS-serialized UTF-8 size exceeds 512 MiB.
pub fn validate_clm_snapshot_admission(
    entries: &[FileEntry],
    candidate: &ClmSnapshot,
) -> Result<(), ClmError> {
    validate_snapshot_admission_with_limit(entries, candidate, MAX_CUMULATIVE_SNAPSHOT_BYTES)
}

fn validate_snapshot_admission_with_limit(
    entries: &[FileEntry],
    candidate: &ClmSnapshot,
    limit: usize,
) -> Result<(), ClmError> {
    let mut bytes = add_snapshot_bytes(
        0,
        super::checksum::js_stringify(candidate.as_value())?.len(),
        limit,
    )?;
    for entry in entries {
        if let Some((accepted, _)) = boundary(entry) {
            bytes = add_snapshot_bytes(
                bytes,
                super::checksum::js_stringify(accepted.snapshot.as_value())?.len(),
                limit,
            )?;
        }
    }
    Ok(())
}

/// Validate historical structure without making a new-write admission decision.
/// # Errors
/// Rejects invalid ancestry, snapshot structure, watermarks, revisions and checksums.
pub fn validate_clm_branch(branch: &[FileEntry]) -> Result<ClmBranchState, ClmError> {
    let mut previous: Option<(&ClmBoundary, ClmMode)> = None;
    for (index, entry) in branch.iter().enumerate() {
        if !branch.iter().any(|e| boundary(e).is_some()) {
            break;
        }
        if matches!(entry, FileEntry::Header { .. })
            || entry.id().is_none()
            || entry.parent_id() != branch.get(index.wrapping_sub(1)).and_then(FileEntry::id)
        {
            return Err(ClmError::journal("CLM branch ancestry is incomplete"));
        }
        let Some((accepted, mode)) = boundary(entry) else {
            continue;
        };
        if accepted.epoch_id.is_empty() || accepted.revision > 9_007_199_254_740_991 {
            return Err(ClmError::journal("invalid epoch or unsafe revision"));
        }
        blocks(&accepted.snapshot)?;
        watermark_index(branch, index, accepted.audit_watermark.as_deref())?;
        match entry {
            FileEntry::ClmOn { .. } => {
                if accepted.revision != 0
                    || previous.is_some_and(|(_, previous_mode)| previous_mode != ClmMode::Off)
                    || previous.is_some_and(|(prev, _)| prev.epoch_id == accepted.epoch_id)
                {
                    return Err(ClmError::journal("invalid enable epoch"));
                }
            }
            FileEntry::ClmRevision { payload, .. } => {
                let Some((prev, ClmMode::On)) = previous else {
                    return Err(ClmError::journal("revision lacks active ancestor"));
                };
                if accepted.epoch_id != prev.epoch_id
                    || accepted.revision != prev.revision + 1
                    || payload.base_revision != prev.revision
                    || payload.observed_action_id.is_empty()
                    || payload.checksum != snapshot_checksum(&accepted.snapshot)?
                {
                    return Err(ClmError::journal(
                        "invalid revision ancestry, receipt or checksum",
                    ));
                }
            }
            FileEntry::ClmOffBoundary { .. } => {
                let Some((prev, ClmMode::On)) = previous else {
                    return Err(ClmError::journal("off lacks active ancestor"));
                };
                if accepted.epoch_id != prev.epoch_id || accepted.revision != prev.revision + 1 {
                    return Err(ClmError::journal("invalid off revision"));
                }
            }
            _ => unreachable!(),
        }
        previous = Some((accepted, mode));
    }
    Ok(match previous {
        Some((b, mode)) => ClmBranchState {
            mode,
            epoch_id: Some(b.epoch_id.clone()),
            revision: Some(b.revision),
            snapshot: Some(b.snapshot.clone()),
            audit_watermark: b.audit_watermark.clone(),
        },
        None => ClmBranchState {
            mode: ClmMode::Off,
            epoch_id: None,
            revision: None,
            snapshot: None,
            audit_watermark: None,
        },
    })
}
/// Returns a CLM safety error for invalid or unavailable input.
///
/// # Errors
/// Returns an error if the document, selected ancestry or budget violates CLM policy.
pub fn validate_child_usage_suffix(
    branch: &[FileEntry],
    captured_leaf: Option<&str>,
) -> Result<(), ClmError> {
    let Some(captured_leaf) = captured_leaf else {
        return if branch.is_empty() {
            Ok(())
        } else {
            Err(ClmError::journal("captured leaf missing"))
        };
    };
    let Some(index) = branch
        .iter()
        .position(|entry| entry.id() == Some(captured_leaf))
    else {
        return Err(ClmError::journal("captured leaf missing"));
    };
    if branch[index + 1..]
        .iter()
        .any(|entry| !matches!(entry, FileEntry::ChildUsageAttributed { .. }))
    {
        return Err(ClmError::journal("non-accounting row after captured leaf"));
    }
    Ok(())
}
fn suffix_block(entry: &FileEntry) -> Option<ClmBlock> {
    let id = entry.id()?.to_owned();
    let message = match entry {
        FileEntry::Message { message, .. } => message.clone(),
        FileEntry::CustomMessage { payload, .. } => {
            AgentMessage::Custom(crate::session::create_custom_message(payload, entry))
        }
        FileEntry::BranchSummary { payload, .. } if !payload.summary.is_empty() => {
            AgentMessage::BranchSummary(BranchSummaryMessage {
                summary: payload.summary.clone(),
                from_id: payload.from_id.clone(),
                timestamp: crate::session::timestamp_to_millis(entry.timestamp()),
            })
        }
        _ => return None,
    };
    Some(ClmBlock {
        id,
        message: serde_json::to_value(message).ok()?,
    })
}
fn digest(block: &ClmBlock) -> bool {
    block.message["role"] == "custom"
        && block.message["customType"]
            == crate::session_engine::headless::HARNESS_DIGEST_CUSTOM_TYPE
}
fn keep_newest_digest(blocks: Vec<ClmBlock>, keep_id: Option<&str>) -> Vec<ClmBlock> {
    blocks
        .into_iter()
        .filter(|block| !digest(block) || Some(block.id.as_str()) == keep_id)
        .collect()
}
/// Returns a CLM safety error for invalid or unavailable input.
///
/// # Errors
/// Returns an error if the document, selected ancestry or budget violates CLM policy.
pub fn project_clm_branch(branch: &[FileEntry]) -> Result<Option<ClmProjection>, ClmError> {
    let state = validate_clm_branch(branch)?;
    let Some(snapshot) = &state.snapshot else {
        return Ok(None);
    };
    let accepted_index = branch
        .iter()
        .rposition(|entry| boundary(entry).is_some())
        .ok_or_else(|| ClmError::journal("accepted boundary missing"))?;
    let start = watermark_index(branch, accepted_index, state.audit_watermark.as_deref())? + 1;
    let mut projected = blocks(snapshot)?;
    let suffix: Vec<_> = branch[start..].iter().filter_map(suffix_block).collect();
    if state.mode == ClmMode::On {
        projected.extend(suffix);
    } else {
        let after = &branch[start..];
        let compaction = after
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, entry)| match entry {
                FileEntry::Compaction { payload, .. } => Some((index, payload)),
                _ => None,
            });
        let newest_digest_id = projected
            .iter()
            .chain(suffix.iter())
            .rev()
            .find(|block| digest(block))
            .map(|block| block.id.clone());
        if let Some((compaction_at, payload)) = compaction {
            let prior: Vec<_> = projected
                .into_iter()
                .chain(after[..compaction_at].iter().filter_map(suffix_block))
                .collect();
            let later: Vec<_> = after[compaction_at + 1..]
                .iter()
                .filter_map(suffix_block)
                .collect();
            let newer_digest = later.iter().any(digest);
            let snapshot_outranks_digest = payload.harness_digest.is_some() && !newer_digest;
            let keep_digest_id = (!snapshot_outranks_digest)
                .then_some(newest_digest_id.as_deref())
                .flatten();
            let retained = prior
                .into_iter()
                .skip_while(|block| block.id != payload.first_kept_entry_id)
                .collect();
            let retained = keep_newest_digest(retained, keep_digest_id);
            let summary_digest = (!newer_digest)
                .then(|| payload.harness_digest.clone())
                .flatten();
            let summary = AgentMessage::CompactionSummary(CompactionSummaryMessage {
                summary: payload.summary.clone(),
                tokens_before: payload.tokens_before,
                retained_message_count: Some(retained.len() as u64),
                custom_instructions: payload.custom_instructions.clone(),
                harness_digest: summary_digest.clone(),
                harness_state_fingerprint: summary_digest
                    .and(payload.harness_state_fingerprint.clone()),
                timestamp: crate::session::timestamp_to_millis(after[compaction_at].timestamp()),
            });
            projected = vec![ClmBlock {
                id: after[compaction_at]
                    .id()
                    .ok_or_else(|| ClmError::journal("compaction ID missing"))?
                    .to_owned(),
                message: serde_json::to_value(summary)
                    .map_err(|e| ClmError::journal(e.to_string()))?,
            }];
            projected.extend(retained);
            projected.extend(keep_newest_digest(later, keep_digest_id));
        } else {
            projected.extend(suffix);
            projected = keep_newest_digest(projected, newest_digest_id.as_deref());
        }
    }
    Ok(Some(ClmProjection {
        audit_watermark: state.audit_watermark.clone(),
        state,
        blocks: projected,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cumulative_snapshot_cap_counts_all_accepted_bytes() {
        assert_eq!(add_snapshot_bytes(0, 6, 10).unwrap(), 6);
        assert_eq!(add_snapshot_bytes(6, 4, 10).unwrap(), 10);
        assert!(add_snapshot_bytes(10, 1, 10).is_err());
        assert!(add_snapshot_bytes(usize::MAX, 1, MAX_CUMULATIVE_SNAPSHOT_BYTES).is_err());
        assert_eq!(MAX_CUMULATIVE_SNAPSHOT_BYTES, 512 * 1024 * 1024);
    }
    #[test]
    fn snapshot_admission_counts_flat_history_once_and_ignores_envelopes() {
        let snapshot = ClmSnapshot::from_value(serde_json::json!({"blocks":[],"future":"same"}));
        let rows = [
            serde_json::json!({"type":"session","version":3,"id":"s","cwd":"/tmp","timestamp":"now","future":"ignored header"}),
            serde_json::json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":"ignored audit bytes","timestamp":1}}),
            serde_json::json!({"type":"clm_on","id":"old","parentId":"u","epochId":"old","revision":0,"snapshot":snapshot,"auditWatermark":"u","futureEnvelope":"ignored"}),
            serde_json::json!({"type":"clm_off_boundary","id":"off-a","parentId":"old","epochId":"old","revision":1,"snapshot":snapshot,"auditWatermark":"u"}),
            serde_json::json!({"type":"clm_off_boundary","id":"off-b","parentId":"old","epochId":"old","revision":1,"snapshot":snapshot,"auditWatermark":"u"}),
            serde_json::json!({"type":"clm_on","id":"new","parentId":"u","epochId":"new","revision":0,"snapshot":snapshot,"auditWatermark":"u"}),
        ].into_iter().map(|row|serde_json::from_value::<FileEntry>(row).unwrap()).collect::<Vec<_>>();
        let bytes = br#"{"blocks":[],"future":"same"}"#.len();
        // Four stored rows, not two selected rows or three duplicated ancestor paths.
        assert!(validate_snapshot_admission_with_limit(&rows, &snapshot, 5 * bytes).is_ok());
        assert!(matches!(
            validate_snapshot_admission_with_limit(&rows, &snapshot, 5 * bytes - 1),
            Err(ClmError::Budget(_))
        ));
        assert!(validate_snapshot_admission_with_limit(&rows[5..], &snapshot, 2 * bytes).is_ok());
        assert!(matches!(
            validate_snapshot_admission_with_limit(&rows, &snapshot, 2 * bytes),
            Err(ClmError::Budget(_))
        ));
        assert!(validate_clm_snapshot_admission(&rows, &snapshot).is_ok());
    }
    #[test]
    fn snapshot_admission_uses_js_utf8_and_all_raw_fields() {
        let snapshot = ClmSnapshot::from_value(
            serde_json::from_str(r#"{"blocks":[],"future":"é🙂\n","10":1e21,"2":-0.0}"#).unwrap(),
        );
        let expected = "{\"2\":0,\"10\":1e+21,\"blocks\":[],\"future\":\"é🙂\\n\"}";
        assert_eq!(
            super::super::checksum::js_stringify(snapshot.as_value()).unwrap(),
            expected
        );
        assert!(validate_snapshot_admission_with_limit(&[], &snapshot, expected.len()).is_ok());
        assert!(matches!(
            validate_snapshot_admission_with_limit(&[], &snapshot, expected.len() - 1),
            Err(ClmError::Budget(_))
        ));
        assert!(matches!(
            validate_snapshot_admission_with_limit(&[], &snapshot, expected.encode_utf16().count()),
            Err(ClmError::Budget(_))
        ));
    }
}
