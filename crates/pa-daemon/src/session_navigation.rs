//! The worker arms for `new_session`, `switch_session`, and `import_jsonl`:
//! all three replace the live session with another file, prepared and
//! validated BEFORE the teardown (a failed prepare leaves the old session
//! untouched); tree moves are NOT replacements — the kernel stays warm.

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::engine::SessionEngine;
use crate::protocol::{response_failure, response_success, DaemonErrorInfo, DaemonResponse};
use crate::session_store::{session_file_name, SessionFile};
use crate::worker::{SessionCore, Worker};

/// A prepared replacement session: the opened file, plus the session cwd the
/// replacement rebinds onto (override, else stored header); `None` keeps the live cwd.
pub(crate) struct PreparedReplacement {
    pub(crate) file: SessionFile,
    pub(crate) cwd: Option<String>,
    // A successful fresh write owes scheduling only after publication.
    trace_persisted: bool,
}

/// The navigation surface: the prepare and swap phases of the replacement
/// flow the three commands share.
#[derive(Clone)]
pub(crate) struct SessionNavigation {
    engine: Arc<dyn SessionEngine>,
    core: Arc<Mutex<SessionCore>>,
}

impl SessionNavigation {
    pub(crate) fn new(engine: Arc<dyn SessionEngine>, core: Arc<Mutex<SessionCore>>) -> Self {
        SessionNavigation { engine, core }
    }

    /// Swap the live session onto `file` (store, engine session file, rebuilt
    /// context). The caller retires the previous runtime and rebinds the cwd
    /// first, so the context park lands on the fresh session.
    async fn replace_session(
        &self,
        target: PreparedReplacement,
        clm_default_eligible: bool,
    ) -> Result<(), String> {
        let PreparedReplacement {
            mut file,
            trace_persisted,
            ..
        } = target;
        let branch_entries = file.branch_file_entries();
        let new_path = file.path.clone();
        // Prime the new store's usage fold before it enters the core: the
        // resume summaries read from this cache and fold only the appended tail.
        let primed = new_path.clone();
        let _ =
            tokio::task::spawn_blocking(move || crate::session_store::read_session_info(&primed))
                .await;
        let (previous, traces) = {
            let mut core = self.core.lock().unwrap();
            if file.trace_upload.is_none() {
                file.trace_upload = core
                    .store
                    .as_ref()
                    .and_then(|old| old.trace_upload.as_ref())
                    .and_then(|traces| traces.rebind(std::path::Path::new(&core.cwd), &file.path));
            }
            let traces = file.trace_upload.clone();
            // No await or fallible preparation between slot transfer and publication.
            (core.store.replace(file), traces)
        };
        if let Some(traces) = traces.filter(|_| trace_persisted) {
            // The successful prepare wrote this file before its controller existed.
            // Record bounded intent outside the core lock, after publication.
            traces.persisted(&new_path);
        }
        // The old store's lease release flushes the window and info
        // sidecars (megabytes for a large session): off the core lock
        // and the runtime.
        let _ = tokio::task::spawn_blocking(move || drop(previous)).await;
        self.engine.set_session_file(new_path.clone());
        self.engine
            .configure_clm_default_eligibility(clm_default_eligible);
        // The replacement session resolves to the model its own file pins,
        // not the previous session's (an explicit flag still wins).
        self.engine.restore_session_model(&new_path, None).await;
        // The replacement retired the runtime, so the rebuild parks on the
        // fresh session and seeds the goal state from the moved branch's rows.
        rebuild_engine_context(
            &self.engine,
            branch_entries,
            pa_core::session_engine::goal_driver::GoalBranchReload::FaithfulBranch,
        )
        .await
    }

    fn target_lease(
        &self,
        path: &std::path::Path,
    ) -> anyhow::Result<Option<Arc<crate::lease::SessionLease>>> {
        let source = self
            .core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .and_then(|store| store.lease.clone());
        match source {
            Some(source) if source.session_path == crate::lease::canonical_session_path(path) => {
                Ok(Some(source))
            }
            Some(source) => source.acquire_target(path).map(Some),
            None => Ok(None),
        }
    }

    /// `new_session`'s prepare phase: a fresh session in the same directory,
    /// optionally parented on `parentSession`; a failure never touches the live session.
    #[allow(clippy::result_large_err)]
    pub(crate) fn prepare_new_session(
        &self,
        payload: &Value,
    ) -> Result<PreparedReplacement, DaemonResponse> {
        let parent_session = payload
            .get("parentSession")
            .and_then(Value::as_str)
            .map(str::to_string);
        let (cwd, session_dir, rlm_depth) = {
            let core = self.core.lock().unwrap();
            match core.store.as_ref() {
                Some(store) => (
                    core.cwd.clone(),
                    store.path.parent().map(std::path::Path::to_path_buf),
                    store.header.rlm_depth.unwrap_or(0) as u32,
                ),
                None => {
                    return Err(response_failure(
                        None,
                        "new_session",
                        "Session is still initializing",
                        None,
                    ))
                }
            }
        };
        let mut fresh = SessionFile::create(&cwd, parent_session.as_deref(), rlm_depth);
        if let Some(session_dir) = session_dir {
            fresh.set_path(session_dir.join(session_file_name(fresh.session_id())));
            fresh.lease = self
                .target_lease(&fresh.path)
                .map_err(|error| response_failure(None, "new_session", &error.to_string(), None))?;
            if let Err(error) = fresh.rewrite() {
                return Err(response_failure(
                    None,
                    "new_session",
                    &error.to_string(),
                    None,
                ));
            }
        }
        // TS `newSession` keeps the runtime's cwd: the fresh session runs where the live one did.
        let trace_persisted = !fresh.path.as_os_str().is_empty();
        Ok(PreparedReplacement {
            file: fresh,
            cwd: None,
            trace_persisted,
        })
    }

