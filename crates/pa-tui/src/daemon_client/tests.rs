use super::*;
use serde_json::json;
use tokio::net::UnixListener;

/// Minimal scripted supervisor used by client tests: hello on connect,
/// canned responses keyed by command type.
async fn spawn_mock_daemon(listener: UnixListener) {
    let (stream, _) = listener.accept().await.expect("accept");
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let hello = json!({
        "type": "daemon_hello",
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "clientId": "srv",
        "serverCapabilities": [],
    });
    let mut line = serde_json::to_string(&hello).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();
    let mut seen = String::new();
    loop {
        seen.clear();
        match reader.read_line(&mut seen).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let envelope: Value = serde_json::from_str(seen.trim()).unwrap();
        let id = envelope["id"].as_str().unwrap().to_string();
        let command_type = envelope["command"]["type"].as_str().unwrap();
        if command_type == "prompt" {
            // Stream an event before the response, like the real worker.
            let event = json!({
                "type": "session_event",
                "activeSessionId": "s1",
                "event": { "type": "turn_end" },
            });
            let mut payload = serde_json::to_string(&event).unwrap();
            payload.push('\n');
            writer.write_all(payload.as_bytes()).await.unwrap();
        }
        let response = json!({
            "type": "response",
            "id": id,
            "command": command_type,
            "success": true,
            "data": { "ok": true },
        });
        let mut payload = serde_json::to_string(&response).unwrap();
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.unwrap();
    }
}

fn empty_prompt_input() -> pa_types::daemon::PromptInput {
    pa_types::daemon::PromptInput {
        content: None,
        images: None,
        streaming_behavior: None,
        queue_if_busy: None,
        expand_prompt_templates: None,
        source: None,
        agent_message_id: None,
        custom_message: None,
        queue_key: None,
        prefix_messages: None,
        admission_id: None,
        rlm_notice_nonce: None,
    }
}

#[tokio::test]
async fn kernel_cwd_capability_requires_all_three_hello_gates() {
    for (version, revision, capabilities, allowed) in [
        (7, 31, vec!["kernel_cwd_propagation"], true),
        (7, 32, vec!["kernel_cwd_propagation"], true),
        (7, 30, vec!["kernel_cwd_propagation"], false),
        (7, 31, vec![], false),
        (6, 32, vec!["kernel_cwd_propagation"], false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello = json!({"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":version},
                "schemaRevision":revision,"clientId":"srv","serverCapabilities":capabilities});
            stream
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();
        });
        let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
        assert_eq!(
            client.supports_kernel_cwd_propagation(),
            allowed,
            "version {version}, revision {revision}"
        );
        client.close();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn handshake_and_request_round_trip() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move { spawn_mock_daemon(listener).await });

    let (client, mut events) = DaemonClient::connect(&socket).await.unwrap();
    assert_eq!(client.protocol().version, 7);
    let data = client
        .request_ok(DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: None,
            rest: Map::default(),
        })
        .await
        .unwrap();
    assert_eq!(data["ok"], true);

    client
        .request_ok(DaemonCommand::Prompt {
            id: None,
            active_session_id: "s1".to_string(),
            message: "hi".to_string(),
            input: empty_prompt_input(),
            rest: Map::default(),
        })
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            event,
            DaemonClientEvent::SessionEvent { ref event, .. } if event["type"] == "turn_end"
        ),
        "unexpected event: {event:?}"
    );
    client.close();
}

#[tokio::test]
async fn request_timeout_reports_socket() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // A daemon that sends hello but never answers commands.
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut writer = stream;
        let mut hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "clientId": "srv",
            "serverCapabilities": [],
        })
        .to_string();
        hello.push('\n');
        writer.write_all(hello.as_bytes()).await.unwrap();
        std::future::pending::<()>().await;
    });
    let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
    let error = client
        .request_with_timeout(
            DaemonCommand::List {
                id: None,
                all: None,
                cwd: None,
                session_dir: None,
                include_client_owned: None,
                rest: Map::default(),
            },
            100,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Timed out after"),
        "unexpected error: {error}"
    );
    assert!(error
        .to_string()
        .contains(socket.display().to_string().as_str()));
    assert!(!is_daemon_rejection(&error));
}

