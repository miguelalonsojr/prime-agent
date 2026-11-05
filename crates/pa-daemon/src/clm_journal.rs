//! CLM acknowledgement at the existing `SessionFile` writer boundary. Call under
//! the worker's store lock, after its event channel has drained. No controller await.
use crate::session_store::{SessionEntry, SessionFile};
use pa_core::{
    clm::{
        journal::{ClmJournalOp, ClmJournalReply, ClmJournalView},
        ClmError, ProtectedActions,
    },
    session::clm::{selected_branch, validate_journal_commit, write_clm_entries},
};
use pa_types::session::FileEntry;

impl SessionFile {
    /// Full authoritative branch for model projection or a journal acknowledgement.
    /// # Errors
    /// Refuses damaged CLM, unresolved writes, truncated history and malformed known rows.
    pub fn branch_file_entries_checked(&self) -> anyhow::Result<Vec<FileEntry>> {
        if let Some(error) = &self.clm_error {
            return Err(ClmError::Persistence(error.clone()).into());
        }
        anyhow::ensure!(
            self.window.is_none(),
            "hydrate full history before CLM journal access"
        );
        let entries = self
            .entries
            .iter()
            .map(|entry| serde_json::from_value(serde_json::to_value(entry)?))
            .collect::<Result<Vec<FileEntry>, serde_json::Error>>()?;
        let branch = selected_branch(&entries, self.leaf_id.as_deref())?;
        pa_core::clm::validate_clm_branch(&branch)?;
        Ok(branch)
    }

    /// Model-visible messages; audit transcript readers remain separate.
    /// # Errors
    /// Invalid CLM data never falls back to covered audit messages.
    pub fn model_context_checked(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        if let Some(error) = &self.clm_error {
            return Err(ClmError::Persistence(error.clone()).into());
        }
        if !self.entries.iter().any(|entry| {
            matches!(
                entry.type_.as_str(),
                "clm_on" | "clm_revision" | "clm_off_boundary"
            )
        }) {
            return Ok(self.messages());
        }
        let branch = self.branch_file_entries_checked()?;
        if let Some(projection) = pa_core::clm::project_clm_branch(&branch)? {
            return Ok(projection
                .blocks
                .into_iter()
                .map(|block| block.message)
                .collect());
        }
        Ok(self.messages())
    }

    /// Execute one journal operation while the existing store mutex/lease is held.
    /// # Errors
    /// Rejects stale views, invalid snapshots and uncertain durable commits.
    pub fn clm_journal(
        &mut self,
        op: ClmJournalOp,
        protected: ProtectedActions,
    ) -> anyhow::Result<ClmJournalReply> {
        self.confirm_child_usage_write()?;
        let branch = self.branch_file_entries_checked()?;
        let current = ClmJournalView {
            generation: self.clm_generation.clone(),
            session_id: self.header.id.clone(),
            leaf_id: self.leaf_id.clone(),
            branch,
            protected,
        };
        let ClmJournalOp::Commit(commit) = op else {
            if !self.path.as_os_str().is_empty() {
                let mut all = vec![FileEntry::Header {
                    header: self.header.clone(),
                }];
                for entry in &self.entries {
                    all.push(serde_json::from_value(serde_json::to_value(entry)?)?);
                }
                self.publish_clm_entries(&all)?;
            }
            return Ok(ClmJournalReply::View(current));
        };
        validate_journal_commit(&current, &commit)?;
        anyhow::ensure!(
            !self.path.as_os_str().is_empty(),
            "CLM requires persistent storage"
        );
        // Rebuild only the envelope, retaining the immutable raw snapshot payload.
        let mut proposed = commit.row;
        let (FileEntry::ClmOn { base, .. }
        | FileEntry::ClmRevision { base, .. }
        | FileEntry::ClmOffBoundary { base, .. }) = &mut proposed
        else {
            return Err(ClmError::Journal("expected a CLM boundary".into()).into());
        };
        *base = pa_types::session::EntryBase {
            id: Some(uuid::Uuid::new_v4().to_string()),
            parent_id: self.leaf_id.clone(),
            timestamp: Some(crate::util::now_iso()),
            rest: serde_json::Map::default(),
        };
        let raw = serde_json::to_value(proposed)?;
        let row: FileEntry = serde_json::from_value(raw.clone())?;
        let stored: SessionEntry = serde_json::from_value(raw)?;
        let mut branch = current.branch;
        branch.push(row.clone());
        pa_core::clm::validate_clm_branch(&branch)?;
        let mut all = vec![FileEntry::Header {
            header: self.header.clone(),
        }];
        for entry in &self.entries {
            all.push(serde_json::from_value(serde_json::to_value(entry)?)?);
        }
        let snapshot = match &row {
            FileEntry::ClmOn { payload, .. } => &payload.boundary.snapshot,
            FileEntry::ClmRevision { payload, .. } => &payload.boundary.snapshot,
            FileEntry::ClmOffBoundary { payload, .. } => &payload.boundary.snapshot,
            _ => return Err(ClmError::Journal("commit requires a CLM boundary".into()).into()),
        };
        pa_core::clm::validate_clm_snapshot_admission(&all, snapshot)?;
        all.push(row.clone());
        self.publish_clm_entries(&all)?;
        self.push_index(stored);
        if let Some(traces) = &self.trace_upload {
            traces.persisted(&self.path);
        }
        Ok(ClmJournalReply::Committed {
            row,
            view: ClmJournalView {
                branch,
                leaf_id: self.leaf_id.clone(),
                ..current
            },
        })
    }
    fn publish_clm_entries(&mut self, entries: &[FileEntry]) -> anyhow::Result<()> {
        let publish = || -> anyhow::Result<()> { Ok(write_clm_entries(&self.path, entries)?) };
        let result = match &self.lease {
            Some(lease) => lease.with_owned_mutation(&self.path, publish),
            None => publish(),
        };
        if let Err(error) = result {
            if error
                .downcast_ref::<ClmError>()
                .is_some_and(|e| matches!(e, ClmError::Persistence(_)))
            {
                self.clm_error = Some(error.to_string());
            }
            return Err(error);
        }
        Ok(())
    }
}