    /// `switch_session`'s prepare phase: a missing file or a gone cwd fails
    /// here, before the teardown, so the live session keeps its kernel.
    #[allow(clippy::result_large_err)]
    pub(crate) fn prepare_switch_session(
        &self,
        payload: &Value,
    ) -> Result<PreparedReplacement, DaemonResponse> {
        let session_path = payload
            .get("sessionPath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cwd_override = payload
            .get("cwdOverride")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.open_replacement(session_path, cwd_override, "switch_session", None)
    }

    /// `import_jsonl`'s prepare phase: copy the input file into the session
    /// dir and open the copy; a missing input answers the TS import error.
    #[allow(clippy::result_large_err)]
    pub(crate) fn prepare_import_jsonl(
        &self,
        payload: &Value,
    ) -> Result<PreparedReplacement, DaemonResponse> {
        let input_path = payload
            .get("inputPath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cwd_override = payload
            .get("cwdOverride")
            .and_then(Value::as_str)
            .map(str::to_string);
        let resolved = std::path::Path::new(input_path);
        if !resolved.is_file() {
            return Err(response_failure(
                None,
                "import_jsonl",
                &format!("File not found: {}", resolved.display()),
                Some(DaemonErrorInfo::SessionImportFileNotFound {
                    file_path: resolved.display().to_string(),
                }),
            ));
        }
        // The destination is the session dir's copy; an in-place import skips the copy.
        let destination = {
            let core = self.core.lock().unwrap();
            core.store
                .as_ref()
                .and_then(|store| store.path.parent().map(std::path::Path::to_path_buf))
        };
        let source_bytes = std::fs::read(resolved)
            .map_err(|error| response_failure(None, "import_jsonl", &error.to_string(), None))?;
        if pa_core::session::clm::contains_clm_records(&source_bytes) {
            let prepare = || -> anyhow::Result<PreparedReplacement> {
                let validated = pa_core::clm::validate_clm_import(&source_bytes)?
                    .ok_or_else(|| anyhow::anyhow!("CLM import has no accepted boundary"))?;
                // Validate every selected path, including suffixes after a boundary.
                for entry in validated
                    .entries
                    .iter()
                    .filter(|entry| entry.id().is_some())
                {
                    let branch =
                        pa_core::session::clm::selected_branch(&validated.entries, entry.id())?;
                    pa_core::clm::validate_clm_branch(&branch)?;
                }
                let dir = destination
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Session is still initializing"))?;
                let mut entries = validated.entries;
                let Some(pa_types::session::FileEntry::Header { header }) = entries.first_mut()
                else {
                    anyhow::bail!("CLM import has no header");
                };
                let cwd = cwd_override.clone().unwrap_or_else(|| header.cwd.clone());
                anyhow::ensure!(
                    std::path::Path::new(&cwd).is_dir(),
                    "Stored session working directory does not exist: {cwd}"
                );
                let id = uuid::Uuid::new_v4().to_string();
                header.id.clone_from(&id);
                header.cwd.clone_from(&cwd);
                header.parent_session = Some(resolved.to_string_lossy().into_owned());
                let target = dir.join(session_file_name(&id));
                let artifact = dir
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("session root missing"))?
                    .join("session-artifacts")
                    .join(&id);
                anyhow::ensure!(!artifact.exists(), "CLM import artifact collision");
                // Reserve with create_new: never replace source or another destination.
                let reserved = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)?;
                drop(reserved);
                let mut created_artifact = false;
                let result = (|| -> anyhow::Result<PreparedReplacement> {
                    let lease = self.target_lease(&target)?;
                    let first_end = source_bytes
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .ok_or_else(|| anyhow::anyhow!("CLM source header is unterminated"))?;
                    let mut copied = serde_json::to_vec(&entries[0])?;
                    copied.push(b'\n');
                    copied.extend_from_slice(&source_bytes[first_end + 1..]);
                    pa_core::clm::validate_clm_import(&copied)?;
                    let mut output = std::fs::OpenOptions::new().write(true).open(&target)?;
                    output.write_all(&copied)?;
                    output.sync_all()?;
                    std::fs::File::open(dir)?.sync_all()?;
                    anyhow::ensure!(
                        std::fs::read(&target)? == copied,
                        "CLM import bytes could not be verified"
                    );
                    let mut file = SessionFile::open(&target)?;
                    file.lease = lease;
                    let branch = file.branch_file_entries_checked()?;
                    let Some(projection) = pa_core::clm::project_clm_branch(&branch)? else {
                        return Ok(PreparedReplacement {
                            file,
                            cwd: Some(cwd),
                            trace_persisted: true,
                        });
                    };
                    let snapshot = pa_types::clm::ClmSnapshot::from_blocks(projection.blocks);
                    let text = pa_core::clm::render_context_document(
                        &snapshot,
                        &std::collections::HashSet::new(),
                    )?;
                    std::fs::create_dir_all(
                        artifact
                            .parent()
                            .ok_or_else(|| anyhow::anyhow!("artifact root missing"))?,
                    )?;
                    #[cfg(test)]
                    clm_import_tests::inject_artifact_collision(&artifact);
                    std::fs::create_dir(&artifact)?;
                    created_artifact = true;
                    let mirror_dir = artifact.join("clm");
                    std::fs::create_dir(&mirror_dir)?;
                    let mut mirror = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(mirror_dir.join("context.txt"))?;
                    mirror.write_all(text.as_bytes())?;
                    mirror.sync_all()?;
                    std::fs::File::open(&mirror_dir)?.sync_all()?;
                    std::fs::File::open(&artifact)?.sync_all()?;
                    Ok(PreparedReplacement {
                        file,
                        cwd: Some(cwd),
                        trace_persisted: true,
                    })
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&target);
                    if created_artifact {
                        let _ = std::fs::remove_dir_all(&artifact);
                    }
                }
                result
            };
            return prepare()
                .map_err(|error| response_failure(None, "import_jsonl", &error.to_string(), None));
        }
        let target = match destination {
            Some(dir) => dir.join(resolved.file_name().map_or_else(
                || session_file_name("imported"),
                |name| name.to_string_lossy().to_string(),
            )),
            None => {
                return Err(response_failure(
                    None,
                    "import_jsonl",
                    "Session is still initializing",
                    None,
                ))
            }
        };
        std::fs::create_dir_all(target.parent().unwrap_or(std::path::Path::new(".")))
            .map_err(|error| error.to_string())
            .ok();
        let lease = self
            .target_lease(&target)
            .map_err(|error| response_failure(None, "import_jsonl", &error.to_string(), None))?;
        if std::fs::canonicalize(&target).ok() != std::fs::canonicalize(resolved).ok() {
            if let Err(error) = std::fs::copy(resolved, &target) {
                return Err(response_failure(
                    None,
                    "import_jsonl",
                    &error.to_string(),
                    None,
                ));
            }
        }
        self.open_replacement(
            &target.to_string_lossy(),
            cwd_override,
            "import_jsonl",
            lease,
        )
    }

