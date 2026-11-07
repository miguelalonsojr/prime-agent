use super::*;

#[test]
fn foreground_reports_root_lifecycle_on_its_actual_pty() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let private = tempfile::tempdir().expect("private OSC test");
    let cwd = private.path().join("cwd");
    let agent = private.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        r#"{"onboardingShown":true,"telemetryEnabled":false}"#,
    )
    .unwrap();
    let socket = private.path().join("daemon.sock");
    let (tx, rx) = std::sync::mpsc::channel();
    let mut supervisor = MockSupervisor::bind(&socket, true);
    supervisor.cli_cwd = Some(cwd.clone());
    supervisor.osc_events = Some(tx);
    supervisor.attach_busy = true;
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    supervisor.osc_requests = Some(request_tx);
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    supervisor.cli_shutdown = Some(Arc::clone(&shutdown));
    let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pty = openpty(
        Some(&Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None,
    )
    .unwrap();
    let child = spawn_cli_child(
        &socket,
        &pty.slave,
        private.path(),
        &cwd,
        &agent,
        std::path::Path::new("/bin/sh"),
    );
    drop(pty.slave);
    let server_connected = Arc::clone(&connected);
    let server =
        std::thread::spawn(move || supervisor.serve_cli(&Arc::default(), &server_connected));
    let mut harness = InputLiveHarness {
        child,
        server: Some(server),
        connected,
        cli_socket: Some(socket),
        cli_shutdown: Some(shutdown),
        master: PtyReader::new(pty.master),
    };
    let mut events = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("UI connected");
    harness
        .master
        .wait_for_raw_since(0, "\x1b]7501;state=working:app=prime-agent\x1b\\");
    // A busy attach must not invent an idle report before its working snapshot.
    assert!(!String::from_utf8_lossy(&harness.master.output).contains("7501;state=idle"));
    write_json(
        &mut events,
        &json!({"type":"session_event","activeSessionId":"background-child","event":{"type":"turn_end"},"meta":{"sequence":99}}),
    );
    let mut responses = events.try_clone().unwrap();
    let mut sequence = 0;
    let mut send = |event: Value| {
        sequence += 1;
        write_json(
            &mut events,
            &json!({"type":"session_event","activeSessionId":"s1","event":event,"meta":{"sequence":sequence}}),
        );
    };
    let mut offset = harness.master.output.len();
    send(json!({"type":"turn_end"}));
    send(json!({"type":"agent_end","messages":[]}));
    harness
        .master
        .wait_for_raw_since(offset, "\x1b]7501;state=idle:app=prime-agent\x1b\\");
    offset = harness.master.output.len();
    send(json!({"type":"turn_start"}));
    send(
        json!({"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"provider down"}}),
    );
    send(json!({"type":"turn_end","error":"provider down"}));
    send(json!({"type":"agent_end","messages":[]}));
    harness
        .master
        .wait_for_raw_since(offset, "\x1b]7501;state=blocked:app=prime-agent\x1b\\");
    offset = harness.master.output.len();
    send(json!({"type":"turn_start"}));
    send(
        json!({"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"aborted"}}),
    );
    send(json!({"type":"turn_end","error":"aborted"}));
    send(json!({"type":"agent_end","messages":[]}));
    harness
        .master
        .wait_for_raw_since(offset, "\x1b]7501;state=idle:app=prime-agent\x1b\\");
    offset = harness.master.output.len();
    send(json!({"type":"turn_start"}));
    send(
        json!({"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error"}}),
    );
    send(json!({"type":"turn_end"}));
    send(json!({"type":"agent_end","messages":[]}));
    send(json!({"type":"auto_retry_start","attempt":1,"maxAttempts":2,"delayMs":0}));
    send(json!({"type":"turn_start"}));
    send(
        json!({"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"stop"}}),
    );
    send(json!({"type":"turn_end"}));
    send(json!({"type":"agent_end","messages":[]}));
    harness
        .master
        .wait_for_raw_since(offset, "\x1b]7501;state=idle:app=prime-agent\x1b\\");
    offset = harness.master.output.len();
    send(json!({"type":"turn_start"}));
    send(
        json!({"type":"session_action_update","actions":{"queuedCount":1,"steering":[],"followUps":["queued"]}}),
    );
    send(json!({"type":"turn_end"}));
    send(json!({"type":"agent_end","messages":[]}));
    send(json!({"type":"turn_start"}));
    send(
        json!({"type":"session_action_update","actions":{"queuedCount":0,"steering":[],"followUps":[]}}),
    );
    send(json!({"type":"turn_end"}));
    send(json!({"type":"agent_end","messages":[]}));
    harness
        .master
        .wait_for_raw_since(offset, "\x1b]7501;state=idle:app=prime-agent\x1b\\");
    for delivered_end in [true, false] {
        offset = harness.master.output.len();
        send(json!({"type":"turn_start"}));
        send(json!({"type":"auto_retry_start","attempt":1,"maxAttempts":2,"delayMs":10000}));
        send(json!({"type":"agent_start"}));
        send(json!({"type":"turn_start"}));
        send(
            json!({"type":"session_action_update","actions":{"queuedCount":0,"steering":[],"followUps":[],"active":{"phase":"running"}}}),
        );
        if delivered_end {
            send(
                json!({"type":"auto_retry_end","success":false,"attempt":1,"finalError":"Retry cancelled"}),
            );
        }
        // A direct prompt is busy without an active action. This unchanged projection may
        // be deduped at final settle, and abort can suppress every ending event.
        send(
            json!({"type":"session_action_update","actions":{"queuedCount":0,"steering":[],"followUps":[]}}),
        );
        if !delivered_end {
            let (mut id, mut command) = request_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("retry idle watcher");
            // Earlier delivered episodes may have left cancelled request IDs at the mock.
            loop {
                write_json(
                    &mut responses,
                    &json!({"type":"response","id":id,"command":command,"success":true}),
                );
                (id, command) = request_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("idle confirmation request");
                if command == "get_state" {
                    break;
                }
            }
            write_json(
                &mut responses,
                &json!({"type":"response","id":id,"command":command,"success":true,"data":{"id":"s1","isStreaming":false,"isCompacting":false,"sessionActions":{"queuedCount":0,"steering":[],"followUps":[]}}}),
            );
        }
        harness
            .master
            .wait_for_raw_since(offset, "\x1b]7501;state=idle:app=prime-agent\x1b\\");
    }
    offset = harness.master.output.len();
    send(json!({"type":"turn_start"}));
    harness
        .master
        .wait_for_raw_since(offset, "\x1b]7501;state=working:app=prime-agent\x1b\\");
    offset = harness.master.output.len();
    harness.type_text("/resume\r");
    harness
        .master
        .wait_for_raw_since(offset, "\x1b]7501;state=idle:app=prime-agent\x1b\\");
    offset = harness.master.output.len();
    harness.type_text("\x03\x03");
    harness
        .master
        .wait_for_raw_since(offset, "\x1b]7501;state=clear:app=prime-agent\x1b\\");
    let reports: Vec<_> = String::from_utf8_lossy(&harness.master.output)
        .split("\x1b]7501;")
        .skip(1)
        .map(|tail| tail.split("\x1b\\").next().unwrap().to_string())
        .collect();
    assert_eq!(
        reports,
        [
            "state=working:app=prime-agent",
            "state=idle:app=prime-agent",
            "state=working:app=prime-agent",
            "state=blocked:app=prime-agent",
            "state=working:app=prime-agent",
            "state=idle:app=prime-agent",
            "state=working:app=prime-agent",
            "state=idle:app=prime-agent",
            "state=working:app=prime-agent",
            "state=idle:app=prime-agent",
            "state=working:app=prime-agent",
            "state=idle:app=prime-agent",
            "state=working:app=prime-agent",
            "state=idle:app=prime-agent",
            "state=working:app=prime-agent",
            "state=idle:app=prime-agent",
            "state=clear:app=prime-agent"
        ]
    );
    if let Some(path) = std::env::var_os("PA_HERDR_OSC_CAPTURE") {
        std::fs::write(path, &harness.master.output).expect("save actual PTY capture");
    }
}

#[test]
fn headless_ui_with_herdr_environment_does_not_write_osc() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let private = tempfile::tempdir().unwrap();
    let socket = private.path().join("headless.sock");
    let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let supervisor = MockSupervisor::bind(&socket, true);
    let server_connected = Arc::clone(&connected);
    let server = std::thread::spawn(move || supervisor.serve(&Arc::default(), &server_connected));
    let pty = openpty(
        Some(&Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None,
    )
    .unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "input_live_open_child_mode", "--nocapture"])
        .env(CHILD_SOCKET_ENV, &socket)
        .env("PA_HERDR_HEADLESS", "1")
        .env("HC_TERMINAL_ID", "headless-test")
        .env("HC_TERMINAL_SOCKET", "unused")
        .stdin(Stdio::null())
        .stdout(slave_as_stdio(&pty.slave))
        .stderr(slave_as_stdio(&pty.slave))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let mut harness = InputLiveHarness {
        child,
        server: Some(server),
        connected,
        cli_socket: None,
        cli_shutdown: None,
        master: PtyReader::new(pty.master),
    };
    harness.master.wait_for_raw_since(0, "test result: ok.");
    assert!(!String::from_utf8_lossy(&harness.master.output).contains("\x1b]7501;"));
}

#[test]
fn redirected_cli_never_reports_even_when_terminal_identity_is_inherited() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "herdr_app_osc::redirected_terminal_guard_child",
            "--nocapture",
        ])
        .env("PA_HERDR_GUARD_CHILD", "1")
        .env("HC_TERMINAL_ID", "pipe-test")
        .env("HC_TERMINAL_SOCKET", "unused")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\x1b]7501;"));
}

#[test]
fn redirected_terminal_guard_child() {
    if std::env::var_os("PA_HERDR_GUARD_CHILD").is_none() {
        return;
    }
    let guard = pa_tui::herdr_app::HerdrAppTerminal::detect();
    assert!(!guard.enabled());
    drop(guard);
}