#[tokio::test]
async fn a_request_after_the_reader_died_refuses_instead_of_riding_the_budget() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // A daemon that greets, then drops the socket.
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut writer = stream;
        let mut hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "clientId": "srv",
            "serverCapabilities": [],
        })
        .to_string();
        hello.push('\n');
        writer.write_all(hello.as_bytes()).await.unwrap();
        drop(writer);
    });
    let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
    // Observable readiness: wait for the reader's death watch before
    // sending (the failure pass has run by then, so the request would
    // register after it — the exact race the refusal closes).
    let mut reader_dead = client.reader_dead();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !*reader_dead.borrow_and_update() {
            reader_dead
                .changed()
                .await
                .expect("the death watch stays live");
        }
    })
    .await
    .expect("the reader death watch fires when the socket closes");
    // A request budget far beyond the refusal bound: without the
    // refusal the send would ride it out and this await would outlive
    // the one-second failure bound.
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        client.request_with_timeout(
            DaemonCommand::List {
                id: None,
                all: None,
                cwd: None,
                session_dir: None,
                include_client_owned: None,
                rest: Map::default(),
            },
            30_000,
        ),
    )
    .await
    .expect("a dead reader refuses the send instead of riding the budget")
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("the daemon connection is closed"),
        "unexpected error: {error}"
    );
    assert!(error
        .to_string()
        .contains(socket.display().to_string().as_str()));
    // The refusal is a transport failure: transient for the submit path
    // (the pane stays mounted for the reconnect driver), never a daemon
    // rejection.
    assert!(is_daemon_unreachable(&error));
    assert!(!is_daemon_rejection(&error));
}

#[test]
fn failed_response_is_a_typed_rejection() {
    let response = DaemonResponse {
        id: None,
        command: "prompt".to_string(),
        success: false,
        data: None,
        error: Some("Prompt cannot be empty".to_string()),
        error_info: None,
    };
    let error = response_data_or_error("prompt", response).unwrap_err();
    assert!(is_daemon_rejection(&error));
    let rejection = error
        .downcast_ref::<RequestRejected>()
        .expect("typed rejection");
    assert_eq!(rejection.command, "prompt");
    assert_eq!(rejection.message, "Prompt cannot be empty");
    assert_eq!(
        rejection.to_string(),
        "the daemon rejected the prompt request: Prompt cannot be empty"
    );
}

#[test]
fn model_catalog_changed_parses_to_the_refresh_event() {
    // The worker's broadcast frame parses into the refresh event:
    // unknown payloads stay None, this one never drops silently.
    let value = json!({"type": "model_catalog_changed"});
    assert!(matches!(
        client_event_from_value(&value),
        Some(DaemonClientEvent::ModelCatalogChanged)
    ));
}

#[test]
fn kernel_cwd_legacy_parser_tolerates_optional_state_and_event() {
    let frame = json!({
        "type":"session_event", "activeSessionId":"s1",
        "event":{"type":"kernel_cwd_changed","cwd":"/effective"},
        "meta":{"sequence":4}
    });
    let Some(DaemonClientEvent::SessionEvent {
        event,
        meta_sequence,
        ..
    }) = client_event_from_value(&frame)
    else {
        panic!("legacy client lost the event frame")
    };
    assert_eq!(meta_sequence, 4);
    assert_eq!(event["cwd"], "/effective");
    assert!(matches!(
        crate::snapshot::event_to_update(&event),
        None | Some(crate::snapshot::TurnUpdate::StatusUpdate)
    ));

    let state = json!({"activeSessionId":"s1", "snapshot":{
        "state":{"sessionId":"saved","cwd":"/session","kernelCwd":"/effective"},
        "messages":[], "lastEventSequence":4
    }});
    let data = crate::snapshot::attach_data_from_response(state).unwrap();
    assert_eq!(data.snapshot["state"]["kernelCwd"], "/effective");
    assert_eq!(crate::snapshot::reconstruct(&data).session_id, "saved");
}