    /// Open one replacement session file and check its stored cwd exists:
    /// the `MissingSessionCwdError` text is TS-verbatim.
    #[allow(clippy::result_large_err)]
    fn open_replacement(
        &self,
        path: &str,
        cwd_override: Option<String>,
        command: &'static str,
        lease: Option<Arc<crate::lease::SessionLease>>,
    ) -> Result<PreparedReplacement, DaemonResponse> {
        let lease = match lease {
            Some(lease) => Some(lease),
            None => self
                .target_lease(std::path::Path::new(path))
                .map_err(|error| response_failure(None, command, &error.to_string(), None))?,
        };
        if lease.is_some() {
            pa_core::session::manager::repair_jsonl_damage(std::path::Path::new(path));
        }
        let mut file = SessionFile::open(std::path::Path::new(path))
            .map_err(|error| response_failure(None, command, &error.to_string(), None))?;
        file.lease = lease;
        let cwd =
            cwd_override.or_else(|| (!file.header.cwd.is_empty()).then(|| file.header.cwd.clone()));
        if let Some(cwd) = cwd.as_deref() {
            if !std::path::Path::new(cwd).is_dir() {
                let fallback = {
                    let core = self.core.lock().unwrap();
                    core.cwd.clone()
                };
                // The typed error info lets clients render the TS missing-cwd prompt (the issue
                // carries the fallback cwd).
                return Err(response_failure(
                    None,
                    command,
                    &format!(
                        "Stored session working directory does not exist: {cwd}\nSession file: {path}\nCurrent working directory: {fallback}"
                    ),
                    Some(DaemonErrorInfo::MissingSessionCwd {
                        issue: json!({
                            "sessionFile": path,
                            "sessionCwd": cwd,
                            "fallbackCwd": fallback,
                        }),
                    }),
                ));
            }
        }
        Ok(PreparedReplacement {
            file,
            cwd,
            trace_persisted: false,
        })
    }
}

/// Rebuild the engine's live context onto the moved session (the same
/// helper `branch_navigation` runs for forks).
async fn rebuild_engine_context(
    engine: &Arc<dyn SessionEngine>,
    branch_entries: Vec<pa_types::session::FileEntry>,
    goal_reload: pa_core::session_engine::goal_driver::GoalBranchReload,
) -> Result<(), String> {
    let engine = Arc::clone(engine);
    tokio::task::spawn_blocking(move || engine.rebuild_session_context(branch_entries, goal_reload))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("{error:#}"))
}

impl Worker {
    /// The shared replacement flow: prepare, retire the runtime, rebind the
    /// cwd (so the fresh kernel spawns in the moved-to cwd), swap the store,
    /// rebuild the context, refresh the derived state.
    async fn run_session_replacement(
        &self,
        command: &'static str,
        prepared: Result<PreparedReplacement, DaemonResponse>,
    ) -> DaemonResponse {
        let target = match prepared {
            Ok(target) => target,
            // A prepare failure never tore anything down: the live session is untouched.
            Err(response) => return response,
        };
        // One replacement at a time: the teardown, swap, restore, and rebuild
        // are one critical section against a concurrent replacement.
        let _replacement_gate = self.replacement_gate.lock().await;
        // A failed retire fails the command with the old runtime already torn down.
        if let Err(error) = self.teardown_for_replacement().await {
            return response_failure(None, command, &format!("{error:#}"), None);
        }
        if let Some(cwd) = target.cwd.as_deref() {
            self.rebind_worker_cwd(cwd);
        }
        match self
            .navigation
            .replace_session(target, command == "new_session")
            .await
        {
            Ok(()) => {
                self.refresh_replaced_session_state().await;
                self.reseed_service_tier_for_replacement();
                if let Err(error) = self.bind_scheduled_jobs().await {
                    return response_failure(None, command, &error.to_string(), None);
                }
                self.prewarm_replacement_session();
                // The replacement never pushed a roster delta, so the subscribed
                // surfaces kept the PREVIOUS session's numbers.
                self.push_roster_delta();
                // The pane reporter re-reports for the successor session
                // (the TS replacement arm: the old instance went silent at
                // the teardown, the successor force-publishes with its own
                // session reference immediately — same pane, new session).
                let (active, session_ref) = {
                    let core = self.core.lock().unwrap();
                    (core.busy, Worker::herdr_session_ref(&core))
                };
                self.herdr
                    .lock()
                    .unwrap()
                    .session_started(active, session_ref);
                response_success(None, command, Some(json!({ "cancelled": false })))
            }
            Err(error) => {
                // A failed replacement tore the old session down without
                // installing the successor: the reporter goes silent (the
                // TS `session_shutdown` non-quit arm — never release, the
                // pane is not the worker's to free here).
                *self.herdr.lock().unwrap() = crate::herdr::HerdrReporter::default();
                response_failure(None, command, &error, None)
            }
        }
    }

    /// `new_session`.
    pub(crate) async fn handle_new_session(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("new_session") {
            return response;
        }
        let prepared = self.navigation.prepare_new_session(payload);
        self.run_session_replacement("new_session", prepared).await
    }

    /// `switch_session`.
    pub(crate) async fn handle_switch_session(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("switch_session") {
            return response;
        }
        let navigation = self.navigation.clone();
        let payload = payload.clone();
        // The closure boxes its error (clippy::result_large_err): the
        // DaemonResponse unwraps in the match arms, not inside any closure.
        let prepared = match tokio::task::spawn_blocking(move || {
            navigation
                .prepare_switch_session(&payload)
                .map_err(Box::new)
        })
        .await
        {
            Ok(Ok(prepared)) => Ok(prepared),
            Ok(Err(boxed)) => Err(*boxed),
            Err(error) => Err(response_failure(
                None,
                "switch_session",
                &error.to_string(),
                None,
            )),
        };
        self.run_session_replacement("switch_session", prepared)
            .await
    }

