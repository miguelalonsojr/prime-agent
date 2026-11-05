//! One session lane for accepted CLM state and its disposable working mirror.
mod context;
mod mirror;
#[cfg(test)]
mod tests;

use super::{
    estimate_clm_request_tokens, get_clm_measured_usage,
    journal::{ClmCommit, ClmJournalFn, ClmJournalOp, ClmJournalReply, ClmJournalView},
    parse_context_document, project_clm_branch, render_context_document, snapshot_checksum,
    validate_clm_branch, ClmError, ClmProjection,
};
use context::{accepted_context, boundary, protected_ids, snapshot_blocks, Context};
use pa_types::{
    ai::{Model, Tool},
    clm::{
        ClmAcceptedView, ClmBoundary, ClmBranchState, ClmMode, ClmModeSource, ClmOffBoundaryEntry,
        ClmOnEntry, ClmRequestBudget, ClmRevisionEntry, ClmRevisionRef, ClmStatus, ClmStatusSource,
    },
    session::{EntryBase, FileEntry},
};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClmToolOutcome {
    Completed,
    Aborted,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ClmRequestInputs {
    pub system_prompt: String,
    pub tools: Vec<Tool>,
    pub model: Model,
}

#[derive(Clone)]
struct Action {
    id: String,
    generation: u64,
    expected: ClmJournalView,
    context: Context,
}
struct State {
    accepted: ClmBranchState,
    journal_generation: String,
    session_id: String,
    published_hash: Option<[u8; 32]>,
    action: Option<Action>,
    cancellation: u64,
    marker: Option<PathBuf>,
    conflict: Option<String>,
    error: Option<String>,
    estimate: Option<ClmRequestBudget>,
}

/// Owns only the disposable mirror. The acknowledged journal owns accepted rows.
/// Callers must retain one controller per session and must not hold the journal
/// owner's lock while awaiting these methods.
pub struct ClmController {
    journal: ClmJournalFn,
    session_file: Option<PathBuf>,
    path: PathBuf,
    lane: tokio::sync::Mutex<()>,
    state: Mutex<State>,
}

impl ClmController {
    /// Read the acknowledged state without importing or overwriting working bytes.
    /// Mirror initialization is deferred so oversized accepted views remain readable.
    /// # Errors
    /// Rejects an invalid journal or a retained writer marker.
    pub async fn new(
        journal: ClmJournalFn,
        session_file: Option<PathBuf>,
        artifact_dir: PathBuf,
    ) -> Result<Self, ClmError> {
        let reply = journal(ClmJournalOp::ReadAfterEvents)
            .await
            .map_err(journal_error)?;
        let ClmJournalReply::View(view) = reply else {
            return Err(ClmError::Journal(
                "read did not acknowledge a journal view".into(),
            ));
        };
        let accepted = validate_clm_branch(&view.branch)?;
        let path = std::path::absolute(artifact_dir)
            .map_err(|e| ClmError::Persistence(e.to_string()))?
            .join("clm/context.txt");
        let controller = Self {
            journal,
            session_file,
            path,
            lane: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                accepted,
                journal_generation: view.generation,
                session_id: view.session_id,
                published_hash: None,
                action: None,
                cancellation: 0,
                marker: None,
                conflict: None,
                error: None,
                estimate: None,
            }),
        };
        controller.assert_no_marker()?;
        Ok(controller)
    }

    /// Guard and project accepted context; this is not provider budget authorization.
    /// # Errors
    /// Rejects writer uncertainty, drift, changed ancestry or invalid protection.
    pub async fn project_request(&self) -> Result<ClmProjection, ClmError> {
        let _lane = self.lane.lock().await;
        let result = async {
            let view = self.read().await?;
            self.assert_idle(&view)?;
            let context = accepted_context(&view, true)?;
            protected_ids(&context.snapshot, &view.protected)?;
            self.check_drift(&context).await?;
            let current = self.read().await?;
            self.check_fence(&view, &current, true)?;
            project_clm_branch(&current.branch)?
                .ok_or_else(|| ClmError::Journal("CLM has no accepted boundary".into()))
        }
        .await;
        self.remember(result)
    }

    /// Check child admission without importing working bytes or authorizing model work.
    /// # Errors
    /// Rejects uncertain ownership, changed accepted state or an unsettled writer.
    pub(crate) async fn assert_child_admission_available(
        &self,
        origin_action_id: Option<&str>,
    ) -> Result<(), ClmError> {
        let _lane = self.lane.lock().await;
        let result = async {
            let action = origin_action_id
                .map(|id| self.owned_action(id))
                .transpose()?;
            if let Some(action) = &action {
                self.check_action(action)?;
            }
            let view = self.read().await?;
            match action {
                Some(action) => self.check_action(&action),
                None => self.assert_idle(&view),
            }
        }
        .await;
        self.remember(result)
    }

    /// Publish a protected editable mirror and admit one owned tool writer.
    /// # Errors
    /// Rejects drift, missing persistence, conflicting actions or changed ancestry.
    pub async fn prepare_tool(&self, tool_call_id: &str) -> Result<(), ClmError> {
        let _lane = self.lane.lock().await;
        let result = async {
            let view = self.read().await?;
            self.assert_idle(&view)?;
            if tool_call_id.is_empty() {
                return Err(ClmError::Writer("tool action ID is required".into()));
            }
            let context = accepted_context(&view, true)?;
            self.check_drift(&context).await?;
            let ids = protected_ids(&context.snapshot, &view.protected)?;
            self.publish(&context.snapshot, &ids).await?;
            let current = self.read().await?;
            self.check_fence(&view, &current, true)?;
            self.arm_marker()?;
            let mut state = self.lock()?;
            state.action = Some(Action {
                id: tool_call_id.into(),
                generation: state.cancellation,
                expected: view,
                context,
            });
            Ok(())
        }
        .await;
        self.remember(result)
    }

    /// Accept a completed owned edit using these exact edit-budget inputs.
    /// # Errors
    /// Aborted/unknown writers remain locked. Invalid edits are rejected and repaired
    /// only when journal state and writer ownership are still known.
    pub async fn settle_tool(
        &self,
        tool_call_id: &str,
        outcome: ClmToolOutcome,
        inputs: &ClmRequestInputs,
    ) -> Result<(), ClmError> {
        let _lane = self.lane.lock().await;
        let result = async {
            let action = self.owned_action(tool_call_id)?;
            if outcome != ClmToolOutcome::Completed {
                self.cancel_tool(tool_call_id);
                return Err(ClmError::Writer(
                    "cancelled or unknown writer cannot be repaired".into(),
                ));
            }
            let before = self.read().await?;
            self.check_fence(&action.expected, &before, true)?;
            let candidate = async {
                let text = self.read_mirror().await?;
                self.check_action(&action)?;
                let current = self.read().await?;
                self.check_action(&action)?;
                self.check_fence(&action.expected, &current, true)?;
                if Some(mirror::hash(&text)) == self.lock()?.published_hash {
                    return Ok(None);
                }
                if !action.context.pending.is_empty() && action.context.watermark.is_none() {
                    return Err(ClmError::Document(
                        "cannot watermark an edit before a pending tool group".into(),
                    ));
                }
                let snapshot = parse_context_document(&text, &action.context.snapshot)?;
                let ids = protected_ids(&snapshot, &action.expected.protected)?;
                // Use the renderer's exact source encoding, not tool-name or payload inference.
                let references = render_context_document(&snapshot, &ids)?;
                for line in references
                    .lines()
                    .filter(|line| line.starts_with("[[CTX_PROTECTED"))
                {
                    if !text.lines().any(|actual| actual == line) {
                        return Err(ClmError::Protection(
                            "protected reference was relabelled".into(),
                        ));
                    }
                }
                let mut messages: Vec<_> = snapshot_blocks(&snapshot)?
                    .into_iter()
                    .map(|b| b.message)
                    .collect();
                messages.extend(action.context.pending.iter().map(|b| b.message.clone()));
                let estimate = estimate_clm_request_tokens(
                    &messages,
                    &inputs.system_prompt,
                    &inputs.tools,
                    &inputs.model,
                )?;
                let exceeded = estimate
                    .max_input_tokens
                    .is_some_and(|max| estimate.estimated_input_tokens > max);
                self.record_request_estimate(estimate);
                if exceeded {
                    return Err(ClmError::Budget(
                        "shorten context or restore an accepted revision".into(),
                    ));
                }
                mirror::check_size(&render_context_document(&snapshot, &HashSet::new())?)?;
                self.check_action(&action)?;
                let row = self.revision_row(
                    snapshot.clone(),
                    action.context.watermark.clone(),
                    tool_call_id,
                )?;
                self.commit(action.expected.clone(), row, true).await?;
                Ok(Some((snapshot, ids)))
            }
            .await;
            match candidate {
                Ok(Some((snapshot, ids))) => {
                    self.check_action(&action)?;
                    self.publish_accepted(&snapshot, &ids).await?;
                    self.check_action(&action)?;
                    self.clear_marker()?;
                    self.lock()?.action = None;
                    Ok(())
                }
                Ok(None) => {
                    self.publish(
                        &action.context.snapshot,
                        &protected_ids(&action.context.snapshot, &action.expected.protected)?,
                    )
                    .await?;
                    let current = self.read().await?;
                    self.check_action(&action)?;
                    self.check_fence(&action.expected, &current, true)?;
                    self.clear_marker()?;
                    self.lock()?.action = None;
                    Ok(())
                }
                Err(error) => {
                    if self.check_action(&action).is_ok() {
                        if let Err(repair) = self.repair_rejected(&action).await {
                            self.poison(format!("repair failed or commit is uncertain: {repair}"));
                        }
                    }
                    Err(error)
                }
            }
        }
        .await;
        self.remember(result)
    }

    /// Invalidate an import synchronously, including a suspended mirror read.
    /// Native Python Done is not authority to clear this cancellation or its marker.
    pub fn cancel_tool(&self, tool_call_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            if state
                .action
                .as_ref()
                .is_some_and(|action| action.id == tool_call_id)
            {
                state.cancellation = state.cancellation.wrapping_add(1);
                let error = "cancelled writer requires physical settlement and explicit repair; native repair is unavailable".to_owned();
                state.conflict = Some(error.clone());
                state.error = Some(error);
            }
        }
    }

    /// Cover only the canonical failed assistant tail while retaining its raw audit.
    /// The caller must keep this request's owned delivery protection live until return.
    pub(crate) async fn discard_failed_provider_entry(
        &self,
        failed_entry: &pa_types::clm::ClmBlock,
    ) -> Result<(), ClmError> {
        let _lane = self.lane.lock().await;
        let result = async {
            let view = self.read().await?;
            self.assert_idle(&view)?;
            if view.protected.is_empty() {
                return Err(ClmError::Protection(
                    "failed provider cleanup requires an owned request".into(),
                ));
            }
            let context = context::failed_provider_context(&view, failed_entry)?;
            if !context.pending.is_empty() {
                return Err(ClmError::Document(
                    "failed provider cleanup cannot cover a pending tool group".into(),
                ));
            }
            let ids = protected_ids(&context.snapshot, &view.protected)?;
            // A fresh mirror may initialize only from acknowledged accepted state,
            // not from this candidate whose failed audit tail is still uncovered.
            let baseline = Context {
                snapshot: self
                    .lock()?
                    .accepted
                    .snapshot
                    .clone()
                    .ok_or_else(|| ClmError::Journal("missing accepted snapshot".into()))?,
                pending: Vec::new(),
                watermark: None,
            };
            self.check_drift(&baseline).await?;
            let row = self.revision_row(
                context.snapshot,
                Some(failed_entry.id.clone()),
                &format!("failed-provider:{}", failed_entry.id),
            )?;
            let committed = self.commit(view, row, false).await?;
            let confirmed = validate_clm_branch(&committed.branch)?
                .snapshot
                .ok_or_else(|| ClmError::Journal("acknowledged cleanup snapshot missing".into()))?;
            self.publish_accepted(&confirmed, &ids).await
        }
        .await;
        self.remember(result)
    }

    /// Append an on/off transition, or return the existing state for a no-op.
    /// Defaults are selected by the session owner and are not reloaded here.
    /// # Errors
    /// Rejects unresolved writers, incomplete tools, invalid context or persistence failure.
    pub async fn set_mode(
        &self,
        enabled: bool,
        source: ClmModeSource,
    ) -> Result<ClmStatus, ClmError> {
        let _lane = self.lane.lock().await;
        let result = async {
            let view = self.read().await?;
            self.assert_idle(&view)?;
            let state = validate_clm_branch(&view.branch)?;
            if (state.mode == ClmMode::On) == enabled {
                return self.status_from(&view, false);
            }
            let row = if enabled {
                let blocks = crate::session::clm::build_model_context_blocks_checked(
                    &view.branch,
                    view.leaf_id.as_deref(),
                )?;
                let mut snapshot = pa_types::clm::ClmSnapshot::from_blocks(blocks).into_value();
                if let Some(previous) = &state.snapshot {
                    let originals = previous.as_value()["blocks"]
                        .as_array()
                        .ok_or_else(|| ClmError::Document("snapshot blocks missing".into()))?;
                    for block in snapshot["blocks"].as_array_mut().ok_or_else(|| {
                        ClmError::Document("constructed snapshot has no blocks".into())
                    })? {
                        if let Some(original) = originals
                            .iter()
                            .find(|original| original["id"] == block["id"])
                        {
                            // Projection retained this durable ID. Preserve its raw block,
                            // including unknown fields; do not reserialize a typed helper.
                            *block = original.clone();
                        }
                    }
                }
                let snapshot = pa_types::clm::ClmSnapshot::from_value(snapshot);
                let document = render_context_document(&snapshot, &HashSet::new())?;
                mirror::check_size(&document)?;
                parse_context_document(&document, &snapshot)?;
                FileEntry::ClmOn {
                    payload: ClmOnEntry {
                        boundary: ClmBoundary {
                            epoch_id: uuid::Uuid::new_v4().to_string(),
                            revision: 0,
                            snapshot,
                            audit_watermark: context::current_watermark(&view.branch),
                        },
                        mode_source: Some(source),
                    },
                    base: pending_base(),
                }
            } else {
                let context = accepted_context(&view, true)?;
                if !context.pending.is_empty() {
                    return Err(ClmError::Document(
                        "cannot turn off with an unfinished tool group".into(),
                    ));
                }
                self.check_drift(&context).await?;
                FileEntry::ClmOffBoundary {
                    payload: ClmOffBoundaryEntry {
                        boundary: ClmBoundary {
                            epoch_id: state
                                .epoch_id
                                .ok_or_else(|| ClmError::Journal("missing epoch".into()))?,
                            revision: state
                                .revision
                                .ok_or_else(|| ClmError::Journal("missing revision".into()))?
                                + 1,
                            snapshot: context.snapshot,
                            audit_watermark: context.watermark,
                        },
                        mode_source: Some(source),
                    },
                    base: pending_base(),
                }
            };
            let snapshot = boundary(&row)
                .ok_or_else(|| ClmError::Journal("constructed row has no CLM boundary".into()))?
                .snapshot
                .clone();
            let committed = self.commit(view, row, false).await?;
            if enabled {
                self.publish_accepted(
                    &snapshot,
                    &snapshot_blocks(&snapshot)?
                        .into_iter()
                        .map(|b| b.id)
                        .collect(),
                )
                .await?;
            } else {
                self.lock()?.published_hash = None;
            }
            self.status_from(&committed, false)
        }
        .await;
        self.remember(result)
    }

    /// Restore an ancestor in the active epoch by appending a new revision.
    /// # Errors
    /// Rejects unknown writers, nonancestors, drift, incomplete tools or failed admission.
    pub async fn restore(&self, target: ClmRevisionRef) -> Result<ClmStatus, ClmError> {
        let _lane = self.lane.lock().await;
        let result = async {
            let view = self.read().await?;
            self.assert_idle(&view)?;
            let state = validate_clm_branch(&view.branch)?;
            if state.mode != ClmMode::On || state.epoch_id.as_ref() != Some(&target.epoch_id) {
                return Err(ClmError::Journal(
                    "restore requires the active epoch".into(),
                ));
            }
            let snapshot = view
                .branch
                .iter()
                .filter(|row| !matches!(row, FileEntry::ClmOffBoundary { .. }))
                .filter_map(boundary)
                .find(|b| b.epoch_id == target.epoch_id && b.revision == target.revision)
                .ok_or_else(|| {
                    ClmError::Journal("restore target is not a selected ancestor".into())
                })?
                .snapshot
                .clone();
            let context = accepted_context(&view, true)?;
            if !context.pending.is_empty() {
                return Err(ClmError::Document(
                    "cannot restore with an unfinished tool group".into(),
                ));
            }
            let rendered = render_context_document(&snapshot, &HashSet::new())?;
            mirror::check_size(&rendered)?;
            parse_context_document(&rendered, &snapshot)?;
            protected_ids(&snapshot, &view.protected)?;
            self.check_drift(&context).await?;
            let action = format!("restore:{}:{}", target.epoch_id, target.revision);
            let row = self.revision_row(
                snapshot.clone(),
                context::current_watermark(&view.branch),
                &action,
            )?;
            let committed = self.commit(view, row, false).await?;
            self.publish_accepted(
                &snapshot,
                &snapshot_blocks(&snapshot)?
                    .into_iter()
                    .map(|b| b.id)
                    .collect(),
            )
            .await?;
            self.status_from(&committed, false)
        }
        .await;
        self.remember(result)
    }

    /// Read accepted JSONL only, never pending mirror bytes.
    /// # Errors
    /// Rejects unresolved writers or invalid accepted context, not its editable size.
    pub async fn accepted_view(&self) -> Result<ClmAcceptedView, ClmError> {
        let _lane = self.lane.lock().await;
        let result = async {
            let view = self.read().await?;
            self.assert_idle(&view)?;
            let projection = project_clm_branch(&view.branch)?
                .ok_or_else(|| ClmError::Journal("no accepted CLM boundary".into()))?;
            if projection.state.mode == ClmMode::On {
                accepted_context(&view, false)?;
            } else if let Some(snapshot) = &projection.state.snapshot {
                parse_context_document(
                    &render_context_document(snapshot, &HashSet::new())?,
                    snapshot,
                )?;
            }
            Ok(ClmAcceptedView {
                epoch_id: projection
                    .state
                    .epoch_id
                    .ok_or_else(|| ClmError::Journal("missing epoch".into()))?,
                revision: projection
                    .state
                    .revision
                    .ok_or_else(|| ClmError::Journal("missing revision".into()))?,
                messages: projection.blocks.into_iter().map(|b| b.message).collect(),
            })
        }
        .await;
        self.remember(result)
    }

    /// Read mode metadata and provider-only measured usage from an acknowledged view.
    /// # Errors
    /// Invalid or uncertain journal data remains an error, never zero usage.
    pub async fn status(&self, configured_default: bool) -> Result<ClmStatus, ClmError> {
        let _lane = self.lane.lock().await;
        let result = async { self.status_from(&self.read().await?, configured_default) }.await;
        self.remember(result)
    }

    /// Store diagnostic occupancy only: no row writes, input caching or authorization.
    pub fn record_request_estimate(&self, estimate: ClmRequestBudget) {
        if let Ok(mut state) = self.state.lock() {
            state.estimate = Some(estimate);
        }
    }

    /// Guard an owner-fenced operation against the last acknowledged effective mode.
    /// # Errors
    /// Rejects on mode, an active writer or an uncertain controller.
    pub fn require_off(&self, operation: &str) -> Result<(), ClmError> {
        let state = self.lock()?;
        if state.accepted.mode == ClmMode::On || state.action.is_some() || state.conflict.is_some()
        {
            return Err(ClmError::Writer(format!("turn CLM off before {operation}")));
        }
        drop(state);
        self.assert_no_marker()
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, ClmError> {
        self.state
            .lock()
            .map_err(|_| ClmError::Writer("controller state is poisoned".into()))
    }
    fn remember<T>(&self, result: Result<T, ClmError>) -> Result<T, ClmError> {
        if let Err(error) = &result {
            if let Ok(mut state) = self.lock() {
                state.error = Some(error.to_string());
            }
        }
        result
    }
    fn poison(&self, message: String) {
        if let Ok(mut state) = self.lock() {
            state.conflict = Some(message.clone());
            state.error = Some(message);
        }
    }
    async fn read(&self) -> Result<ClmJournalView, ClmError> {
        let reply = (self.journal)(ClmJournalOp::ReadAfterEvents)
            .await
            .map_err(journal_error)?;
        let ClmJournalReply::View(view) = reply else {
            return Err(ClmError::Journal("read returned no view".into()));
        };
        let accepted = validate_clm_branch(&view.branch)?;
        let mut state = self.lock()?;
        if (view.generation != state.journal_generation
            || view.session_id != state.session_id
            || accepted != state.accepted)
            && state.conflict.is_none()
        {
            let error = "accepted state changed; reopen the controller".to_owned();
            state.conflict = Some(error.clone());
            state.error = Some(error);
        }
        drop(state);
        Ok(view)
    }
    fn assert_idle(&self, view: &ClmJournalView) -> Result<(), ClmError> {
        let state = self.lock()?;
        if let Some(conflict) = &state.conflict {
            return Err(ClmError::Writer(conflict.clone()));
        }
        if state.action.is_some() {
            return Err(ClmError::Writer(
                "tool action writer has not settled".into(),
            ));
        }
        if view.generation != state.journal_generation
            || view.session_id != state.session_id
            || validate_clm_branch(&view.branch)? != state.accepted
        {
            return Err(ClmError::Journal(
                "accepted state changed; reopen the controller".into(),
            ));
        }
        drop(state);
        self.assert_no_marker()
    }
    fn owned_action(&self, id: &str) -> Result<Action, ClmError> {
        let state = self.lock()?;
        if let Some(conflict) = &state.conflict {
            return Err(ClmError::Writer(conflict.clone()));
        }
        let action = state
            .action
            .as_ref()
            .filter(|a| a.id == id)
            .cloned()
            .ok_or_else(|| ClmError::Writer("tool action does not own this mirror".into()))?;
        let marker = state
            .marker
            .clone()
            .ok_or_else(|| ClmError::Writer("owned writer marker is missing".into()))?;
        drop(state);
        if !std::fs::symlink_metadata(marker).is_ok_and(|metadata| metadata.is_file()) {
            self.poison("owned writer marker is missing or invalid".into());
            return Err(ClmError::Writer(
                "owned writer marker is missing or invalid".into(),
            ));
        }
        Ok(action)
    }
    fn check_action(&self, action: &Action) -> Result<(), ClmError> {
        let current = self.owned_action(&action.id)?;
        if current.generation != action.generation || self.lock()?.cancellation != action.generation
        {
            return Err(ClmError::Writer("tool import was cancelled".into()));
        }
        Ok(())
    }
    fn check_fence(
        &self,
        expected: &ClmJournalView,
        current: &ClmJournalView,
        allow_child_usage_suffix: bool,
    ) -> Result<(), ClmError> {
        // Reuse the owner comparator, including accounting-derived usage normalization.
        // This row is only a comparison witness; no commit is submitted here.
        let row = expected
            .branch
            .iter()
            .rev()
            .find(|row| boundary(row).is_some())
            .cloned()
            .ok_or_else(|| ClmError::Journal("captured CLM boundary missing".into()))?;
        let result = crate::session::clm::validate_journal_commit(
            current,
            &ClmCommit {
                expected: expected.clone(),
                row,
                allow_child_usage_suffix,
            },
        );
        if let Err(error) = &result {
            self.poison(format!("session changed during CLM operation: {error}"));
        }
        result
    }
    fn revision_row(
        &self,
        snapshot: pa_types::clm::ClmSnapshot,
        watermark: Option<String>,
        action: &str,
    ) -> Result<FileEntry, ClmError> {
        let state = self.lock()?;
        let revision = state
            .accepted
            .revision
            .ok_or_else(|| ClmError::Journal("missing accepted revision".into()))?;
        let epoch_id = state
            .accepted
            .epoch_id
            .clone()
            .ok_or_else(|| ClmError::Journal("missing accepted epoch".into()))?;
        Ok(FileEntry::ClmRevision {
            payload: ClmRevisionEntry {
                checksum: snapshot_checksum(&snapshot)?,
                observed_action_id: action.into(),
                base_revision: revision,
                boundary: ClmBoundary {
                    epoch_id,
                    revision: revision + 1,
                    snapshot,
                    audit_watermark: watermark,
                },
            },
            base: pending_base(),
        })
    }
    async fn commit(
        &self,
        expected: ClmJournalView,
        row: FileEntry,
        allow_child_usage_suffix: bool,
    ) -> Result<ClmJournalView, ClmError> {
        let wanted = boundary(&row)
            .cloned()
            .ok_or_else(|| ClmError::Journal("commit is not a CLM boundary".into()))?;
        let reply = (self.journal)(ClmJournalOp::Commit(ClmCommit {
            expected,
            row,
            allow_child_usage_suffix,
        }))
        .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                let error = journal_error(error);
                if matches!(error, ClmError::Persistence(_) | ClmError::Journal(_)) {
                    self.poison(error.to_string());
                }
                return Err(error);
            }
        };
        let ClmJournalReply::Committed { row, view } = reply else {
            self.poison("commit acknowledgement is unavailable".into());
            return Err(ClmError::Persistence(
                "commit acknowledgement is unavailable".into(),
            ));
        };
        if boundary(&row) != Some(&wanted)
            || row.id().is_none()
            || view.branch.last() != Some(&row)
            || view.leaf_id.as_deref() != row.id()
        {
            self.poison("accepted row cannot be verified".into());
            return Err(ClmError::Persistence(
                "accepted row cannot be verified".into(),
            ));
        }
        let accepted = validate_clm_branch(&view.branch)?;
        let mut state = self.lock()?;
        state.accepted = accepted;
        state.error = None;
        Ok(view)
    }
    fn status_from(
        &self,
        view: &ClmJournalView,
        configured_default: bool,
    ) -> Result<ClmStatus, ClmError> {
        let accepted = validate_clm_branch(&view.branch)?;
        let source = view
            .branch
            .iter()
            .rev()
            .find_map(|row| match row {
                FileEntry::ClmOn { payload, .. } => Some(payload.mode_source),
                FileEntry::ClmOffBoundary { payload, .. } => Some(payload.mode_source),
                _ => None,
            })
            .flatten();
        let mode_source = match source {
            Some(ClmModeSource::ConfiguredDefault) => ClmStatusSource::ConfiguredDefault,
            Some(ClmModeSource::InheritedChild) => ClmStatusSource::InheritedChild,
            Some(ClmModeSource::SessionOverride) => ClmStatusSource::SessionOverride,
            None => ClmStatusSource::Unknown,
        };
        let recent_revisions = view
            .branch
            .iter()
            .rev()
            .filter_map(boundary)
            .filter(|b| Some(&b.epoch_id) == accepted.epoch_id.as_ref())
            .take(10)
            .map(|b| ClmRevisionRef {
                epoch_id: b.epoch_id.clone(),
                revision: b.revision,
            })
            .collect();
        let state = self.lock()?;
        let warning = state
            .estimate
            .as_ref()
            .filter(|e| {
                e.max_input_tokens.is_some_and(|max| {
                    u128::from(e.estimated_input_tokens) * 4 >= u128::from(max) * 3
                })
            })
            .map(|_| "CLM context is near its request budget; shorten stale history".into());
        Ok(ClmStatus {
            mode_source,
            recent_revisions,
            configured_default,
            mode: accepted.mode,
            epoch_id: accepted.epoch_id,
            revision: accepted.revision,
            path: self.path.to_string_lossy().into_owned(),
            estimated_occupancy: state.estimate.clone(),
            warning,
            error: state.error.clone(),
            measured_usage: get_clm_measured_usage(&view.branch)?,
        })
    }
}

fn journal_error(error: anyhow::Error) -> ClmError {
    error
        .downcast::<ClmError>()
        .unwrap_or_else(|error| ClmError::Persistence(error.to_string()))
}

fn pending_base() -> EntryBase {
    EntryBase {
        id: None,
        parent_id: None,
        timestamp: None,
        rest: serde_json::Map::new(),
    }
}