/// The layout handoff keys the stash on the LATEST sequence the worker
/// reported, so `meta.sequence` must ride the parsed session event.
#[test]
fn session_event_parses_the_meta_sequence_for_the_handoff_tracker() {
    let with_sequence = json!({
        "type": "session_event",
        "activeSessionId": "sess-1",
        "event": { "type": "message_end" },
        "meta": {
            "id": "sess-1:41",
            "sequence": 41,
            "cursor": { "generation": "g-1", "sequence": 41 }
        }
    });
    match client_event_from_value(&with_sequence) {
        Some(DaemonClientEvent::SessionEvent { meta_sequence, .. }) => {
            assert_eq!(meta_sequence, 41);
        }
        other => panic!("the frame must parse as a session event: {other:?}"),
    }
    // The cursor's sequence is the fallback shape; a frame without
    // either collapses to zero (the tracker's monotonic max ignores
    // it - an unkeyed event never lowers the tracked sequence).
    let cursor_only = json!({
        "type": "session_event",
        "activeSessionId": "sess-1",
        "event": { "type": "message_end" },
        "meta": { "cursor": { "generation": "g-1", "sequence": 12 } }
    });
    match client_event_from_value(&cursor_only) {
        Some(DaemonClientEvent::SessionEvent { meta_sequence, .. }) => {
            assert_eq!(meta_sequence, 12);
        }
        other => panic!("the frame must parse as a session event: {other:?}"),
    }
    let no_meta = json!({
        "type": "session_event",
        "activeSessionId": "sess-1",
        "event": { "type": "message_end" }
    });
    match client_event_from_value(&no_meta) {
        Some(DaemonClientEvent::SessionEvent { meta_sequence, .. }) => {
            assert_eq!(meta_sequence, 0);
        }
        other => panic!("the frame must parse as a session event: {other:?}"),
    }
}

#[test]
fn plain_errors_are_not_rejections() {
    let error = anyhow!("the daemon connection is closed");
    assert!(!is_daemon_rejection(&error));
}

/// TS #2391 `update_restarting` wire round-trip: the typed info
/// survives the wire to a `RequestRejected`, and an unrelated refusal
/// never classifies as the update-restart transient state.
#[test]
fn update_restarting_rejection_round_trips_the_wire() {
    let line = serde_json::from_str::<DaemonResponse>(
        r#"{"type":"response","command":"create","success":false,"error":"Daemon is preparing an update restart","errorInfo":{"code":"update_restarting"}}"#,
    )
    .expect("typed refusal parses");
    let error = response_data_or_error("create", line).unwrap_err();
    assert!(is_update_restarting_rejection(&error));
    let rejection = error
        .downcast_ref::<RequestRejected>()
        .expect("typed rejection");
    assert_eq!(
        rejection.error_info,
        Some(pa_types::daemon::DaemonErrorInfo::UpdateRestarting)
    );
    let legacy = serde_json::from_str::<DaemonResponse>(
        r#"{"type":"response","command":"create","success":false,"error":"Daemon is preparing an update restart"}"#,
    )
    .expect("legacy refusal parses");
    let error = response_data_or_error("create", legacy).unwrap_err();
    assert!(is_update_restarting_rejection(&error));
    let other = serde_json::from_str::<DaemonResponse>(
        r#"{"type":"response","command":"create","success":false,"error":"Unknown active session: active-gap"}"#,
    )
    .expect("unrelated refusal parses");
    let error = response_data_or_error("create", other).unwrap_err();
    assert!(!is_update_restarting_rejection(&error));
}

/// The session-addressed lanes' kernel-not-running refusal (the
/// `/factory off` guard's definitive-zero class): the exact wire string
/// classifies through the plain-message predicate, and no other refusal
/// does — the guard's zero read must not fire on an unknown-command or
/// initializing refusal, which stay unreadable and fail closed.
#[test]
fn kernel_not_running_refusal_round_trips_the_wire() {
    let line = serde_json::from_str::<DaemonResponse>(
        r#"{"type":"response","command":"factory_activity","success":false,"error":"Kernel is not running"}"#,
    )
    .expect("refusal parses");
    let error = response_data_or_error("factory_activity", line).unwrap_err();
    assert!(is_kernel_not_running(&error));
    // A refusal that merely embeds the phrase is not the class.
    let wrapped = serde_json::from_str::<DaemonResponse>(
        r#"{"type":"response","command":"factory_activity","success":false,"error":"the session reports: Kernel is not running for this lane"}"#,
    )
    .expect("unrelated refusal parses");
    let error = response_data_or_error("factory_activity", wrapped).unwrap_err();
    assert!(!is_kernel_not_running(&error));
    // The adjacent unreadable classes never classify as the zero read.
    for message in [
        "Session is still initializing",
        "Unknown worker command: factory_activity",
    ] {
        let unrelated = serde_json::from_str::<DaemonResponse>(&format!(
            r#"{{"type":"response","command":"factory_activity","success":false,"error":"{message}"}}"#
        ))
        .expect("unrelated refusal parses");
        let error = response_data_or_error("factory_activity", unrelated).unwrap_err();
        assert!(!is_kernel_not_running(&error), "{message}");
    }
    // A transport failure is not a refusal at all.
    let plain = anyhow!("the daemon connection is closed");
    assert!(!is_kernel_not_running(&plain));
}

