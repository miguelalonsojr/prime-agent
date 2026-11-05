//! Checked model-context reads. Audit readers retain their lenient parsing contract.
use super::{build_normal_session_context, SessionContext};
use crate::clm::{project_clm_branch, ClmError};
use pa_types::session::FileEntry;
use std::collections::{HashMap, HashSet};

/// Whether a row is an accepted CLM boundary.
#[must_use]
pub fn is_clm_entry(entry: &FileEntry) -> bool {
    matches!(
        entry,
        FileEntry::ClmOn { .. } | FileEntry::ClmRevision { .. } | FileEntry::ClmOffBoundary { .. }
    )
}

/// Reconstruct model context without replaying audit covered by an accepted boundary.
/// # Errors
/// Rejects incomplete CLM ancestry or an invalid accepted snapshot.
pub fn build_session_context_checked(
    entries: &[FileEntry],
    leaf_id: Option<&str>,
) -> Result<SessionContext, ClmError> {
    if !entries.iter().any(is_clm_entry) {
        return Ok(build_normal_session_context(entries, leaf_id));
    }
    let selected = leaf_id
        .filter(|id| !id.is_empty())
        .or_else(|| entries.last().and_then(FileEntry::id));
    let branch = selected_branch(entries, selected)?;
    let mut context = build_normal_session_context(entries, leaf_id);
    if let Some(projected) = project_clm_branch(&branch)? {
        context.messages = projected
            .blocks
            .into_iter()
            .map(|block| {
                serde_json::from_value(block.message)
                    .map_err(|error| ClmError::Journal(error.to_string()))
            })
            .collect::<Result<_, _>>()?;
    }
    Ok(context)
}

/// Select the complete ancestor chain, excluding the file header.
/// # Errors
/// Rejects missing IDs, duplicate IDs and cycles instead of returning a partial branch.
pub fn selected_branch(
    entries: &[FileEntry],
    leaf: Option<&str>,
) -> Result<Vec<FileEntry>, ClmError> {
    let mut by_id = HashMap::new();
    for entry in entries {
        if let Some(id) = entry.id() {
            if by_id.insert(id, entry).is_some() {
                return Err(ClmError::Journal("duplicate journal ID".into()));
            }
        }
    }
    let ordinary_ancestry = !entries.iter().any(is_clm_entry);
    let mut seen = HashSet::new();
    let mut current = leaf;
    let mut branch = Vec::new();
    while let Some(id) = current.filter(|id| !ordinary_ancestry || !id.is_empty()) {
        if !seen.insert(id) {
            return Err(ClmError::Journal("cyclic journal ancestry".into()));
        }
        let entry = by_id
            .get(id)
            .ok_or_else(|| ClmError::Journal("missing journal ancestor".into()))?;
        branch.push((*entry).clone());
        current = entry.parent_id();
    }
    branch.reverse();
    Ok(branch)
}

/// Recognize CLM tags even in a torn row. Valid ordinary JSON is inspected by tag,
/// not by message text. Torn rows are conservative because their tag may be lost.
#[must_use]
pub fn contains_clm_records(bytes: &[u8]) -> bool {
    bytes.split(|b| *b == b'\n').any(|line| {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
            matches!(
                value["type"].as_str(),
                Some("clm_on" | "clm_revision" | "clm_off_boundary")
            )
        } else {
            let text = String::from_utf8_lossy(line);
            text.match_indices("\"type\"").any(|(at, key)| {
                text[at + key.len()..]
                    .trim_start()
                    .strip_prefix(':')
                    .is_some_and(|rest| rest.trim_start().starts_with("\"clm_"))
            })
        }
    })
}

/// Parse model history, retaining ordinary leniency only for non-CLM logs.
/// # Errors
/// Recognized CLM with parse damage, invalid boundaries or missing ancestry fails closed.
pub fn parse_session_entries_checked(bytes: &[u8]) -> Result<Vec<FileEntry>, ClmError> {
    if contains_clm_records(bytes) {
        crate::clm::validate_clm_import(bytes)?;
    }
    let text = std::str::from_utf8(bytes).map_err(|e| ClmError::Journal(e.to_string()))?;
    Ok(super::parse_session_entries(text))
}

/// Check an optimistic CLM commit under the existing owner's exclusive lock.
/// Child accounting may update audit usage but cannot alter message identity/content.
/// # Errors
/// Rejects owner replacement, changed protection, dropped ancestry and non-accounting suffixes.
pub fn validate_journal_commit(
    current: &crate::clm::journal::ClmJournalView,
    commit: &crate::clm::journal::ClmCommit,
) -> Result<(), ClmError> {
    let expected = &commit.expected;
    if current.generation != expected.generation
        || current.session_id != expected.session_id
        || current.protected != expected.protected
    {
        return Err(ClmError::Journal(
            "journal owner or protection changed".into(),
        ));
    }
    let mut expected_branch = expected.branch.clone();
    if commit.allow_child_usage_suffix {
        crate::clm::validate_child_usage_suffix(&current.branch, expected.leaf_id.as_deref())?;
        expected_branch.extend(current.branch.iter().skip(expected.branch.len()).cloned());
        super::apply_child_usage_attributions(&mut expected_branch);
    }
    if current.branch != expected_branch
        || (!commit.allow_child_usage_suffix && current.leaf_id != expected.leaf_id)
    {
        return Err(ClmError::Journal(
            "journal changed during CLM action".into(),
        ));
    }
    if !is_clm_entry(&commit.row) {
        return Err(ClmError::Journal("commit requires a CLM boundary".into()));
    }
    Ok(())
}

