//! Direct-owner journal operations. The caller holds the `SessionManager` mutex;
//! these synchronous methods never await a controller or create another writer.
use super::{FileEntry, SessionManager};
use crate::clm::{
    journal::{ClmJournalOp, ClmJournalReply, ClmJournalView},
    ClmError, ProtectedActions,
};
use crate::session::clm::{selected_branch, validate_journal_commit, write_clm_entries};

impl SessionManager {
    /// Execute the acknowledged journal ABI under the existing owner lock.
    /// Hydrate through `ensure_full_history` before calling on a windowed manager.
    /// # Errors
    /// Incomplete history, stale views, invalid rows or uncertain persistence fail closed.
    pub fn clm_journal(
        &mut self,
        op: ClmJournalOp,
        protected: ProtectedActions,
    ) -> Result<ClmJournalReply, ClmError> {
        if let Some(error) = &self.clm_error {
            return Err(ClmError::Persistence(error.clone()));
        }
        if self.window.is_some() {
            return Err(ClmError::Journal(
                "hydrate full history before CLM journal access".into(),
            ));
        }
        let branch = selected_branch(&self.file_entries, self.leaf_id.as_deref())?;
        crate::clm::validate_clm_branch(&branch)?;
        let current = ClmJournalView {
            generation: self.clm_generation.clone(),
            session_id: self.session_id.clone(),
            leaf_id: self.leaf_id.clone(),
            branch,
            protected,
        };
        let ClmJournalOp::Commit(commit) = op else {
            if self.persist {
                let path = self
                    .session_file
                    .as_ref()
                    .ok_or_else(|| ClmError::Persistence("CLM journal has no file".into()))?;
                if let Err(error) = write_clm_entries(path, &self.file_entries) {
                    if matches!(error, ClmError::Persistence(_)) {
                        self.clm_error = Some(error.to_string());
                    }
                    return Err(error);
                }
                self.flushed = true;
            }
            return Ok(ClmJournalReply::View(current));
        };
        validate_journal_commit(&current, &commit)?;
        if !self.persist {
            return Err(ClmError::Persistence(
                "CLM requires persistent storage".into(),
            ));
        }
        let path = self
            .session_file
            .as_ref()
            .ok_or_else(|| ClmError::Persistence("CLM requires a session file".into()))?
            .clone();
        let mut row = commit.row;
        *super::super::entry_base_mut(&mut row)
            .ok_or_else(|| ClmError::Journal("missing CLM envelope".into()))? = self.next_base();
        // Match the wire-deserialized representation (flattened envelope extensions
        // are populated by serde on reopen), without changing raw snapshot values.
        let row: FileEntry = serde_json::from_value(
            serde_json::to_value(row).map_err(|e| ClmError::Journal(e.to_string()))?,
        )
        .map_err(|e| ClmError::Journal(e.to_string()))?;
        let mut branch = current.branch;
        branch.push(row.clone());
        crate::clm::validate_clm_branch(&branch)?;
        let snapshot = match &row {
            FileEntry::ClmOn { payload, .. } => &payload.boundary.snapshot,
            FileEntry::ClmRevision { payload, .. } => &payload.boundary.snapshot,
            FileEntry::ClmOffBoundary { payload, .. } => &payload.boundary.snapshot,
            _ => return Err(ClmError::Journal("commit requires a CLM boundary".into())),
        };
        crate::clm::validate_clm_snapshot_admission(&self.file_entries, snapshot)?;
        let mut entries = self.file_entries.clone();
        entries.push(row.clone());
        if let Err(error) = write_clm_entries(&path, &entries) {
            if matches!(error, ClmError::Persistence(_)) {
                self.clm_error = Some(error.to_string());
            }
            return Err(error);
        }
        self.file_entries = entries;
        self.build_index();
        self.flushed = true;
        // No success, accepted row or observer is published before sync and verification.
        for listener in &self.persist_listeners {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(&path)));
        }
        let view = ClmJournalView {
            branch,
            leaf_id: self.leaf_id.clone(),
            ..current
        };
        Ok(ClmJournalReply::Committed { row, view })
    }

    /// Adopt the exact acknowledged row into an externally persisted mirror.
    /// Idempotence is by authoritative ID and full row, never by message body.
    /// Comparison uses acknowledged usage, not derived child-accounting totals.
    /// # Errors
    /// Rejects a second durable owner, conflicting IDs, or missing parent delivery.
    pub fn adopt_authoritative_row(&mut self, row: FileEntry) -> Result<(), ClmError> {
        if self.persist {
            return Err(ClmError::Journal(
                "durable owner cannot adopt external rows".into(),
            ));
        }
        let id = row
            .id()
            .ok_or_else(|| ClmError::Journal("authoritative row has no ID".into()))?
            .to_owned();
        if let Some(index) = self.by_id.get(&id) {
            let mut acknowledged = self.file_entries[*index].clone();
            if let FileEntry::Message {
                message: super::AgentMessage::Assistant(assistant),
                ..
            } = &mut acknowledged
            {
                if let Some(usage) = self.authoritative_assistant_usage.get(&id) {
                    assistant.usage = *usage;
                }
            }
            return if acknowledged == row {
                Ok(())
            } else {
                Err(ClmError::Journal("authoritative ID collision".into()))
            };
        }
        if row.parent_id() != self.leaf_id.as_deref() {
            return Err(ClmError::Journal(
                "authoritative parent not yet adopted".into(),
            ));
        }
        if let FileEntry::Message {
            message: super::AgentMessage::Assistant(assistant),
            ..
        } = &row
        {
            self.authoritative_assistant_usage
                .insert(id.clone(), assistant.usage);
        }
        self.by_id.insert(id.clone(), self.file_entries.len());
        let accounting = matches!(row, FileEntry::ChildUsageAttributed { .. });
        self.file_entries.push(row);
        if accounting {
            super::super::apply_child_usage_attributions(&mut self.file_entries);
        }
        self.leaf_id = Some(id);
        Ok(())
    }
}