#[test]
fn fail_pending_resolves_a_transport_failure_not_a_refusal() {
    // The reader task's dead-connection failure must stay on the
    // transport half of the pending channel: a closed socket can never
    // masquerade as a daemon refusal and keep the UI alive.
    let shared = Shared::default();
    let (tx, rx) = oneshot::channel();
    shared.register_pending("daemon_1".to_string(), tx, None);
    shared.fail_pending("daemon_", "the daemon connection closed");
    let error = rx.blocking_recv().unwrap().unwrap_err();
    assert!(!is_daemon_rejection(&error));
    assert_eq!(error.to_string(), "the daemon connection closed");
}

/// A scripted worker socket for the direct-link tests: hello frame,
/// `peer_auth` acceptance, one attach response, then one streamed event.
async fn spawn_mock_worker(listener: tokio::net::UnixListener, seen: mpsc::UnboundedSender<Value>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (stream, _) = listener.accept().await.expect("worker accept");
    let (mut reader, mut writer) = stream.into_split();
    let hello = pa_types::daemon::framing::encode_private_frame(
        &serde_json::json!({ "kind": "outbound", "outboundType": "daemon_hello" }),
        br#"{"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":7}}"#,
        pa_types::daemon::framing::DEFAULT_PRIVATE_FRAME_LIMITS,
    )
    .unwrap();
    writer.write_all(&hello).await.unwrap();

    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut attached = false;
    loop {
        let read = reader.read(&mut chunk).await.expect("worker read");
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..read]);
        let mut decoder = pa_types::daemon::framing::PrivateFrameDecoder::new(
            pa_types::daemon::framing::DEFAULT_PRIVATE_FRAME_LIMITS,
        );
        let frames = decoder.push(&pending).expect("decode frames");
        pending.clear();
        for frame in frames {
            let command_type = frame.header["commandType"].as_str().unwrap_or_default();
            if command_type == "set_kernel_cwd" {
                seen.send(serde_json::from_slice(&frame.payload).unwrap())
                    .unwrap();
            }
            let request_id = frame.header["requestId"].as_str().unwrap_or_default();
            let (success, data) = match command_type {
                "peer_auth" => (
                    true,
                    Some(serde_json::json!({
                        "workerInstanceId": "inst-1",
                        "activeSessionId": "s1",
                        "purpose": "session_client",
                    })),
                ),
                "attach" => {
                    attached = true;
                    (
                        true,
                        Some(serde_json::json!({
                            "activeSessionId": "s1",
                            "snapshot": { "messages": [], "summary": {} },
                        })),
                    )
                }
                "get_state" => (
                    true,
                    Some(serde_json::json!({ "id": "s1", "activity": "idle" })),
                ),
                "set_kernel_cwd" => (true, None),
                _ => (false, None),
            };
            let response = serde_json::json!({
                "id": request_id,
                "type": "response",
                "command": command_type,
                "success": success,
                "data": data,
            });
            let frame = pa_types::daemon::framing::encode_private_frame(
                &serde_json::json!({
                    "kind": "outbound",
                    "requestId": request_id,
                    "outboundType": "response",
                }),
                &serde_json::to_vec(&response).unwrap(),
                pa_types::daemon::framing::DEFAULT_PRIVATE_FRAME_LIMITS,
            )
            .unwrap();
            writer.write_all(&frame).await.unwrap();
        }
        if attached {
            // Stream one event, then wait for the client to close.
            let event = serde_json::json!({
                "type": "session_event",
                "activeSessionId": "s1",
                "event": { "type": "turn_end" },
            });
            let frame = pa_types::daemon::framing::encode_private_frame(
                &serde_json::json!({
                    "kind": "outbound",
                    "outboundType": "session_event",
                    "activeSessionId": "s1",
                }),
                &serde_json::to_vec(&event).unwrap(),
                pa_types::daemon::framing::DEFAULT_PRIVATE_FRAME_LIMITS,
            )
            .unwrap();
            writer.write_all(&frame).await.unwrap();
            attached = false;
        }
    }
}