    /// `import_jsonl`.
    pub(crate) async fn handle_import_jsonl(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("import_jsonl") {
            return response;
        }
        let navigation = self.navigation.clone();
        let payload = payload.clone();
        let prepared = match tokio::task::spawn_blocking(move || {
            navigation.prepare_import_jsonl(&payload).map_err(Box::new)
        })
        .await
        {
            Ok(Ok(prepared)) => Ok(prepared),
            Ok(Err(boxed)) => Err(*boxed),
            Err(error) => Err(response_failure(
                None,
                "import_jsonl",
                &error.to_string(),
                None,
            )),
        };
        self.run_session_replacement("import_jsonl", prepared).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    async fn created_worker() -> Arc<Worker> {
        let dir = std::env::temp_dir().join(format!("pa-worker-nav-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = crate::worker::WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "nav-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "nav" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        worker
    }

    #[tokio::test]
    async fn new_session_answers_the_ts_cancelled_shape() {
        let worker = created_worker().await;
        let response = worker
            .dispatch("new_session", &json!({ "activeSessionId": "nav-session" }))
            .await;
        assert_eq!(response.data, Some(json!({ "cancelled": false })));
        let stats = worker
            .dispatch(
                "get_session_stats",
                &json!({ "activeSessionId": "nav-session" }),
            )
            .await;
        assert!(stats.success, "{stats:?}");
    }

    #[tokio::test]
    async fn switch_session_replaces_the_store_and_fails_on_missing_files() {
        let worker = created_worker().await;
        let response = worker
            .dispatch(
                "switch_session",
                &json!({ "activeSessionId": "nav-session", "sessionPath": "/tmp/definitely-missing.jsonl" }),
            )
            .await;
        assert!(!response.success, "{response:?}");
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("/tmp/definitely-missing.jsonl"),
            "{response:?}"
        );
    }

    /// A tree move is not a replacement: the built session stays warm.
    // The faux provider registration is global; the lock must span the
    // awaited turns that consume its queue.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn replacement_flows_retire_only_on_a_prepared_file() {
        let _faux = crate::agent_engine::FAUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::TempDir::new().expect("temp dir");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let config = crate::worker::WorkerConfig {
            socket_path: dir.path().join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "replacement-session".to_string(),
            agent_dir: dir.path().join("agent"),
            recovery_journal_path: dir.path().join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({
                "engine": "faux",
                "responses": [{ "text": "one" }, { "text": "two" }],
            })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({
                    "cwd": dir.path().to_string_lossy(),
                    "sessionDir": sessions_dir.to_string_lossy(),
                }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        let session_id = "replacement-session".to_string();
        let prompted = worker
            .dispatch(
                "prompt_and_wait",
                &json!({ "activeSessionId": session_id, "message": "hello" }),
            )
            .await;
        assert!(prompted.success, "prompt failed: {prompted:?}");
        let engine = worker
            .agent_engine
            .as_ref()
            .expect("faux script drives the real engine")
            .clone();
        let session_built = || {
            let engine = std::sync::Arc::clone(&engine);
            tokio::task::spawn_blocking(move || engine.session.blocking_lock().is_some())
        };
        assert!(
            session_built().await.expect("built join"),
            "the turn built the session"
        );

        // A failed switch prepare leaves the live session untouched: no teardown ran.
        let failed = worker
            .dispatch(
                "switch_session",
                &json!({
                    "activeSessionId": session_id,
                    "sessionPath": "/tmp/definitely-missing-replacement.jsonl",
                }),
            )
            .await;
        assert!(
            !failed.success,
            "missing switch target succeeded: {failed:?}"
        );
        assert!(
            session_built().await.expect("built join"),
            "a failed prepare must not retire the session"
        );

        // A tree move is not a replacement: the built session stays warm.
        let tree = worker
            .dispatch(
                "get_session_tree",
                &json!({ "activeSessionId": session_id }),
            )
            .await;
        assert!(tree.success, "tree failed: {tree:?}");
        assert!(
            session_built().await.expect("built join"),
            "a tree move must keep the session warm"
        );

        // A successful replacement retires the built session and rebuilds it in the background.
        let replaced = worker
            .dispatch("new_session", &json!({ "activeSessionId": session_id }))
            .await;
        assert_eq!(
            replaced.data,
            Some(json!({ "cancelled": false })),
            "new_session failed: {replaced:?}"
        );
        let second = worker
            .dispatch(
                "prompt_and_wait",
                &json!({ "activeSessionId": session_id, "message": "again" }),
            )
            .await;
        assert!(
            second.success,
            "the replacement session's turn failed: {second:?}"
        );
        assert!(
            session_built().await.expect("built join"),
            "the replacement session rebuilt"
        );

        // The worker (and its engine's private runtime) must drop off the
        // async context.
        let worker_for_drop = worker;
        drop(engine);
        tokio::task::spawn_blocking(move || drop(worker_for_drop))
            .await
            .expect("worker drop join");
    }

    #[tokio::test]
    async fn switch_session_rebinds_the_worker_cwd_and_new_session_keeps_it() {
        let worker = created_worker().await;
        let state = worker
            .dispatch("get_state", &json!({ "activeSessionId": "nav-session" }))
            .await;
        assert!(state.success, "{state:?}");
        assert_eq!(state.data.as_ref().unwrap()["cwd"], "/tmp");

        // A target session file recording another existing cwd.
        let target_cwd = tempfile::TempDir::new().expect("target cwd");
        let target = target_cwd.path().join("switch-target.jsonl");
        std::fs::write(
            &target,
            format!(
                "{}\n",
                json!({
                    "type": "session",
                    "id": "switch-target",
                    "timestamp": "2026-09-21T00:00:00.000Z",
                    "cwd": target_cwd.path().to_string_lossy(),
                })
            ),
        )
        .expect("write switch target");
        let switched = worker
            .dispatch(
                "switch_session",
                &json!({
                    "activeSessionId": "nav-session",
                    "sessionPath": target.to_string_lossy(),
                }),
            )
            .await;
        assert!(switched.success, "{switched:?}");
        let state = worker
            .dispatch("get_state", &json!({ "activeSessionId": "nav-session" }))
            .await;
        let target_cwd = target_cwd.path().to_string_lossy().to_string();
        assert_eq!(state.data.as_ref().unwrap()["cwd"], target_cwd);

        // A target whose stored cwd no longer exists fails at the prepare
        // (TS `MissingSessionCwdError`); the live session keeps its rebound cwd.
        let gone_dir = tempfile::TempDir::new().expect("gone dir");
        let gone = gone_dir.path().join("gone-target.jsonl");
        std::fs::write(
            &gone,
            format!(
                "{}\n",
                json!({
                    "type": "session",
                    "id": "gone-target",
                    "timestamp": "2026-09-21T00:00:00.000Z",
                    "cwd": gone_dir.path().join("missing-cwd"),
                })
            ),
        )
        .expect("write gone target");
        let failed = worker
            .dispatch(
                "switch_session",
                &json!({
                    "activeSessionId": "nav-session",
                    "sessionPath": gone.to_string_lossy(),
                }),
            )
            .await;
        assert!(!failed.success, "{failed:?}");
        assert!(
            failed
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Stored session working directory does not exist"),
            "{failed:?}"
        );
        let state = worker
            .dispatch("get_state", &json!({ "activeSessionId": "nav-session" }))
            .await;
        assert_eq!(state.data.as_ref().unwrap()["cwd"], target_cwd);

        let fresh = worker
            .dispatch("new_session", &json!({ "activeSessionId": "nav-session" }))
            .await;
        assert!(fresh.success, "{fresh:?}");
        let state = worker
            .dispatch("get_state", &json!({ "activeSessionId": "nav-session" }))
            .await;
        assert_eq!(state.data.as_ref().unwrap()["cwd"], target_cwd);
    }

    #[tokio::test]
    async fn fork_rebinds_the_scheduled_jobs_onto_the_forked_session() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let config = crate::worker::WorkerConfig {
            socket_path: dir.path().join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "fork-schedule-session".to_string(),
            agent_dir: dir.path().join("agent"),
            recovery_journal_path: dir.path().join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({
                    "cwd": dir.path().to_string_lossy(),
                    "sessionDir": sessions_dir.to_string_lossy(),
                }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        // One user message so the fork has a branch point.
        let entry_id = {
            let mut core = worker.core.lock().unwrap();
            let store = core.store.as_mut().expect("created store");
            let entry_id = store
                .append_message(&json!({ "role": "user", "content": "hi", "timestamp": 1u64 }));
            let _ = store.rewrite();
            entry_id
        };
        let added = worker
            .dispatch(
                "cron_add",
                &json!({
                    "activeSessionId": "fork-schedule-session",
                    "schedule": "in 10m",
                    "prompt": "run me",
                }),
            )
            .await;
        assert!(added.success, "cron_add failed: {added:?}");
        let source_file = {
            let core = worker.core.lock().unwrap();
            core.store
                .as_ref()
                .expect("store")
                .path
                .to_string_lossy()
                .to_string()
        };

        let forked = worker
            .dispatch(
                "fork",
                &json!({
                    "activeSessionId": "fork-schedule-session",
                    "entryId": entry_id,
                    "position": "at",
                }),
            )
            .await;
        assert!(forked.success, "fork failed: {forked:?}");

        let (forked_file, forked_id, forked_cwd) = {
            let core = worker.core.lock().unwrap();
            let store = core.store.as_ref().expect("forked store");
            (
                store.path.to_string_lossy().to_string(),
                store.session_id().to_string(),
                core.cwd.clone(),
            )
        };
        assert_ne!(forked_file, source_file, "fork did not move the store");
        let jobs = worker.scheduled.store().list();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].session_file, forked_file, "{jobs:?}");
        assert_eq!(jobs[0].session_id, forked_id, "{jobs:?}");
        assert_eq!(jobs[0].active_session_id, "fork-schedule-session");
        assert_eq!(jobs[0].cwd, forked_cwd, "{jobs:?}");
    }

    #[tokio::test]
    async fn import_jsonl_answers_the_ts_file_not_found_error() {
        let worker = created_worker().await;
        let response = worker
            .dispatch(
                "import_jsonl",
                &json!({ "activeSessionId": "nav-session", "inputPath": "/tmp/no-such-import.jsonl" }),
            )
            .await;
        assert!(!response.success, "{response:?}");
        assert_eq!(
            response.error.as_deref(),
            Some("File not found: /tmp/no-such-import.jsonl")
        );
        // The typed error info lets the client render the TS import error surface.
        assert_eq!(
            response.error_info,
            Some(DaemonErrorInfo::SessionImportFileNotFound {
                file_path: "/tmp/no-such-import.jsonl".to_string()
            })
        );
    }

    /// The typed issue carries the fallback cwd the client's confirm answers with.
    #[tokio::test]
    async fn import_jsonl_answers_the_ts_missing_cwd_error_info() {
        let dir = std::env::temp_dir().join(format!("pa-import-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // The import copies its input into the live session's directory, so the worker starts on a
        // real file.
        let live = dir.join("live-session.jsonl");
        let mut live_file = SessionFile::create("/tmp", None, 0);
        live_file.set_path(live.clone());
        live_file.rewrite().unwrap();
        let config = crate::worker::WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "nav-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(crate::worker::Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({ "cwd": "/tmp", "name": "nav", "sessionPath": live.to_string_lossy() }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        let gone = dir.join("gone-session.jsonl");
        let gone_cwd = dir.join("gone-cwd");
        // A well-formed session file whose stored cwd no longer exists.
        let mut file = SessionFile::create(&gone_cwd.to_string_lossy(), None, 0);
        file.set_path(gone.clone());
        file.rewrite().unwrap();
        let response = worker
            .dispatch(
                "import_jsonl",
                &json!({
                    "activeSessionId": "nav-session",
                    "inputPath": gone.to_string_lossy(),
                }),
            )
            .await;
        assert!(!response.success, "{response:?}");
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .starts_with("Stored session working directory does not exist:"),
            "the TS-verbatim error text, got {response:?}"
        );
        match response.error_info {
            Some(DaemonErrorInfo::MissingSessionCwd { issue }) => {
                assert_eq!(issue["sessionCwd"], json!(gone_cwd.to_string_lossy()));
                assert_eq!(issue["fallbackCwd"], json!("/tmp"));
            }
            other => panic!("expected the typed missing-cwd error info, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod trace_replacement_tests {
    use super::*;

    #[test]
    fn failed_new_session_prepare_preserves_the_live_trace_registration() {
        let _env = crate::trace_test_env::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-directory");
        std::fs::write(&blocked, "synthetic blocker").unwrap();
        let old_path = blocked.join("old.jsonl");
        let agent_dir = dir.path().join("agent");
        let (_, consent) =
            pa_core::agent_traces::ContinuousTraceUpload::load_settings(dir.path(), &agent_dir);
        let controller = pa_core::agent_traces::ContinuousTraceUpload::install(
            dir.path(),
            &agent_dir,
            Some(&old_path),
            consent,
        )
        .unwrap();
        let mut store = SessionFile::create(dir.path().to_str().unwrap(), None, 0);
        store.set_path(old_path.clone());
        store.trace_upload = Some(controller.clone());
        let core = Arc::new(Mutex::new(SessionCore::test_core(
            Some(store),
            dir.path().to_string_lossy().into_owned(),
        )));
        let navigation = SessionNavigation::new(
            Arc::new(crate::engine::ScriptedEngine::default()),
            core.clone(),
        );
        assert!(navigation.prepare_new_session(&json!({})).is_err());
        let live = core.lock().unwrap();
        assert_eq!(live.store.as_ref().unwrap().path, old_path);
        assert!(Arc::ptr_eq(
            live.store.as_ref().unwrap().trace_upload.as_ref().unwrap(),
            &controller
        ));
        drop(live);
        // A cancelled predecessor cannot transfer its registry slot again.
        assert!(controller
            .rebind(dir.path(), &dir.path().join("probe.jsonl"))
            .is_some());
        assert!(!agent_dir.join("agent-traces-outbox").exists());
    }
}

#[cfg(test)]
mod clm_import_tests {
    thread_local! { static ARTIFACT_COLLISION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    pub(super) fn inject_artifact_collision(path: &std::path::Path) {
        if ARTIFACT_COLLISION.replace(false) {
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("foreign-owner"), "keep").unwrap();
        }
    }
    #[test]
    fn import_jsonl_does_not_clean_up_an_artifact_created_by_another_owner() {
        let dir = tempfile::tempdir().unwrap();
        let (navigation, sessions) = fixture(dir.path());
        let input = dir.path().join("source.jsonl");
        let original = source(&input);
        let before = std::fs::read_dir(&sessions).unwrap().count();
        ARTIFACT_COLLISION.set(true);
        assert!(navigation
            .prepare_import_jsonl(&serde_json::json!({"inputPath":input}))
            .is_err());
        let artifact_root = dir.path().join("session-artifacts");
        let artifacts = std::fs::read_dir(artifact_root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            std::fs::read_to_string(artifacts[0].join("foreign-owner")).unwrap(),
            "keep"
        );
        assert_eq!(std::fs::read_dir(sessions).unwrap().count(), before);
        assert_eq!(std::fs::read(input).unwrap(), original);
    }

    use super::*;
    fn fixture(dir: &std::path::Path) -> (SessionNavigation, std::path::PathBuf) {
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mut live = SessionFile::create("/tmp", None, 0);
        live.set_path(sessions.join("live.jsonl"));
        live.rewrite().unwrap();
        let core = Arc::new(Mutex::new(SessionCore::test_core(
            Some(live),
            "/tmp".into(),
        )));
        (
            SessionNavigation::new(Arc::new(crate::engine::ScriptedEngine::default()), core),
            sessions,
        )
    }
    fn source(path: &std::path::Path) -> Vec<u8> {
        let rows = [
            json!({"type":"session","version":3,"id":"source","cwd":"/tmp","timestamp":"now"}),
            json!({"type":"message","id":"u","parentId":null,"timestamp":"now","message":{"role":"user","content":"obsolete","timestamp":1}}),
            json!({"type":"clm_on","id":"on","parentId":"u","timestamp":"now","epochId":"e","revision":0,"auditWatermark":"u","modeSource":"inherited-child","snapshot":{"blocks":[{"id":"image","message":{"role":"user","content":[{"type":"text","text":"accepted"},{"type":"image","data":"YWJj","mimeType":"image/png"}],"timestamp":1}}]}}),
        ];
        let bytes = rows
            .iter()
            .map(|r| r.to_string() + "\n")
            .collect::<String>()
            .into_bytes();
        std::fs::write(path, &bytes).unwrap();
        bytes
    }
    #[test]
    fn import_jsonl_creates_independent_identity_and_accepted_mirror_only() {
        let dir = tempfile::tempdir().unwrap();
        let (navigation, sessions) = fixture(dir.path());
        let source_path = sessions.join("source.jsonl");
        let original = source(&source_path);
        let artifact = dir.path().join("session-artifacts/source/clm");
        std::fs::create_dir_all(&artifact).unwrap();
        std::fs::write(artifact.join("context.txt"), "pending source edits").unwrap();
        std::fs::write(
            source_path.with_extension("jsonl.clm-writer"),
            "live marker",
        )
        .unwrap();
        let target = navigation
            .prepare_import_jsonl(&json!({"inputPath":source_path}))
            .unwrap_or_else(|r| panic!("{r:?}"));
        assert_ne!(target.file.header.id, "source");
        assert_ne!(target.file.path, source_path);
        let copied = std::fs::read(&target.file.path).unwrap();
        assert_eq!(
            copied.splitn(2, |b| *b == b'\n').nth(1),
            original.splitn(2, |b| *b == b'\n').nth(1)
        );
        assert_eq!(std::fs::read(&source_path).unwrap(), original);
        assert_eq!(
            std::fs::read_to_string(artifact.join("context.txt")).unwrap(),
            "pending source edits"
        );
        let mirror = dir
            .path()
            .join("session-artifacts")
            .join(&target.file.header.id)
            .join("clm/context.txt");
        let text = std::fs::read_to_string(mirror).unwrap();
        assert!(text.contains("accepted"));
        assert!(!text.contains("pending"));
        assert!(!text.contains("obsolete"));
        assert!(!target.file.path.with_extension("jsonl.clm-writer").exists());
        assert_eq!(target.file.model_context_checked().unwrap().len(), 1);
        let second = navigation
            .prepare_import_jsonl(&json!({"inputPath":source_path}))
            .unwrap_or_else(|r| panic!("{r:?}"));
        assert_ne!(second.file.path, target.file.path);
        assert_ne!(second.file.header.id, target.file.header.id);
        assert_eq!(std::fs::read(&source_path).unwrap(), original);
    }
    #[test]
    fn import_jsonl_validates_unselected_clm_branch_before_copying() {
        let dir = tempfile::tempdir().unwrap();
        let (navigation, sessions) = fixture(dir.path());
        let input = dir.path().join("bad.jsonl");
        let mut bytes = source(&input);
        bytes.extend_from_slice(
            b"{\"type\":\"clm_revision\",\"id\":\"bad\",\"parentId\":\"missing\"}\n",
        );
        std::fs::write(&input, &bytes).unwrap();
        let before = std::fs::read_dir(&sessions).unwrap().count();
        assert!(navigation
            .prepare_import_jsonl(&json!({"inputPath":input}))
            .is_err());
        assert_eq!(std::fs::read_dir(&sessions).unwrap().count(), before);
        assert_eq!(std::fs::read(input).unwrap(), bytes);
    }
    #[test]
    fn import_jsonl_keeps_an_ordinary_selected_branch_without_adopting_source_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let (navigation, sessions) = fixture(dir.path());
        let input = dir.path().join("branch.jsonl");
        let mut bytes = source(&input);
        let off = json!({"type":"clm_off_boundary","id":"off","parentId":"on","timestamp":"now","epochId":"e","revision":1,"auditWatermark":"u","snapshot":{"blocks":[]}});
        bytes.extend_from_slice((off.to_string() + "\n").as_bytes());
        let sibling = json!({"type":"message","id":"sibling","parentId":"u","timestamp":"now","message":{"role":"user","content":"selected ordinary branch","timestamp":2}});
        bytes.extend_from_slice((sibling.to_string() + "\n").as_bytes());
        std::fs::write(&input, &bytes).unwrap();
        let target = navigation
            .prepare_import_jsonl(&json!({"inputPath":input}))
            .unwrap_or_else(|r| panic!("{r:?}"));
        assert_eq!(target.file.model_context_checked().unwrap().len(), 2);
        assert!(!sessions
            .parent()
            .unwrap()
            .join("session-artifacts")
            .join(&target.file.header.id)
            .join("clm/context.txt")
            .exists());
        assert_eq!(std::fs::read(input).unwrap(), bytes);
    }
    #[test]
    fn import_jsonl_preserves_revision_usage_and_frozen_off_context() {
        let dir = tempfile::tempdir().unwrap();
        let (navigation, _) = fixture(dir.path());
        let input = dir.path().join("revisions.jsonl");
        let mut bytes = source(&input);
        let snapshot = pa_types::clm::ClmSnapshot::from_value(
            json!({"blocks":[{"id":"note","message":{"role":"user","content":"edited accepted note","timestamp":2}}]}),
        );
        let revision = json!({"type":"clm_revision","id":"rev","parentId":"on","timestamp":"now","epochId":"e","revision":1,"baseRevision":0,"snapshot":snapshot,"auditWatermark":"u","observedActionId":"edit","checksum":pa_core::clm::snapshot_checksum(&snapshot).unwrap()});
        let usage = json!({"input":2,"output":3,"cacheRead":4,"cacheWrite":5,"totalTokens":14,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.25}});
        let pin = pa_types::clm::ClmUsagePin {
            mode: pa_types::clm::ClmMode::On,
            branch_id: Some("rev".into()),
            epoch_id: Some("e".into()),
            revision: Some(1),
            entry_id: Some("rev".into()),
        };
        let mut receipt = pa_core::clm::clm_usage_receipt(&pin, &usage, None);
        receipt["type"] = json!("custom");
        receipt["id"] = json!("receipt");
        receipt["parentId"] = json!("rev");
        receipt["timestamp"] = json!("now");
        bytes.extend_from_slice(
            (revision.to_string() + "\n" + &receipt.to_string() + "\n").as_bytes(),
        );
        std::fs::write(&input, &bytes).unwrap();
        let target = navigation
            .prepare_import_jsonl(&json!({"inputPath":input}))
            .unwrap_or_else(|r| panic!("{r:?}"));
        let measured = pa_core::clm::get_clm_measured_usage(
            &target.file.branch_file_entries_checked().unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            measured,
            pa_types::clm::ClmMeasuredUsage {
                input: 2.,
                output: 3.,
                cache_read: 4.,
                cache_write: 5.,
                cost: 0.25
            }
        );
        let off = json!({"type":"clm_off_boundary","id":"off","parentId":"receipt","timestamp":"now","epochId":"e","revision":2,"snapshot":snapshot,"auditWatermark":"u"});
        bytes.extend_from_slice((off.to_string() + "\n").as_bytes());
        std::fs::write(&input, &bytes).unwrap();
        let target = navigation
            .prepare_import_jsonl(&json!({"inputPath":input}))
            .unwrap_or_else(|r| panic!("{r:?}"));
        assert_eq!(
            target.file.model_context_checked().unwrap(),
            vec![json!({"role":"user","content":"edited accepted note","timestamp":2})]
        );
        assert_eq!(
            pa_core::clm::get_clm_measured_usage(
                &target.file.branch_file_entries_checked().unwrap()
            )
            .unwrap(),
            None
        );
        assert_eq!(std::fs::read(input).unwrap(), bytes);
    }
    #[test]
    fn imported_clm_reopen_preserves_destination_and_source_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (navigation, sessions) = fixture(dir.path());
        let input = dir.path().join("source.jsonl");
        let original = source(&input);
        let target = navigation
            .prepare_import_jsonl(&json!({"inputPath":input}))
            .unwrap_or_else(|r| panic!("{r:?}"));
        assert!(target.file.header.parent_session.is_some());
        assert_eq!(target.file.header.rlm_depth, None);
        let destination = target.file.path.clone();
        let copied = std::fs::read(&destination).unwrap();
        drop(target);
        let mut core =
            pa_core::session::manager::SessionManager::open(dir.path(), &sessions, &destination);
        assert_eq!(core.get_header().unwrap().rlm_depth, None);
        assert_eq!(std::fs::read(&destination).unwrap(), copied);
        core.clm_journal(
            pa_core::clm::journal::ClmJournalOp::ReadAfterEvents,
            pa_core::clm::ProtectedActions::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), copied);
        assert_eq!(std::fs::read(&input).unwrap(), original);
    }
    #[test]
    #[ignore = "required Task7 opt-in: serial 512 MiB owner/history/import proof; run --ignored --exact --nocapture --test-threads=1"]
    fn clm_session_snapshot_cap_real_owners_and_history() -> anyhow::Result<()> {
        use pa_core::clm::{
            journal::{ClmCommit, ClmJournalOp, ClmJournalReply, ClmJournalView},
            ClmError, ProtectedActions,
        };
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        use std::path::Path;

        fn fingerprint(path: &Path) -> anyhow::Result<(u64, Vec<u8>)> {
            let mut input = std::fs::File::open(path)?;
            let mut hash = Sha256::new();
            let mut buffer = vec![0u8; 64 * 1024].into_boxed_slice();
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            Ok((input.metadata()?.len(), hash.finalize().to_vec()))
        }
        fn padded_boundary(output: &mut impl Write, envelope: &str) -> anyhow::Result<()> {
            output.write_all(envelope.as_bytes())?;
            output.write_all(br#"{"blocks":[],"padding":""#)?;
            let padding = pa_core::clm::MAX_CUMULATIVE_SNAPSHOT_BYTES / 2
                - br#"{"blocks":[],"padding":""}"#.len();
            let chunk = vec![b'x'; 64 * 1024].into_boxed_slice();
            let mut remaining = padding;
            while remaining > 0 {
                let count = remaining.min(chunk.len());
                output.write_all(&chunk[..count])?;
                remaining -= count;
            }
            output.write_all(b"\"}}\n")?;
            Ok(())
        }
        fn off(view: ClmJournalView) -> ClmCommit {
            let state = pa_core::clm::validate_clm_branch(&view.branch).unwrap();
            let row = serde_json::from_value(json!({"type":"clm_off_boundary","epochId":state.epoch_id,"revision":state.revision.unwrap()+1,"auditWatermark":"u","snapshot":{"blocks":[]}})).unwrap();
            ClmCommit {
                expected: view,
                row,
                allow_child_usage_suffix: false,
            }
        }
        fn core_probe(label: &str, source: &Path, work: &Path) -> anyhow::Result<bool> {
            std::fs::copy(source, work)?;
            let before = fingerprint(work)?;
            let dir = work.parent().unwrap();
            let mut owner = pa_core::session::manager::SessionManager::open(dir, dir, work);
            if label == "core-sibling" {
                let pa_types::session::FileEntry::ClmOffBoundary { payload, .. } =
                    &owner.get_all_entries()[3]
                else {
                    panic!("off fixture");
                };
                pa_core::clm::validate_clm_snapshot_admission(
                    &owner.get_all_entries()[..3],
                    &payload.boundary.snapshot,
                )?;
                println!("actual_512MiB_equality_allowed=true");
            }
            let count = owner.get_all_entries().len();
            let leaf = owner.get_leaf_id().map(str::to_owned);
            let readable = owner.active_context_checked().is_ok();
            let current = owner.clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new());
            let rejected = if let Ok(ClmJournalReply::View(view)) = current {
                matches!(
                    owner.clm_journal(ClmJournalOp::Commit(off(view)), ProtectedActions::new()),
                    Err(ClmError::Budget(_))
                )
            } else {
                false
            };
            let unchanged = fingerprint(work)? == before
                && owner.get_all_entries().len() == count
                && owner.get_leaf_id() == leaf.as_deref();
            let usable = owner.active_context_checked().is_ok()
                && owner
                    .clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new())
                    .is_ok();
            println!("{label}: readable={readable} budget_rejected={rejected} unchanged={unchanged} usable={usable}");
            Ok(readable && rejected && unchanged && usable)
        }
        fn daemon_probe(label: &str, source: &Path, work: &Path) -> anyhow::Result<bool> {
            std::fs::copy(source, work)?;
            let before = fingerprint(work)?;
            let mut owner = SessionFile::open(work)?;
            let count = owner.entries().len();
            assert!(count >= 4, "native fixture rows must not be skipped");
            let leaf = owner.leaf_id().map(str::to_owned);
            let readable = owner.model_context_checked().is_ok();
            let current = owner.clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new());
            let rejected = if let Ok(ClmJournalReply::View(view)) = current {
                let result =
                    owner.clm_journal(ClmJournalOp::Commit(off(view)), ProtectedActions::new());
                if let Err(error) = &result {
                    println!("{label}: commit_error={error}");
                }
                result.is_err_and(|e| {
                    matches!(e.downcast_ref::<ClmError>(), Some(ClmError::Budget(_)))
                })
            } else {
                false
            };
            let unchanged = fingerprint(work)? == before
                && owner.entries().len() == count
                && owner.leaf_id() == leaf.as_deref();
            let usable = owner.model_context_checked().is_ok()
                && owner
                    .clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new())
                    .is_ok();
            println!("{label}: readable={readable} budget_rejected={rejected} unchanged={unchanged} usable={usable}");
            Ok(readable && rejected && unchanged && usable)
        }
        let temporary = tempfile::tempdir()?;
        // An explicit artifact directory retains this generated fixture for the manual exercise.
        let root = std::env::var_os("PA_CLM_CAP_PROOF_DIR")
            .map_or_else(|| temporary.path().to_owned(), std::path::PathBuf::from);
        std::fs::create_dir_all(&root)?;
        let source = root.join("cap-history.jsonl");
        let work = root.join("owner-probe.jsonl");
        let mut output = std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&source)?,
        );
        writeln!(
            output,
            "{}",
            json!({"type":"session","version":3,"id":"cap-source","cwd":"/tmp","timestamp":"now"})
        )?;
        writeln!(
            output,
            "{}",
            json!({"type":"message","id":"u","parentId":null,"timestamp":"now","message":{"role":"user","content":"covered audit","timestamp":1}})
        )?;
        padded_boundary(
            &mut output,
            r#"{"type":"clm_on","id":"a","parentId":"u","timestamp":"now","epochId":"old","revision":0,"auditWatermark":"u","snapshot":"#,
        )?;
        padded_boundary(
            &mut output,
            r#"{"type":"clm_off_boundary","id":"a-off","parentId":"a","timestamp":"now","epochId":"old","revision":1,"auditWatermark":"u","snapshot":"#,
        )?;
        writeln!(
            output,
            "{}",
            json!({"type":"clm_on","id":"b","parentId":"u","timestamp":"now","epochId":"sibling","revision":0,"auditWatermark":"u","snapshot":{"blocks":[]}})
        )?;
        output.flush()?;
        output.get_ref().sync_all()?;
        drop(output);
        let mut outcomes = vec![
            core_probe("core-sibling", &source, &work)?,
            daemon_probe("daemon-sibling", &source, &work)?,
        ];
        // Extend the completed old epoch: this ancestry itself is now over the cap.
        let mut output = std::fs::OpenOptions::new().append(true).open(&source)?;
        writeln!(
            output,
            "{}",
            json!({"type":"clm_on","id":"c","parentId":"a-off","timestamp":"now","epochId":"later","revision":0,"auditWatermark":"u","snapshot":{"blocks":[]}})
        )?;
        output.sync_all()?;
        drop(output);
        outcomes.push(core_probe("core-historical-over-cap", &source, &work)?);
        outcomes.push(daemon_probe("daemon-historical-over-cap", &source, &work)?);
        let source_before = fingerprint(&source)?;
        let (navigation, _) = fixture(&root);
        let imported = navigation.prepare_import_jsonl(&json!({"inputPath":source}));
        let import_ok = if let Ok(mut prepared) = imported {
            let before = fingerprint(&prepared.file.path)?;
            let leaf = prepared.file.leaf_id().map(str::to_owned);
            let count = prepared.file.entries().len();
            let readable = prepared.file.model_context_checked().is_ok();
            let current = prepared
                .file
                .clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new());
            let rejected = if let Ok(ClmJournalReply::View(view)) = current {
                prepared
                    .file
                    .clm_journal(ClmJournalOp::Commit(off(view)), ProtectedActions::new())
                    .is_err_and(|e| {
                        matches!(e.downcast_ref::<ClmError>(), Some(ClmError::Budget(_)))
                    })
            } else {
                false
            };
            let unchanged = before == fingerprint(&prepared.file.path)?
                && prepared.file.leaf_id() == leaf.as_deref()
                && prepared.file.entries().len() == count;
            let usable = prepared.file.model_context_checked().is_ok();
            println!("copy-import: readable={readable} budget_rejected={rejected} unchanged={unchanged} usable={usable}");
            readable && rejected && unchanged && usable
        } else {
            println!("copy-import: rejected_existing_history=true");
            false
        };
        let source_unchanged = source_before == fingerprint(&source)?;
        println!(
            "copy-source-unchanged={source_unchanged} fixture={}",
            source.display()
        );
        outcomes.push(import_ok && source_unchanged);
        assert!(
            outcomes.iter().all(|outcome| *outcome),
            "owner/history outcomes: {outcomes:?}"
        );
        Ok(())
    }
}
