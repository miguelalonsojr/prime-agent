use super::{validate_clm_branch, ClmError};
use base64::Engine;
use pa_types::session::FileEntry;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedClmImport {
    pub entries: Vec<FileEntry>,
    pub source_session_id: String,
}

/// Returns a CLM safety error for invalid or unavailable input.
///
/// # Errors
/// Returns an error if the document, selected ancestry or budget violates CLM policy.
pub fn validate_clm_import(bytes: &[u8]) -> Result<Option<ValidatedClmImport>, ClmError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ClmError::journal(format!("invalid UTF-8 session log: {e}")))?;
    let mut entries = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| ClmError::journal(format!("damaged session log: {e}")))?;
        entries.push(
            serde_json::from_value::<FileEntry>(value)
                .map_err(|e| ClmError::journal(format!("invalid session row: {e}")))?,
        );
    }
    if !entries.iter().any(|e| {
        matches!(
            e,
            FileEntry::ClmOn { .. }
                | FileEntry::ClmRevision { .. }
                | FileEntry::ClmOffBoundary { .. }
        )
    }) {
        return Ok(None);
    }
    let Some(FileEntry::Header { header }) = entries.first() else {
        return Err(ClmError::journal("missing session header"));
    };
    if header.version != Some(crate::session::CURRENT_SESSION_VERSION) {
        return Err(ClmError::journal("unsupported CLM session version"));
    }
    let session_id = header.id.clone();
    for entry in entries.iter().filter(|row| {
        matches!(
            row,
            FileEntry::ClmOn { .. }
                | FileEntry::ClmRevision { .. }
                | FileEntry::ClmOffBoundary { .. }
        )
    }) {
        let snapshot = match entry {
            FileEntry::ClmOn { payload, .. } => &payload.boundary.snapshot,
            FileEntry::ClmRevision { payload, .. } => &payload.boundary.snapshot,
            FileEntry::ClmOffBoundary { payload, .. } => &payload.boundary.snapshot,
            _ => unreachable!(),
        };
        for block in snapshot.as_value()["blocks"]
            .as_array()
            .ok_or_else(|| ClmError::journal("missing snapshot blocks"))?
        {
            let message = &block["message"];
            if !matches!(
                message["role"].as_str(),
                Some(
                    "user"
                        | "assistant"
                        | "toolResult"
                        | "bashExecution"
                        | "custom"
                        | "branchSummary"
                        | "compactionSummary"
                )
            ) {
                return Err(ClmError::journal("unsupported CLM snapshot role"));
            }
            for item in message["content"].as_array().into_iter().flatten() {
                if item["type"] == "image"
                    && (!item["mimeType"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("image/"))
                        || item["data"].as_str().is_none_or(|data| {
                            base64::engine::general_purpose::STANDARD
                                .decode(data)
                                .is_err()
                        }))
                {
                    return Err(ClmError::journal("invalid CLM snapshot image"));
                }
            }
        }
    }
    let mut by_id = HashMap::new();
    for (index, entry) in entries.iter().enumerate().skip(1) {
        let id = entry
            .id()
            .ok_or_else(|| ClmError::journal("entry ID missing"))?;
        if id.is_empty() || by_id.insert(id, index).is_some() {
            return Err(ClmError::journal("duplicate or empty entry ID"));
        }
    }
    for (index, row) in entries.iter().enumerate() {
        if let FileEntry::Custom { payload, .. } = row {
            if matches!(
                payload.custom_type.as_str(),
                "clm_usage_v1" | "clm_usage_unknown_v1"
            ) {
                validate_usage(
                    payload.data.as_ref(),
                    payload.custom_type.as_str(),
                    row,
                    index,
                    &entries,
                    &by_id,
                )?;
            }
        }
    }
    for entry in entries.iter().skip(1).filter(|e| {
        matches!(
            e,
            FileEntry::ClmOn { .. }
                | FileEntry::ClmRevision { .. }
                | FileEntry::ClmOffBoundary { .. }
        )
    }) {
        let mut ancestry = Vec::new();
        let mut current = Some(entry);
        let mut seen = HashSet::new();
        while let Some(row) = current {
            let id = row
                .id()
                .ok_or_else(|| ClmError::journal("missing ancestor identity"))?;
            if !seen.insert(id) {
                return Err(ClmError::journal("cyclic ancestry"));
            }
            ancestry.push(row.clone());
            current = match row.parent_id() {
                Some(parent) => Some(
                    &entries[*by_id
                        .get(parent)
                        .ok_or_else(|| ClmError::journal("missing selected ancestor"))?],
                ),
                None => None,
            };
        }
        ancestry.reverse();
        validate_clm_branch(&ancestry)?;
    }
    Ok(Some(ValidatedClmImport {
        entries,
        source_session_id: session_id,
    }))
}