#[tokio::test]
async fn kernel_cwd_direct_upgrade_routes_attach_and_streams_events() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("d.sock");
    let worker_socket = dir.path().join("w.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let worker_listener = tokio::net::UnixListener::bind(&worker_socket).unwrap();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let worker = tokio::spawn(async move { spawn_mock_worker(worker_listener, seen_tx).await });

    // Scripted supervisor: advertises direct_peer_transport, answers the
    // ticket request with a ticket for the mock worker socket.
    let ticket_socket = worker_socket.clone();
    let supervisor = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("supervisor accept");
        let (reader, mut writer) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        let hello = serde_json::json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "schemaRevision": 31,
            "serverCapabilities": ["direct_peer_transport", "kernel_cwd_propagation"],
        });
        writer
            .write_all(format!("{hello}\n").as_bytes())
            .await
            .unwrap();
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap() == 0 || line.trim().is_empty() {
                break;
            }
            let envelope: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let id = envelope["id"].as_str().unwrap().to_string();
            let command_type = envelope["command"]["type"].as_str().unwrap();
            let response = if command_type == "get_direct_worker_transport" {
                let identity = pa_types::platform::socket_identity(&ticket_socket).unwrap();
                serde_json::json!({
                    "type": "response",
                    "id": id,
                    "command": command_type,
                    "success": true,
                    "data": {
                        "purpose": "session_client",
                        "socketPath": ticket_socket.to_string_lossy(),
                        "socketIdentity": { "dev": identity.dev, "ino": identity.ino },
                        "workerInstanceId": "inst-1",
                        "activeSessionId": "s1",
                        "grantId": "g1",
                        "token": "t",
                        "expiresAt": "2999-01-01T00:00:00.000Z",
                    },
                })
            } else {
                serde_json::json!({
                    "type": "response",
                    "id": id,
                    "command": command_type,
                    "success": true,
                    "data": { "ok": true },
                })
            };
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
    });

    let (client, mut events) = DaemonClient::connect(&socket).await.unwrap();
    assert!(
        crate::direct_transport::supervisor_supports_direct(client.hello()),
        "capability advertised"
    );
    assert!(client.upgrade_direct("s1").await.unwrap());
    assert_eq!(client.direct_session_id().as_deref(), Some("s1"));

    let data = client
        .request_ok(DaemonCommand::Attach {
            id: None,
            active_session_id: "s1".to_string(),
            client_id: None,
            capabilities: None,
            resume_cursor: None,
            telemetry_disabled: None,
            recovery_config: None,
            env: None,
            launch_env: None,
            rest: Map::default(),
        })
        .await
        .unwrap();
    assert_eq!(data["activeSessionId"], "s1");
    let state = client
        .request_ok(DaemonCommand::GetState {
            id: None,
            active_session_id: "s1".to_string(),
            rest: Map::default(),
        })
        .await
        .unwrap();
    assert_eq!(state["id"], "s1");
    assert!(client.supports_kernel_cwd_propagation());
    client
        .request_ok(DaemonCommand::SetKernelCwd {
            id: None,
            active_session_id: "s1".to_string(),
            dir: "/next".to_string(),
            rest: Map::default(),
        })
        .await
        .unwrap();
    let received = seen_rx.recv().await.unwrap();
    assert_eq!(received["type"], "set_kernel_cwd");
    assert_eq!(received["activeSessionId"], "s1");
    assert_eq!(received["dir"], "/next");
    assert!(received["clientId"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            event,
            DaemonClientEvent::SessionEvent { ref event, .. } if event["type"] == "turn_end"
        ),
        "unexpected event: {event:?}"
    );
    client.close();
    supervisor
        .await
        .expect("mock supervisor must shut down cleanly");
    worker.await.expect("mock worker must shut down cleanly");
}