fn journal_comparison_rows(
    entries: &[FileEntry],
) -> Result<Vec<serde_json::Value>, serde_json::Error> {
    let ordinary_ancestry = !entries.iter().any(is_clm_entry);
    entries
        .iter()
        .map(|entry| {
            let mut value = serde_json::to_value(entry)?;
            if ordinary_ancestry && matches!(entry, FileEntry::Unknown { .. }) {
                if let Some(fields) = value.as_object_mut() {
                    if fields
                        .get("parentId")
                        .is_some_and(serde_json::Value::is_null)
                    {
                        fields.remove("parentId");
                    }
                }
            }
            Ok(value)
        })
        .collect()
}

/// Publish the owner's staged append as one synced JSONL replacement. This is not
/// another durable owner: callers hold their existing writer lock/lease throughout.
/// # Errors
/// Any write, file/directory sync, rename or byte verification failure is uncertainty.
pub fn write_clm_entries(path: &std::path::Path, entries: &[FileEntry]) -> Result<(), ClmError> {
    use std::io::Write;
    let canonical;
    let path = if path.exists() {
        canonical =
            std::fs::canonicalize(path).map_err(|e| ClmError::Persistence(e.to_string()))?;
        canonical.as_path()
    } else {
        path
    };
    let parent = path
        .parent()
        .ok_or_else(|| ClmError::Journal("CLM journal directory missing".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| ClmError::Journal(format!("CLM directory unavailable: {e}")))?;
    #[cfg(test)]
    super::clm_tests::io_fault("directory-preflight", path)
        .map_err(|e| ClmError::Journal(format!("CLM directory sync unavailable: {e}")))?;
    let directory = std::fs::File::open(parent)
        .and_then(|directory| {
            directory.sync_all()?;
            Ok(directory)
        })
        .map_err(|e| ClmError::Journal(format!("CLM directory sync unavailable: {e}")))?;
    let result = (|| -> anyhow::Result<()> {
        let mut bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let persisted = if bytes.is_empty() {
            Vec::new()
        } else {
            parse_session_entries_checked(&bytes)?
        };
        anyhow::ensure!(
            persisted.len() <= entries.len(),
            "CLM journal changed on disk"
        );
        let prefix_len = persisted.len();
        let mut combined = persisted;
        combined.extend_from_slice(&entries[prefix_len..]);
        super::apply_child_usage_attributions(&mut combined);
        // Compare serialized semantic rows: typed envelopes may have duplicated
        // flattened extension fields after deserialization, but no data may change.
        anyhow::ensure!(
            journal_comparison_rows(&combined)? == journal_comparison_rows(entries)?,
            "CLM journal changed on disk"
        );
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        for entry in &entries[prefix_len..] {
            serde_json::to_writer(&mut bytes, entry)?;
            bytes.push(b'\n');
        }
        // Validate the actual publication, including parse damage in an ordinary
        // prefix that would become unsafe when the first CLM boundary is added.
        crate::clm::validate_clm_import(&bytes)?;
        if prefix_len == entries.len() {
            return Ok(());
        }
        let temp = path.with_extension(format!("clm-{}.tmp", uuid::Uuid::new_v4()));
        let published = (|| -> anyhow::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            crate::platform::perms::set_private_mode(&mut options);
            let mut file = options.open(&temp)?;
            #[cfg(test)]
            super::clm_tests::io_fault("write", path)?;
            file.write_all(&bytes)?;
            #[cfg(test)]
            super::clm_tests::io_fault("file-sync", path)?;
            file.sync_all()?;
            drop(file);
            crate::platform::rename_onto(&temp, path)?;
            #[cfg(test)]
            super::clm_tests::io_fault("post-rename", path)?;
            directory.sync_all()?;
            #[cfg(test)]
            super::clm_tests::io_fault("readback", path)?;
            anyhow::ensure!(
                std::fs::read(path)? == bytes,
                "CLM committed bytes could not be verified"
            );
            Ok(())
        })();
        if published.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        published
    })();
    result.map_err(|e| ClmError::Persistence(e.to_string()))
}

/// Capture model context with its exact durable IDs, including synthesized summaries.
/// CLM branches use the shared projection; ordinary branches use the existing normal fold.
/// # Errors
/// Incomplete ancestry, missing message IDs and invalid accepted context fail closed.
pub fn build_model_context_blocks_checked(
    entries: &[FileEntry],
    leaf_id: Option<&str>,
) -> Result<Vec<pa_types::clm::ClmBlock>, ClmError> {
    let selected = leaf_id
        .filter(|id| !id.is_empty())
        .or_else(|| entries.last().and_then(FileEntry::id));
    let branch = selected_branch(entries, selected)?;
    if let Some(projection) = project_clm_branch(&branch)? {
        return Ok(projection.blocks);
    }
    let (context, ids) = super::build_normal_session_context_with_ids(entries, leaf_id);
    context
        .messages
        .into_iter()
        .zip(ids)
        .map(|(message, id)| {
            if id.is_empty() {
                return Err(ClmError::Journal(
                    "CLM context has no durable entry ID".into(),
                ));
            }
            Ok(pa_types::clm::ClmBlock {
                id,
                message: serde_json::to_value(message)
                    .map_err(|e| ClmError::Journal(e.to_string()))?,
            })
        })
        .collect()
}