fn is_ancestor(
    id: &str,
    leaf: &FileEntry,
    entries: &[FileEntry],
    by_id: &HashMap<&str, usize>,
) -> bool {
    let mut current = Some(leaf);
    let mut visited = HashSet::new();
    while let Some(row) = current {
        if row.id() == Some(id) {
            return true;
        }
        let Some(parent) = row.parent_id() else {
            break;
        };
        if !visited.insert(parent) {
            break;
        }
        current = by_id.get(parent).and_then(|index| entries.get(*index));
    }
    false
}
fn validate_usage(
    data: Option<&Value>,
    kind: &str,
    receipt: &FileEntry,
    receipt_index: usize,
    entries: &[FileEntry],
    by_id: &HashMap<&str, usize>,
) -> Result<(), ClmError> {
    let data = data.ok_or_else(|| ClmError::journal("CLM usage receipt missing"))?;
    if data["version"] != 1
        || !matches!(data["pin"]["mode"].as_str(), Some("on" | "off"))
        || !data["pin"]["branchId"].is_null() && !data["pin"]["branchId"].is_string()
    {
        return Err(ClmError::journal("CLM usage pin invalid"));
    }
    let pin = &data["pin"];
    if let Some(branch_id) = pin["branchId"].as_str() {
        let index = *by_id
            .get(branch_id)
            .ok_or_else(|| ClmError::journal("CLM usage dispatch branch missing"))?;
        if index >= receipt_index || !is_ancestor(branch_id, receipt, entries, by_id) {
            return Err(ClmError::journal(
                "CLM usage dispatch branch is not ancestral",
            ));
        }
    } else if pin["mode"] == "on" {
        return Err(ClmError::journal("on usage requires dispatch branch"));
    }
    if let Some(entry_id) = pin["entryId"].as_str() {
        let index = *by_id
            .get(entry_id)
            .ok_or_else(|| ClmError::journal("CLM usage accepted entry missing"))?;
        let accepted = &entries[index];
        let epoch_revision = match accepted {
            FileEntry::ClmOn { payload, .. } => {
                Some((&payload.boundary.epoch_id, payload.boundary.revision, "on"))
            }
            FileEntry::ClmRevision { payload, .. } => {
                Some((&payload.boundary.epoch_id, payload.boundary.revision, "on"))
            }
            FileEntry::ClmOffBoundary { payload, .. } => {
                Some((&payload.boundary.epoch_id, payload.boundary.revision, "off"))
            }
            _ => None,
        };
        let Some((epoch, revision, mode)) = epoch_revision else {
            return Err(ClmError::journal("CLM usage entry is not accepted"));
        };
        if pin["epochId"].as_str() != Some(epoch)
            || pin["revision"].as_u64() != Some(revision)
            || pin["mode"] != mode
            || pin["branchId"].as_str().is_none_or(|branch| {
                !is_ancestor(
                    entry_id,
                    &entries[*by_id.get(branch).expect("checked branch ID")],
                    entries,
                    by_id,
                )
            })
        {
            return Err(ClmError::journal(
                "CLM usage pin does not match selected accepted entry",
            ));
        }
    } else if pin["mode"] == "on" || !pin["epochId"].is_null() || !pin["revision"].is_null() {
        return Err(ClmError::journal("CLM usage accepted identity missing"));
    }
    if kind == "clm_usage_v1" && !super::is_observed_clm_usage(&data["usage"]) {
        return Err(ClmError::journal("CLM observed usage invalid"));
    }
    Ok(())
}