#[tokio::test]
async fn dead_connection_fails_pending_requests_immediately() {
    // The supervisor dies after the handshake while a request is in flight:
    // the reader task must fail the pending request at once (the exit-hang
    // class).
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut writer = stream;
        let mut hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "clientId": "srv",
            "serverCapabilities": [],
        })
        .to_string();
        hello.push('\n');
        writer.write_all(hello.as_bytes()).await.unwrap();
        // Hold the accept open until the request is in flight, then
        // close the socket (the supervisor process dies).
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(writer);
    });
    let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
    // The default timeout is 30s; the failure must land in well under a
    // second because the socket died, not because a timer fired.
    let started = std::time::Instant::now();
    let error = client
        .request_ok(DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: None,
            rest: Map::default(),
        })
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        error.to_string().contains("the daemon connection closed"),
        "unexpected error: {error}"
    );
    // The dead-connection failure is transport, never a daemon refusal.
    assert!(!is_daemon_rejection(&error));
    client.close();
    let _ = handle.await;
}

#[tokio::test]
async fn without_capability_upgrade_is_a_no_op() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move { spawn_mock_daemon(listener).await });
    let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
    assert!(!crate::direct_transport::supervisor_supports_direct(
        client.hello()
    ));
    assert!(!client.upgrade_direct("s1").await.unwrap());
    assert_eq!(client.direct_session_id(), None);
    client.close();
}

