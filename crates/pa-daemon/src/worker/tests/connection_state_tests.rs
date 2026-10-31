//! The attach snapshot's context-usage fill (TS `createAgentConnectionState`
//! parity): the state block carries `contextUsage` off the same store walk
//! `get_session_stats` serves, so the client's first frame reads the tray's
//! usage off the snapshot instead of blocking on a stats round-trip.
use super::*;
use crate::engine::{PromptRequest, SessionEngine};
use crate::worker::WorkerConfig;

/// The scripted harness engine with a resolved model context window: the
/// tray-usage fill's one input beyond the store.
struct WindowedEngine;
impl SessionEngine for WindowedEngine {
    fn run_prompt(
        &self,
        _: usize,
        _: PromptRequest,
        _: &dyn Fn() -> bool,
        _: &mut dyn FnMut(crate::engine::EngineEvent) -> bool,
    ) {
    }
    fn run_side_question(
        &self,
        request: crate::engine::SideQuestionRequest,
        signal: &pa_agent::abort::AbortSignal,
        sink: &pa_core::session_engine::side_question::SideQuestionSink,
    ) -> crate::engine::SideQuestionOutcome {
        crate::engine::ScriptedEngine::default().run_side_question(request, signal, sink)
    }
    fn run_compaction(
        &self,
        request: crate::engine::CompactionRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> crate::engine::CompactionOutcome {
        crate::engine::ScriptedEngine::default().run_compaction(request, signal)
    }
    fn run_branch_summary(
        &self,
        request: crate::engine::BranchSummaryRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> crate::engine::BranchSummaryOutcome {
        crate::engine::ScriptedEngine::default().run_branch_summary(request, signal)
    }
    fn rebuild_session_context(
        &self,
        _: Vec<pa_types::session::FileEntry>,
        _: pa_core::session_engine::goal_driver::GoalBranchReload,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn model_context_window(&self) -> Option<u64> {
        Some(128_000)
    }
}

/// The model resolver may finish with an old selection while a command
/// waits for core. The lock-side selection check retries the resolve instead
/// of pairing that old model with the post-switch core state.
struct SwitchingEngine {
    selection: std::sync::Mutex<String>,
    pause: std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl SessionEngine for SwitchingEngine {
    fn run_prompt(
        &self,
        _: usize,
        _: PromptRequest,
        _: &dyn Fn() -> bool,
        _: &mut dyn FnMut(crate::engine::EngineEvent) -> bool,
    ) {
    }
    fn run_side_question(
        &self,
        request: crate::engine::SideQuestionRequest,
        signal: &pa_agent::abort::AbortSignal,
        sink: &pa_core::session_engine::side_question::SideQuestionSink,
    ) -> crate::engine::SideQuestionOutcome {
        WindowedEngine.run_side_question(request, signal, sink)
    }
    fn run_compaction(
        &self,
        request: crate::engine::CompactionRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> crate::engine::CompactionOutcome {
        WindowedEngine.run_compaction(request, signal)
    }
    fn run_branch_summary(
        &self,
        request: crate::engine::BranchSummaryRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> crate::engine::BranchSummaryOutcome {
        WindowedEngine.run_branch_summary(request, signal)
    }
    fn rebuild_session_context(
        &self,
        entries: Vec<pa_types::session::FileEntry>,
        reload: pa_core::session_engine::goal_driver::GoalBranchReload,
    ) -> anyhow::Result<()> {
        WindowedEngine.rebuild_session_context(entries, reload)
    }
    fn model_identity(&self) -> (Option<String>, Option<String>) {
        (
            Some("test".to_string()),
            Some(self.selection.lock().unwrap().clone()),
        )
    }
    fn model_metadata(&self) -> Option<Value> {
        let model = self.selection.lock().unwrap().clone();
        if let Some((entered, resume)) = self.pause.lock().unwrap().take() {
            entered.send(()).unwrap();
            resume.recv().unwrap();
        }
        Some(json!({ "provider": "test", "id": model }))
    }
    fn effective_thinking_level(&self) -> Option<String> {
        Some(self.selection.lock().unwrap().clone())
    }
    fn supported_thinking_levels(&self) -> Option<Vec<String>> {
        Some(vec![self.selection.lock().unwrap().clone()])
    }
    fn model_context_window(&self) -> Option<u64> {
        Some(if *self.selection.lock().unwrap() == "new" {
            200
        } else {
            100
        })
    }
}

#[tokio::test]
async fn summary_retry_resolves_the_selection_that_owns_the_locked_core() {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let engine = std::sync::Arc::new(SwitchingEngine {
        selection: std::sync::Mutex::new("old".to_string()),
        pause: std::sync::Mutex::new(Some((entered_tx, resume_rx))),
    });
    let dir = tempfile::tempdir().unwrap();
    let mut worker = Worker::new(
        WorkerConfig {
            socket_path: dir.path().join("worker.sock"),
            supervisor_socket_path: PathBuf::new(),
            token: "switch-test".into(),
            worker_instance_id: String::new(),
            active_session_id: "switch-test".into(),
            agent_dir: dir.path().join("agent"),
            recovery_journal_path: dir.path().join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": [] })),
        },
        None,
    );
    worker.engine = engine.clone();
    let worker = std::sync::Arc::new(worker);
    let core = worker.core.lock().unwrap();
    let reader = {
        let worker = std::sync::Arc::clone(&worker);
        std::thread::spawn(move || {
            let (core, inputs) = worker.summary_inputs();
            worker.summary_locked(&core, inputs)
        })
    };
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    *engine.selection.lock().unwrap() = "new".to_string();
    resume_tx.send(()).unwrap();
    drop(core);
    let summary = reader.join().unwrap();
    assert_eq!(
        summary.model,
        Some(json!({ "provider": "test", "id": "new" }))
    );
    assert_eq!(summary.thinking_level.as_deref(), Some("new"));
    let (core, inputs) = worker.connection_state_inputs();
    let state = Worker::connection_state_locked(&core, inputs);
    assert_eq!(
        state.model,
        Some(json!({ "provider": "test", "id": "new" }))
    );
    assert_eq!(state.available_thinking_levels, vec!["new".to_string()]);
    assert_eq!(state.context_usage, None);
}

/// The attach snapshot's state carries the tray's context usage exactly as
/// `get_session_stats` serves it (TS `createAgentConnectionState`'s
/// `contextUsage: session.getContextUsage()`): the same walk, the same
/// store, so the snapshot-fed tray row is byte-identical to the row a
/// blocking stats fetch would have painted.
#[tokio::test]
async fn the_attach_state_carries_the_stats_context_usage() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut worker = Worker::new(
        WorkerConfig {
            socket_path: dir.path().join("worker.sock"),
            supervisor_socket_path: PathBuf::new(),
            token: "context-usage-test".into(),
            worker_instance_id: String::new(),
            active_session_id: "ctx-usage".into(),
            agent_dir: dir.path().join("agent"),
            recovery_journal_path: dir.path().join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({"responses":[]})),
        },
        None,
    );
    worker.engine = std::sync::Arc::new(WindowedEngine);
    let created = worker
        .dispatch(
            "create",
            &json!({"cwd": "/tmp", "sessionDir": session_dir, "name": "usage"}),
        )
        .await;
    assert!(created.success, "create failed: {created:?}");

    let snapshot_state = {
        let (core, inputs) = worker.connection_state_inputs();
        let sealed = Worker::connection_state_locked(&core, inputs);
        serde_json::to_value(&sealed).unwrap()
    };
    assert_eq!(
        snapshot_state.get("contextUsage"),
        Some(&json!({"tokens": 0, "contextWindow": 128_000, "percent": 0.0})),
        "the attach state carries the tray usage: {snapshot_state}"
    );

    // The equivalence the byte-identity oracle rests on: the stats
    // response serves the very same usage object.
    let stats = worker.dispatch("get_session_stats", &json!({})).await;
    assert!(stats.success, "{stats:?}");
    assert_eq!(
        stats.data.as_ref().unwrap().get("contextUsage"),
        snapshot_state.get("contextUsage"),
        "the snapshot fill equals the stats response"
    );

    // A model-less engine keeps the field off the wire exactly like a TS
    // session without a model (the scripted default).
    let worker = Worker::new(
        WorkerConfig {
            socket_path: dir.path().join("worker2.sock"),
            supervisor_socket_path: PathBuf::new(),
            token: "context-usage-test-2".into(),
            worker_instance_id: String::new(),
            active_session_id: "ctx-usage-2".into(),
            agent_dir: dir.path().join("agent"),
            recovery_journal_path: dir.path().join("recovery2.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({"responses":[]})),
        },
        None,
    );
    worker
        .dispatch(
            "create",
            &json!({"cwd": "/tmp", "sessionDir": session_dir, "name": "usage-2"}),
        )
        .await;
    let modelless = {
        let (core, inputs) = worker.connection_state_inputs();
        let sealed = Worker::connection_state_locked(&core, inputs);
        serde_json::to_value(&sealed).unwrap()
    };
    assert!(
        modelless.get("contextUsage").is_none(),
        "a model-less session omits the field: {modelless}"
    );
}

fn strict_cwd_events(
    subscription: &mut tokio::sync::broadcast::Receiver<Arc<OutboundFrame>>,
) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        match subscription.try_recv() {
            Ok(frame) => {
                let outbound: Value =
                    serde_json::from_slice(&frame.payload).expect("valid worker frame");
                if outbound["event"]["type"] == "kernel_cwd_changed" {
                    events.push(outbound);
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(error) => panic!("worker event stream lost a frame: {error}"),
        }
    }
    events
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // faux provider registration is process-global across the async flow
async fn kernel_cwd_worker_routes_request_and_publishes_public_cell_readback() {
    let _faux = crate::agent_engine::tests::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let script = json!({"engine":"faux","responses":[
        {"content":[{"type":"toolCall","name":"python_repl","id":"cwd_cell",
            "arguments":{"code":format!("import os; os.chdir({}); print('cwd tool stayed intact')", json!(second.to_str().unwrap()))}}]},
        {"text":"done"},
        {"content":[{"type":"toolCall","name":"python_repl","id":"cwd_unchanged",
            "arguments":{"code":format!("import os; os.chdir({}); print('unchanged cell stayed intact')", json!(second.to_str().unwrap()))}}]},
        {"text":"done"}
    ]});
    let worker = Worker::new(
        WorkerConfig {
            socket_path: root.path().join("w.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "cwd-test".into(),
            worker_instance_id: String::new(),
            active_session_id: "cwd-session".into(),
            agent_dir: root.path().join("agent"),
            recovery_journal_path: root.path().join("recovery.jsonl"),
            telemetry_disabled: Some(true),
            script: Some(script),
        },
        None,
    );
    assert!(
        worker.agent_engine.is_some(),
        "faux agent engine must be live"
    );
    let created = worker
        .dispatch("create", &json!({"cwd":root.path(),"noSession":true}))
        .await;
    assert!(created.success, "{created:?}");
    let mut events = worker.events.subscribe();
    let wrong = worker
        .dispatch(
            "set_kernel_cwd",
            &json!({"activeSessionId":"other","dir":first}),
        )
        .await;
    assert!(
        !wrong.success,
        "wrong session changed kernel CWD: {wrong:?}"
    );
    let changed = worker
        .dispatch(
            "set_kernel_cwd",
            &json!({"activeSessionId":"cwd-session","dir":first}),
        )
        .await;
    assert!(changed.success, "{changed:?}");
    let attached = worker.dispatch("attach", &json!({"clientId":"test"})).await;
    assert_eq!(
        attached.data.as_ref().unwrap()["snapshot"]["state"]["kernelCwd"],
        json!(first)
    );
    assert_eq!(
        attached.data.as_ref().unwrap()["snapshot"]["state"]["cwd"],
        json!(root.path())
    );
    strict_cwd_events(&mut events);
    let engine = worker.agent_engine.as_ref().unwrap().clone();
    let provisioner = engine
        .kernel_cwd_probe
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .upgrade()
        .unwrap();
    let mut changed_receipt = provisioner.subscribe_cwd_observations();
    let tool_finished = tokio::task::spawn_blocking(move || {
        let mut finished = false;
        engine.run_prompt(
            0,
            crate::engine::PromptRequest {
                batch: Vec::new(),
                images: Vec::new(),
                message: "change cwd".into(),
                source: "user".into(),
                agent_message_id: None,
                custom_message: None,
            },
            &|| false,
            &mut |event| {
                if let crate::engine::EngineEvent::ToolExecutionEnd {
                    tool_call_id,
                    result,
                    ..
                } = &event
                {
                    if tool_call_id == "cwd_cell" {
                        finished = result.to_string().contains("cwd tool stayed intact");
                    }
                }
                true
            },
        );
        finished
    })
    .await
    .unwrap();
    assert!(
        tool_finished,
        "the public python_repl result must survive CWD observation"
    );
    let receipt = tokio::time::timeout(std::time::Duration::from_secs(10), changed_receipt.recv())
        .await
        .expect("changed observation must settle")
        .expect("changed receipt channel closed");
    assert_eq!(receipt.cwd, second);
    assert!(receipt.changed);
    let changed_frames = strict_cwd_events(&mut events);
    assert_eq!(
        changed_frames.len(),
        1,
        "changed cell must emit exactly one CWD frame"
    );
    assert_eq!(changed_frames[0]["activeSessionId"], "cwd-session");
    assert_eq!(changed_frames[0]["event"]["cwd"], json!(second));

    let mut unchanged_receipt = provisioner.subscribe_cwd_observations();
    let engine = worker.agent_engine.as_ref().unwrap().clone();
    let unchanged_tool_finished = tokio::task::spawn_blocking(move || {
        let mut finished = false;
        engine.run_prompt(
            0,
            crate::engine::PromptRequest {
                batch: Vec::new(),
                images: Vec::new(),
                message: "same cwd".into(),
                source: "user".into(),
                agent_message_id: None,
                custom_message: None,
            },
            &|| false,
            &mut |event| {
                if let crate::engine::EngineEvent::ToolExecutionEnd {
                    tool_call_id,
                    result,
                    ..
                } = &event
                {
                    if tool_call_id == "cwd_unchanged" {
                        finished = result.to_string().contains("unchanged cell stayed intact");
                    }
                }
                true
            },
        );
        finished
    })
    .await
    .unwrap();
    assert!(
        unchanged_tool_finished,
        "unchanged public cell tool result must survive"
    );
    let receipt =
        tokio::time::timeout(std::time::Duration::from_secs(10), unchanged_receipt.recv())
            .await
            .expect("unchanged observation must settle")
            .expect("unchanged receipt channel closed");
    assert_eq!(receipt.cwd, second);
    assert!(!receipt.changed);
    assert!(
        strict_cwd_events(&mut events).is_empty(),
        "unchanged public cell emitted CWD frame"
    );
    let state = worker.dispatch("get_connection_state", &json!({})).await;
    assert_eq!(state.data.as_ref().unwrap()["kernelCwd"], json!(second));
}