/// Observe routed setters at the real socket receiver, not at a mock response.
#[tokio::test]
async fn routed_setters_preserve_payload_budget_and_do_not_replay_sent_timeouts() {
    use pa_types::daemon::framing::{
        encode_private_frame, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS,
    };

    let commands = [
        (
            DaemonCommand::SetModel {
                id: None,
                active_session_id: "s1".into(),
                provider: "provider-olive".into(),
                model_id: "model-violet".into(),
                rest: Map::default(),
            },
            json!({"type":"set_model","activeSessionId":"s1","provider":"provider-olive","modelId":"model-violet"}),
        ),
        (
            DaemonCommand::SetThinkingLevel {
                id: None,
                active_session_id: "s1".into(),
                level: "high".into(),
                rest: Map::default(),
            },
            json!({"type":"set_thinking_level","activeSessionId":"s1","level":"high"}),
        ),
        (
            DaemonCommand::SetScopedModels {
                id: None,
                active_session_id: "s1".into(),
                scoped_models: json!([{"provider":"provider-cyan","modelId":"model-gold","thinkingLevel":"low"}]),
                rest: Map::default(),
            },
            json!({"type":"set_scoped_models","activeSessionId":"s1","scopedModels":[{"provider":"provider-cyan","modelId":"model-gold","thinkingLevel":"low"}]}),
        ),
    ];
    for (index, (command, expected)) in commands.into_iter().enumerate() {
        for fallback in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("d.sock");
            let worker_socket = dir.path().join("w.sock");
            let supervisor_listener = UnixListener::bind(&socket).unwrap();
            let worker_listener = UnixListener::bind(&worker_socket).unwrap();
            let (worker_seen_tx, mut worker_seen_rx) = mpsc::unbounded_channel::<Value>();
            let (supervisor_seen_tx, mut supervisor_seen_rx) = mpsc::unbounded_channel::<Value>();
            let (close_worker_tx, close_worker_rx) = oneshot::channel::<()>();
            let (late_reply_tx, late_reply_rx) = oneshot::channel::<()>();
            let worker = tokio::spawn(async move {
                let (stream, _) = worker_listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut reader = PrivateFrameReader::new(reader, DEFAULT_PRIVATE_FRAME_LIMITS);
                let hello = encode_private_frame(
                    &json!({"kind":"outbound","outboundType":"daemon_hello"}),
                    br#"{"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":7}}"#,
                    DEFAULT_PRIVATE_FRAME_LIMITS,
                ).unwrap();
                writer.write_all(&hello).await.unwrap();
                let auth = reader.read_frame().await.unwrap().expect("peer auth frame");
                assert_eq!(auth.header["commandType"], "peer_auth");
                let auth_id = auth.header["requestId"].as_str().unwrap();
                let response = json!({"type":"response","id":auth_id,"command":"peer_auth","success":true,
                    "data":{"workerInstanceId":"inst-1","activeSessionId":"s1","purpose":"session_client"}});
                writer.write_all(&encode_private_frame(
                    &json!({"kind":"outbound","outboundType":"response","requestId":auth_id}),
                    &serde_json::to_vec(&response).unwrap(), DEFAULT_PRIVATE_FRAME_LIMITS,
                ).unwrap()).await.unwrap();
                if fallback {
                    close_worker_rx.await.expect("release worker link");
                    return;
                }
                let selected = reader.read_frame().await.unwrap().expect("setter frame");
                assert_eq!(selected.header["kind"], "command");
                assert_eq!(
                    selected.header["commandType"],
                    serde_json::from_slice::<Value>(&selected.payload).unwrap()["type"]
                );
                worker_seen_tx
                    .send(serde_json::from_slice(&selected.payload).unwrap())
                    .unwrap();
                late_reply_rx.await.expect("release late worker reply");
                let request_id = selected.header["requestId"].as_str().unwrap();
                let response = json!({"type":"response","id":request_id,"command":selected.header["commandType"],"success":true});
                writer.write_all(&encode_private_frame(
                    &json!({"kind":"outbound","outboundType":"response","requestId":request_id}),
                    &serde_json::to_vec(&response).unwrap(), DEFAULT_PRIVATE_FRAME_LIMITS,
                ).unwrap()).await.unwrap();
                assert!(
                    reader.read_frame().await.unwrap().is_none(),
                    "no duplicate worker frame"
                );
            });
            let ticket_socket = worker_socket.clone();
            let supervisor = tokio::spawn(async move {
                let (stream, _) = supervisor_listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                writer.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"schemaRevision\":31,\"clientId\":\"srv\",\"serverCapabilities\":[\"direct_peer_transport\"]}\n").await.unwrap();
                let mut seen = Vec::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap() == 0 || line.trim().is_empty() {
                        break;
                    }
                    let envelope: Value = serde_json::from_str(&line).unwrap();
                    let kind = envelope["command"]["type"].as_str().expect("command type");
                    let response = if kind == "get_direct_worker_transport" {
                        let identity = pa_types::platform::socket_identity(&ticket_socket).unwrap();
                        json!({"type":"response","id":envelope["id"],"command":kind,"success":true,
                            "data":{"purpose":"session_client","socketPath":ticket_socket.to_string_lossy(),
                            "socketIdentity":{"dev":identity.dev,"ino":identity.ino},"workerInstanceId":"inst-1",
                            "activeSessionId":"s1","grantId":"g1","token":"t","expiresAt":"2999-01-01T00:00:00.000Z"}})
                    } else if kind == "list" {
                        assert_eq!(
                            seen.len(),
                            usize::from(fallback),
                            "replay preceded FIFO marker"
                        );
                        json!({"type":"response","id":envelope["id"],"command":kind,"success":true})
                    } else {
                        seen.push(envelope["command"].clone());
                        supervisor_seen_tx.send(envelope).unwrap();
                        continue;
                    };
                    writer
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .unwrap();
                }
                assert_eq!(
                    seen.len(),
                    usize::from(fallback),
                    "unexpected supervisor replay"
                );
            });
            let (client, mut events) = DaemonClient::connect(&socket).await.unwrap();
            assert!(client.upgrade_direct("s1").await.unwrap());
            if fallback {
                close_worker_tx.send(()).unwrap();
                let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    matches!(event, DaemonClientEvent::DirectLinkLost { active_session_id } if active_session_id == "s1")
                );
            }
            let budget_ms = 110 + (index as u64 * 23) + u64::from(fallback) * 11;
            let start = std::time::Instant::now();
            let request = tokio::spawn({
                let client = client.clone();
                let command = command.clone();
                async move { client.request_with_timeout(command, budget_ms).await }
            });
            let received = tokio::time::timeout(Duration::from_secs(3), async {
                if fallback {
                    supervisor_seen_rx.recv().await
                } else {
                    worker_seen_rx.recv().await
                }
            })
            .await
            .unwrap()
            .expect("selected receiver got command");
            let payload = if fallback {
                &received["command"]
            } else {
                &received
            };
            let client_id = &received["clientId"];
            assert_eq!(
                client_id.as_str(),
                Some(format!("daemon-tui:{}", std::process::id()).as_str())
            );
            let mut body = payload.clone();
            if let Some(fields) = body.as_object_mut() {
                fields.remove("clientId");
            }
            assert_eq!(body, expected, "route fallback={fallback}, setter={index}");
            let error = tokio::time::timeout(Duration::from_secs(3), request)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(
                start.elapsed() >= Duration::from_millis(budget_ms / 2),
                "premature timeout: {error}"
            );
            let endpoint = if fallback { &socket } else { &worker_socket };
            assert!(
                error
                    .to_string()
                    .contains(&format!("Timed out after {budget_ms}ms")),
                "{error}"
            );
            assert!(
                error.to_string().contains(&endpoint.display().to_string()),
                "{error}"
            );
            assert!(
                error.to_string().contains(if fallback {
                    "daemon response"
                } else {
                    "session response"
                }),
                "{error}"
            );
            if !fallback {
                assert!(
                    supervisor_seen_rx.try_recv().is_err(),
                    "direct command reached supervisor"
                );
            }
            // The successful control response is a FIFO boundary after terminal timeout.
            client
                .request_ok(DaemonCommand::List {
                    id: None,
                    all: None,
                    cwd: None,
                    session_dir: None,
                    include_client_owned: None,
                    rest: Map::default(),
                })
                .await
                .unwrap();
            if !fallback {
                late_reply_tx.send(()).unwrap();
            }
            client.close();
            tokio::time::timeout(Duration::from_secs(3), worker)
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), supervisor)
                .await
                .unwrap()
                .unwrap();
        }
    }
}

#[tokio::test]
async fn retry_wait_cancellation_removes_registration_without_waiting_for_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("cancel.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (seen_tx, seen_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        writer.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7}}\n").await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        seen_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
    let request_client = client.clone();
    let task = tokio::spawn(async move {
        request_client
            .request_ok(DaemonCommand::WaitForIdle {
                id: None,
                active_session_id: "s1".into(),
                wait_for_rlm_quiescence: None,
                rest: Map::default(),
            })
            .await
    });
    seen_rx.await.unwrap();
    assert_eq!(client.shared.pending.lock().unwrap().len(), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(client.shared.pending.lock().unwrap().is_empty());
    client.close();
    server.abort();
}

#[tokio::test]
async fn retry_route_rejects_a_replaced_direct_owner_without_supervisor_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("supervisor.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(spawn_mock_daemon(listener));
    let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
    let supervisor_route = client.retry_route();
    for index in 0..2 {
        let socket = dir.path().join(format!("worker-{index}.sock"));
        let listener = UnixListener::bind(&socket).unwrap();
        let (seen, _seen_rx) = mpsc::unbounded_channel();
        tokio::spawn(spawn_mock_worker(listener, seen));
        let ticket = pa_types::daemon::DaemonPeerTransportTicket {
            purpose: "session_client".into(),
            socket_path: socket.to_string_lossy().into(),
            socket_identity: pa_types::platform::socket_identity(&socket).unwrap(),
            worker_instance_id: "inst-1".into(),
            active_session_id: "s1".into(),
            grant_id: "g1".into(),
            token: "t".into(),
            expires_at: "2999-01-01T00:00:00.000Z".into(),
        };
        let pinned = client.retry_route();
        let link = connect_direct(
            &ticket,
            Arc::clone(&client.shared),
            client.direct.event_sender().unwrap(),
        )
        .await
        .unwrap();
        client.direct.set_link(link);
        let before = client.next_request_id.load(Ordering::SeqCst);
        let outcome = pinned
            .request(
                &client,
                "s1",
                DaemonCommand::GetState {
                    id: None,
                    active_session_id: "s1".into(),
                    rest: Map::default(),
                },
            )
            .await;
        assert!(outcome.is_err());
        assert_eq!(
            client.next_request_id.load(Ordering::SeqCst),
            before,
            "replaced route must not send on either socket"
        );
        assert!(!supervisor_route.is_current(&client, "s1"));
    }
    let pinned = client.retry_route();
    client.direct.drop_link();
    assert!(pinned
        .request(
            &client,
            "s1",
            DaemonCommand::GetState {
                id: None,
                active_session_id: "s1".into(),
                rest: Map::default(),
            }
        )
        .await
        .is_err());
    client.close();
    server.abort();
}
