//! Foreground Herdr App root status over OSC 7501. Retry settle uses the existing daemon idle barrier.
use pa_types::daemon::DaemonCommand;
use std::collections::VecDeque;
use std::io::{IsTerminal, Stdout, Write};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Clone, Copy)]
pub(crate) enum RetryRouteState {
    Current,
    Changed,
}

pub(crate) struct RetrySettled {
    session: String,
    generation: u64,
    revision: u64,
    state: serde_json::Value,
}

/// Process-lifetime cleanup for an interactive Herdr App terminal.
/// Keep this guard across chat/Agents-view handoffs; dropping it clears the root report.
pub struct HerdrAppTerminal {
    enabled: bool,
}

impl HerdrAppTerminal {
    /// Detect a Herdr Core terminal. Construction does not invent an idle attach state.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            enabled: terminal_enabled(),
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

impl Drop for HerdrAppTerminal {
    fn drop(&mut self) {
        if self.enabled {
            // Exit-only best effort: never print another error into a broken terminal.
            let _ = write_packet(
                &mut std::io::stdout(),
                b"\x1b]7501;state=clear:app=prime-agent\x1b\\",
            );
        }
    }
}

fn terminal_enabled() -> bool {
    std::io::stdout().is_terminal()
        && ["HC_TERMINAL_ID", "HC_TERMINAL_SOCKET"]
            .iter()
            .all(|key| std::env::var(key).is_ok_and(|value| !value.trim().is_empty()))
}

fn write_packet(writer: &mut impl Write, packet: &[u8]) -> std::io::Result<()> {
    writer.write_all(packet)?;
    writer.flush()
}

pub(crate) struct HerdrAppStatus<W: Write = Stdout> {
    writer: Option<W>,
    last: Option<&'static str>,
    pending: VecDeque<(&'static str, String)>,
    provider_error: bool,
    retry_active: bool,
    deadline: Option<(Instant, &'static str)>,
    generation: u64,
    revision: Arc<AtomicU64>,
    retry_watcher: Option<tokio::task::AbortHandle>,
    settled_tx: mpsc::UnboundedSender<(RetrySettled, crate::daemon_client::RetryRoute)>,
    settled_rx: mpsc::UnboundedReceiver<(RetrySettled, crate::daemon_client::RetryRoute)>,
}

impl<W: Write> Drop for HerdrAppStatus<W> {
    fn drop(&mut self) {
        if let Some(watcher) = self.retry_watcher.take() {
            watcher.abort();
        }
    }
}

impl Default for HerdrAppStatus {
    fn default() -> Self {
        Self::new(None)
    }
}

impl HerdrAppStatus {
    pub(crate) fn terminal() -> Self {
        Self::new(terminal_enabled().then(std::io::stdout))
    }
}

impl<W: Write> HerdrAppStatus<W> {
    fn new(writer: Option<W>) -> Self {
        let (settled_tx, settled_rx) = mpsc::unbounded_channel();
        Self {
            writer,
            last: None,
            pending: VecDeque::new(),
            provider_error: false,
            retry_active: false,
            deadline: None,
            generation: 0,
            revision: Arc::new(AtomicU64::new(0)),
            retry_watcher: None,
            settled_tx,
            settled_rx,
        }
    }

    fn publish(&mut self, state: &'static str) {
        if self.writer.is_none()
            || self.pending.back().map(|(state, _)| *state).or(self.last) == Some(state)
        {
            return;
        }
        self.pending.push_back((
            state,
            format!("\x1b]7501;state={state}:app=prime-agent\x1b\\"),
        ));
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Called only after a terminal frame or at teardown, never by event application.
    /// The existing terminal writer can block here, but no report delays the preceding paint.
    pub(crate) fn drain_after_paint(&mut self) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        while let Some((state, packet)) = self.pending.pop_front() {
            if write_packet(writer, packet.as_bytes()).is_err() {
                // A broken terminal is best effort. Do not dedupe a later retry as delivered.
                self.last = None;
                self.pending.clear();
                break;
            }
            self.last = Some(state);
        }
    }

    pub(crate) fn session_open(&mut self, active: bool) {
        self.deadline = None;
        self.provider_error = false;
        self.cancel_retry_watch();
        self.publish(if active { "working" } else { "idle" });
    }

    pub(crate) fn event(&mut self, event: &serde_json::Value, queued: bool, now: Instant) {
        match event["type"].as_str().unwrap_or_default() {
            "agent_start" | "turn_start" => {
                self.revision.fetch_add(1, Ordering::SeqCst);
                self.deadline = None;
                self.provider_error = false;
                // Resumed retry attempts also emit these starts; keep the settle fallback armed.
                self.publish("working");
            }
            "compaction_start" => self.session_open(true),
            "auto_retry_start" => {
                self.deadline = None;
                self.provider_error = false;
                if !self.retry_active {
                    self.generation += 1;
                }
                self.retry_active = true;
                self.publish("working");
            }
            "message_end" if event["message"]["role"] == "assistant" => {
                self.provider_error = event["message"]["stopReason"] == "error";
            }
            "auto_retry_end" => {
                self.cancel_retry_watch();
                if event["finalError"] == "Retry cancelled" {
                    self.provider_error = false;
                    self.run_ended(queued, now);
                } else if event["finalError"].as_str().is_some() {
                    self.provider_error = true;
                    self.run_ended(queued, now);
                }
            }
            "agent_end" => {
                self.cancel_retry_watch();
                self.run_ended(queued, now);
            }
            "compaction_end" => self.idle_for_handoff(),
            _ => {}
        }
    }

    fn cancel_retry_watch(&mut self) {
        self.retry_active = false;
        self.generation += 1;
        if let Some(watcher) = self.retry_watcher.take() {
            watcher.abort();
        }
    }

    pub(crate) fn watch_retry(
        &mut self,
        client: &crate::daemon_client::DaemonClient,
        session: &str,
    ) {
        if !self.retry_active || self.writer.is_none() || self.retry_watcher.is_some() {
            return;
        }
        let client = client.clone();
        let session = session.to_string();
        let generation = self.generation;
        let revision = Arc::clone(&self.revision);
        let route = client.retry_route();
        let settled = self.settled_tx.clone();
        let task = tokio::spawn(async move {
            let result = route
                .request(
                    &client,
                    &session,
                    DaemonCommand::WaitForIdle {
                        id: None,
                        wait_for_rlm_quiescence: None,
                        active_session_id: session.clone(),
                        rest: serde_json::Map::default(),
                    },
                )
                .await;
            let revision = revision.load(Ordering::SeqCst);
            let state = if result.is_ok() {
                route
                    .request(
                        &client,
                        &session,
                        DaemonCommand::GetState {
                            id: None,
                            active_session_id: session.clone(),
                            rest: serde_json::Map::default(),
                        },
                    )
                    .await
                    .ok()
            } else {
                None
            };
            let _ = settled.send((
                RetrySettled {
                    session,
                    generation,
                    revision,
                    state: state.unwrap_or(serde_json::Value::Null),
                },
                route,
            ));
        });
        self.retry_watcher = Some(task.abort_handle());
    }

    pub(crate) async fn retry_settled(
        &mut self,
    ) -> Option<(RetrySettled, crate::daemon_client::RetryRoute)> {
        self.settled_rx.recv().await
    }

    pub(crate) fn apply_retry_settled(
        &mut self,
        settled: &RetrySettled,
        session: &str,
        route: RetryRouteState,
        now: Instant,
    ) -> bool {
        if !self.retry_active || settled.session != session || settled.generation != self.generation
        {
            return false;
        }
        self.retry_watcher.take();
        if matches!(route, RetryRouteState::Changed) {
            return true;
        }
        if settled.state.is_null() {
            return false;
        }
        if settled.revision != self.revision.load(Ordering::SeqCst)
            || settled.state["isStreaming"] == true
            || settled.state["isCompacting"] == true
        {
            return true;
        }
        if settled.state["isStreaming"] != false || settled.state["isCompacting"] != false {
            return false;
        }
        let Some(queued) = settled.state["sessionActions"]["queuedCount"].as_u64() else {
            return false;
        };
        self.provider_error = false;
        if queued > 0 {
            self.deadline = Some((now + Duration::from_millis(250), "idle"));
        } else {
            self.deadline = None;
            self.publish("idle");
        }
        false
    }

    fn run_ended(&mut self, queued: bool, now: Instant) {
        if self.provider_error {
            self.deadline = Some((now + Duration::from_millis(2500), "blocked"));
        } else if queued {
            self.deadline = Some((now + Duration::from_millis(250), "idle"));
        } else {
            self.idle_for_handoff();
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline.map(|(at, _)| at)
    }

    pub(crate) fn settle(&mut self, now: Instant) {
        if let Some((at, state)) = self.deadline {
            if now >= at {
                self.deadline = None;
                self.publish(state);
            }
        }
    }

    pub(crate) fn transport_lost(
        &mut self,
        client: &crate::daemon_client::DaemonClient,
        session: &str,
        lost_direct: Option<&str>,
    ) {
        let lost_session_transport = match lost_direct {
            Some(lost) => lost == session,
            None => client.direct_session_id().as_deref() != Some(session),
        };
        if lost_session_transport {
            self.idle_for_handoff();
        }
    }

    pub(crate) fn idle_for_handoff(&mut self) {
        self.deadline = None;
        self.provider_error = false;
        self.cancel_retry_watch();
        self.publish("idle");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retry_cancel_settles_without_a_turn_end_but_active_actions_do_not() {
        let now = Instant::now();
        for (end, resumed, queued_count) in [
            (None, false, 0),
            (Some("Retry cancelled"), false, 0),
            (None, true, 0),
            (None, true, 1),
            (Some("Retry cancelled"), true, 0),
            (Some("provider unavailable"), true, 0),
        ] {
            let mut status = HerdrAppStatus::new(Some(Vec::new()));
            status.session_open(true);
            status.event(&json!({"type":"auto_retry_start"}), false, now);
            // Each resumed provider attempt emits both starts before an abort can gate its end.
            if resumed {
                status.event(&json!({"type":"agent_start"}), false, now);
                status.event(&json!({"type":"turn_start"}), false, now);
            }
            // Direct busy prompts have no active action; a queue edit is not a settle signal.
            status.event(&json!({"type":"session_action_update","actions":{"queuedCount":0,"steering":[],"followUps":[]}}), false, now);
            status.settle(now + Duration::from_secs(10));
            status.drain_after_paint();
            assert_eq!(
                status.writer.as_ref().unwrap(),
                b"\x1b]7501;state=working:app=prime-agent\x1b\\"
            );
            if let Some(error) = end {
                status.event(
                    &json!({"type":"auto_retry_end","success":false,"finalError":error}),
                    false,
                    now,
                );
            }
            if end == Some("Retry cancelled") {
                status.drain_after_paint();
                assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
            }
            // The idle barrier confirms settle even when final queue actions are deduped.
            status.apply_retry_settled(&RetrySettled {
                session: "s1".into(), generation: status.generation,
                revision: status.revision.load(Ordering::SeqCst),
                state: json!({"isStreaming":false,"isCompacting":false,"sessionActions":{"queuedCount":queued_count}}),
            }, "s1", RetryRouteState::Current, now);
            if queued_count > 0 {
                status.settle(now + Duration::from_millis(249));
                status.drain_after_paint();
                assert_eq!(
                    status.writer.as_ref().unwrap(),
                    b"\x1b]7501;state=working:app=prime-agent\x1b\\"
                );
                status.settle(now + Duration::from_millis(250));
                status.drain_after_paint();
                assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
            }
            status.settle(now + Duration::from_secs(10));
            let expected = if end == Some("provider unavailable") {
                "blocked"
            } else {
                "idle"
            };
            status.drain_after_paint();
            assert_eq!(String::from_utf8(status.writer.as_ref().unwrap().clone()).unwrap(), format!("\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state={expected}:app=prime-agent\x1b\\"));
            status.writer.as_mut().unwrap().clear();
            status.event(&json!({"type":"agent_start"}), false, now);
            status.event(&json!({"type":"turn_start"}), false, now);
            status.event(&json!({"type":"agent_end"}), false, now);
            status.settle(now + Duration::from_secs(10));
            status.drain_after_paint();
            assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
        }
    }

    #[test]
    fn retry_confirmation_cannot_settle_a_new_run_busy_state_or_provider_failure() {
        let now = Instant::now();
        for scenario in [
            "different-session",
            "new-episode",
            "new-start",
            "busy",
            "compacting",
            "give-up",
        ] {
            let mut status = HerdrAppStatus::new(Some(Vec::new()));
            status.event(&json!({"type":"auto_retry_start"}), false, now);
            let mut settled = RetrySettled {
                session: "s1".into(),
                generation: status.generation,
                revision: status.revision.load(Ordering::SeqCst),
                state: json!({"isStreaming":false,"isCompacting":false,"sessionActions":{"queuedCount":0}}),
            };
            match scenario {
                "different-session" => settled.session = "other".into(),
                "new-episode" => {
                    status.session_open(true);
                    status.event(&json!({"type":"auto_retry_start"}), false, now);
                }
                "new-start" => status.event(&json!({"type":"agent_start"}), false, now),
                "busy" => settled.state["isStreaming"] = json!(true),
                "compacting" => settled.state["isCompacting"] = json!(true),
                "give-up" => status.event(
                    &json!({"type":"auto_retry_end","finalError":"provider unavailable"}),
                    false,
                    now,
                ),
                _ => unreachable!(),
            }
            status.apply_retry_settled(&settled, "s1", RetryRouteState::Current, now);
            status.settle(now + Duration::from_secs(3));
            status.drain_after_paint();
            let expected = if scenario == "give-up" {
                "\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=blocked:app=prime-agent\x1b\\"
            } else {
                "\x1b]7501;state=working:app=prime-agent\x1b\\"
            };
            assert_eq!(
                status.writer.as_ref().unwrap(),
                expected.as_bytes(),
                "{scenario}"
            );
        }
    }

    #[tokio::test]
    async fn a_rejected_confirmation_releases_the_waiter_and_idle_stays_eligible() {
        let now = Instant::now();
        for busy in [false, true] {
            let mut status = HerdrAppStatus::new(Some(Vec::new()));
            status.event(&json!({"type":"auto_retry_start"}), false, now);
            let task = tokio::spawn(async {});
            status.retry_watcher = Some(task.abort_handle());
            task.await.unwrap();
            let mut confirmation = RetrySettled {
                session: "s1".into(),
                generation: status.generation,
                revision: status.revision.load(Ordering::SeqCst),
                state: json!({"isStreaming":busy,"isCompacting":false,"sessionActions":{"queuedCount":0}}),
            };
            // Native reader queued this before resolving the state response. Whether the UI
            // selects events first or drains them from the confirmation arm, it applies first.
            status.event(&json!({"type":"agent_start"}), false, now);
            if busy {
                confirmation.revision = status.revision.load(Ordering::SeqCst);
            }
            assert!(status.apply_retry_settled(&confirmation, "s1", RetryRouteState::Current, now));
            assert!(
                status.retry_watcher.is_none(),
                "completed waiter must not strand the retry"
            );
            assert!(status.retry_active);
            confirmation.revision = status.revision.load(Ordering::SeqCst);
            confirmation.state["isStreaming"] = json!(false);
            status.apply_retry_settled(&confirmation, "s1", RetryRouteState::Current, now);
            assert!(
                status.retry_active,
                "a later native start must remain observable"
            );
            assert!(status.retry_watcher.is_none());
            status.drain_after_paint();
            assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
            status.event(
                &json!({"type":"session_action_update","actions":{"queuedCount":0}}),
                false,
                now,
            );
            status.settle(now + Duration::from_secs(30));
            status.drain_after_paint();
            assert!(status.retry_watcher.is_none());
            assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
            status.event(&json!({"type":"turn_start"}), false, now);
            confirmation.revision = status.revision.load(Ordering::SeqCst);
            status.apply_retry_settled(&confirmation, "s1", RetryRouteState::Current, now);
            status.drain_after_paint();
            assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
        }
    }

    #[tokio::test]
    async fn queued_native_starts_rearm_once_in_either_ui_select_order() {
        use crate::daemon_client::{DaemonClient, DaemonClientEvent};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        for event_first in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("retry.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let (requests, mut seen) = mpsc::unbounded_channel();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                writer.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7}}\n").await.unwrap();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                let mut first = true;
                while reader.read_line(&mut line).await.unwrap() != 0 {
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    line.clear();
                    let command = request["command"]["type"].as_str().unwrap();
                    requests.send(command.to_string()).unwrap();
                    if command == "get_state" && first {
                        first = false;
                        for kind in ["agent_start", "turn_start"] {
                            let event = json!({"type":"session_event","activeSessionId":"s1","event":{"type":kind}});
                            writer
                                .write_all(format!("{event}\n").as_bytes())
                                .await
                                .unwrap();
                        }
                    }
                    let response = json!({"type":"response","id":request["id"],"command":command,"success":true,
                        "data":{"isStreaming":false,"isCompacting":false,"sessionActions":{"queuedCount":0}}});
                    writer
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .unwrap();
                }
            });
            let (client, mut events) = DaemonClient::connect(&socket).await.unwrap();
            let mut status = HerdrAppStatus::new(Some(Vec::new()));
            let now = Instant::now();
            status.event(&json!({"type":"auto_retry_start"}), false, now);
            status.watch_retry(&client, "s1");
            let (first, _) = tokio::time::timeout(Duration::from_secs(3), status.retry_settled())
                .await
                .unwrap()
                .unwrap();
            // Either the normal event select wins, or confirmation drains this same FIFO first.
            let mut applied = 0;
            if event_first {
                let event = events.recv().await.unwrap();
                if let DaemonClientEvent::SessionEvent { event, .. } = event {
                    status.event(&event, false, now);
                    applied += 1;
                }
            }
            while let Ok(event) = events.try_recv() {
                if let DaemonClientEvent::SessionEvent { event, .. } = event {
                    status.event(&event, false, now);
                    applied += 1;
                }
            }
            assert_eq!(
                applied, 2,
                "reader queues prior frames before completing the response"
            );
            assert!(status.apply_retry_settled(&first, "s1", RetryRouteState::Current, now));
            status.watch_retry(&client, "s1");
            let (second, _) = tokio::time::timeout(Duration::from_secs(3), status.retry_settled())
                .await
                .unwrap()
                .unwrap();
            assert!(!status.apply_retry_settled(&second, "s1", RetryRouteState::Current, now));
            status.drain_after_paint();
            assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
            let mut commands = Vec::new();
            while let Ok(command) = seen.try_recv() {
                commands.push(command);
            }
            assert_eq!(
                commands,
                ["wait_for_idle", "get_state", "wait_for_idle", "get_state"]
            );
            assert!(
                status.retry_watcher.is_none(),
                "no activity means no third wait"
            );
            status.event(&json!({"type":"turn_start"}), false, now);
            status.watch_retry(&client, "s1");
            let (next, _) = tokio::time::timeout(Duration::from_secs(3), status.retry_settled())
                .await
                .unwrap()
                .unwrap();
            assert!(!status.apply_retry_settled(&next, "s1", RetryRouteState::Current, now));
            status.drain_after_paint();
            assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
            assert_eq!(seen.try_recv().unwrap(), "wait_for_idle");
            assert_eq!(seen.try_recv().unwrap(), "get_state");
            assert!(seen.try_recv().is_err());
            client.close();
            server.abort();
        }
    }

    #[test]
    fn tool_turn_boundaries_do_not_complete_the_agent_run() {
        let now = Instant::now();
        let mut status = HerdrAppStatus::new(Some(Vec::new()));
        status.event(&json!({"type":"auto_retry_start"}), false, now);
        // Native pa-agent run_loop: each tool batch ends a turn, then starts the next
        // assistant turn inside the same run. Only AgentEnd closes that run.
        for event in [
            json!({"type":"agent_start"}),
            json!({"type":"turn_start"}),
            json!({"type":"message_end","message":{"role":"assistant","stopReason":"toolUse"}}),
            json!({"type":"tool_execution_end","toolCallId":"tool-1"}),
            json!({"type":"turn_end","toolResults":[{"role":"toolResult"}]}),
            json!({"type":"turn_start"}),
            json!({"type":"message_end","message":{"role":"assistant","stopReason":"toolUse"}}),
            json!({"type":"tool_execution_end","toolCallId":"tool-2"}),
            json!({"type":"turn_end","toolResults":[{"role":"toolResult"}]}),
            json!({"type":"turn_start"}),
            json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}),
            json!({"type":"turn_end","toolResults":[]}),
        ] {
            status.event(&event, false, now);
            assert!(
                status.retry_active,
                "internal turn boundaries retain retry observation"
            );
            status.drain_after_paint();
            assert_eq!(
                status.writer.as_ref().unwrap(),
                b"\x1b]7501;state=working:app=prime-agent\x1b\\",
                "{event}"
            );
        }
        status.event(&json!({"type":"agent_end","messages":[]}), false, now);
        status.drain_after_paint();
        assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
    }

    #[tokio::test]
    async fn transport_loss_follows_the_attached_direct_reader_not_the_supervisor() {
        use crate::daemon_client::{DaemonClient, DaemonClientEvent};
        use pa_types::daemon::framing::{
            encode_private_frame, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS,
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        for supervisor_first in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("supervisor.sock");
            let worker_socket = dir.path().join("worker.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let worker_listener = tokio::net::UnixListener::bind(&worker_socket).unwrap();
            let (close_supervisor, supervisor_closed) = tokio::sync::oneshot::channel::<()>();
            let (close_worker, worker_closed) = tokio::sync::oneshot::channel::<()>();
            let (continue_worker, worker_continued) = tokio::sync::oneshot::channel::<()>();
            let worker = tokio::spawn(async move {
                let (stream, _) = worker_listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let hello = encode_private_frame(&json!({"kind":"outbound","outboundType":"daemon_hello"}),
                    br#"{"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":7}}"#, DEFAULT_PRIVATE_FRAME_LIMITS).unwrap();
                writer.write_all(&hello).await.unwrap();
                let mut reader = PrivateFrameReader::new(reader, DEFAULT_PRIVATE_FRAME_LIMITS);
                let frame = reader.read_frame().await.unwrap().unwrap();
                let response = json!({"type":"response","command":"peer_auth","success":true,"data":{"workerInstanceId":"inst-1","activeSessionId":"s1","purpose":"session_client"}});
                let frame = encode_private_frame(&json!({"kind":"outbound","outboundType":"response","requestId":frame.header["requestId"]}), &serde_json::to_vec(&response).unwrap(), DEFAULT_PRIVATE_FRAME_LIMITS).unwrap();
                writer.write_all(&frame).await.unwrap();
                worker_continued.await.unwrap();
                let event = json!({"type":"session_event","activeSessionId":"s1","event":{"type":"turn_start"}});
                let frame = encode_private_frame(
                    &json!({"kind":"outbound","outboundType":"session_event"}),
                    &serde_json::to_vec(&event).unwrap(),
                    DEFAULT_PRIVATE_FRAME_LIMITS,
                )
                .unwrap();
                writer.write_all(&frame).await.unwrap();
                worker_closed.await.unwrap();
            });
            let supervisor = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                writer.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"schemaRevision\":31,\"serverCapabilities\":[\"direct_peer_transport\"]}\n").await.unwrap();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let ticket = json!({"purpose":"session_client","socketPath":worker_socket,"socketIdentity":pa_types::platform::socket_identity(&worker_socket).unwrap(),"workerInstanceId":"inst-1","activeSessionId":"s1","grantId":"g1","token":"t","expiresAt":"2999-01-01T00:00:00.000Z"});
                let response = json!({"type":"response","id":request["id"],"command":"get_direct_worker_transport","success":true,"data":ticket});
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
                supervisor_closed.await.unwrap();
            });
            let (client, mut events) = DaemonClient::connect(&socket).await.unwrap();
            assert!(client.upgrade_direct("s1").await.unwrap());
            let mut dead = client.reader_dead();
            let mut status = HerdrAppStatus::new(Some(Vec::new()));
            status.session_open(true);
            status.transport_lost(&client, "s1", Some("unrelated"));
            if supervisor_first {
                close_supervisor.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(3), dead.changed())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(client.direct_session_id().as_deref(), Some("s1"));
                status.transport_lost(&client, "s1", /*lost_direct*/ None);
                status.drain_after_paint();
                assert_eq!(
                    status.writer.as_ref().unwrap(),
                    b"\x1b]7501;state=working:app=prime-agent\x1b\\"
                );
            }
            continue_worker.send(()).unwrap();
            let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap();
            let DaemonClientEvent::SessionEvent { event, .. } = event else {
                panic!("live direct event expected");
            };
            status.event(&event, false, Instant::now());
            status.drain_after_paint();
            assert_eq!(
                status.writer.as_ref().unwrap(),
                b"\x1b]7501;state=working:app=prime-agent\x1b\\"
            );
            close_worker.send(()).unwrap();
            let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap();
            let DaemonClientEvent::DirectLinkLost { active_session_id } = event else {
                panic!("direct reader loss expected");
            };
            status.transport_lost(&client, "s1", Some(&active_session_id));
            status.drain_after_paint();
            assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
            if !supervisor_first {
                assert!(!*dead.borrow());
            }
            status.transport_lost(&client, "s1", /*lost_direct*/ None);
            status.drain_after_paint();
            assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
            status.session_open(true);
            status.drain_after_paint();
            assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\\x1b]7501;state=working:app=prime-agent\x1b\\");
            client.close();
            supervisor.abort();
            worker.await.unwrap();
        }
    }

    #[test]
    fn lifecycle_application_does_not_reach_a_blocked_output_sink() {
        struct LatchedWriter(std::sync::mpsc::Receiver<()>);
        impl Write for LatchedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.recv().unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (release, latch) = std::sync::mpsc::channel();
        let (applied, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut status = HerdrAppStatus::new(Some(LatchedWriter(latch)));
            status.session_open(true);
            status.event(&json!({"type":"agent_end"}), false, Instant::now());
            status.session_open(true);
            applied.send(()).unwrap();
            // The explicit paint boundary owns output; the test only exercises event application.
            status.writer.take();
        });
        let outcome = completion.recv_timeout(Duration::from_secs(1));
        // Release enough writes on RED so the test never leaks a blocked thread.
        for _ in 0..32 {
            let _ = release.send(());
        }
        worker.join().unwrap();
        assert!(
            outcome.is_ok(),
            "event application waited on the terminal sink"
        );
    }

    #[test]
    fn one_paint_drains_every_transition_and_failed_delivery_can_be_retried() {
        struct FailingOnce {
            fail: bool,
            bytes: Vec<u8>,
        }
        impl Write for FailingOnce {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if std::mem::take(&mut self.fail) {
                    return Err(std::io::ErrorKind::BrokenPipe.into());
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut status = HerdrAppStatus::new(Some(Vec::new()));
        status.session_open(true);
        status.session_open(false);
        status.session_open(true);
        assert!(status.writer.as_ref().unwrap().is_empty());
        status.drain_after_paint();
        assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\\x1b]7501;state=working:app=prime-agent\x1b\\");

        let mut status = HerdrAppStatus::new(Some(FailingOnce {
            fail: true,
            bytes: Vec::new(),
        }));
        status.session_open(true);
        status.drain_after_paint();
        status.session_open(true);
        status.drain_after_paint();
        assert_eq!(
            status.writer.take().unwrap().bytes,
            b"\x1b]7501;state=working:app=prime-agent\x1b\\"
        );
    }

    #[test]
    fn retry_and_queue_graces_preserve_working_until_their_deadlines() {
        let now = Instant::now();
        let mut status = HerdrAppStatus::new(Some(Vec::new()));
        status.session_open(true);
        status.event(
            &json!({"type":"message_end","message":{"role":"assistant","stopReason":"error"}}),
            false,
            now,
        );
        status.event(&json!({"type":"agent_end"}), false, now);
        status.settle(now + Duration::from_millis(2499));
        status.drain_after_paint();
        assert_eq!(
            status.writer.as_ref().unwrap(),
            b"\x1b]7501;state=working:app=prime-agent\x1b\\"
        );
        status.event(
            &json!({"type":"auto_retry_start"}),
            false,
            now + Duration::from_millis(2499),
        );
        status.settle(now + Duration::from_secs(10));
        status.event(
            &json!({"type":"agent_end"}),
            true,
            now + Duration::from_secs(10),
        );
        status.settle(now + Duration::from_millis(10249));
        status.drain_after_paint();
        assert_eq!(
            status.writer.as_ref().unwrap(),
            b"\x1b]7501;state=working:app=prime-agent\x1b\\"
        );
        status.settle(now + Duration::from_millis(10250));
        status.drain_after_paint();
        assert_eq!(status.writer.as_ref().unwrap(), b"\x1b]7501;state=working:app=prime-agent\x1b\\\x1b]7501;state=idle:app=prime-agent\x1b\\");
    }

    #[test]
    fn provider_error_blocks_but_abort_and_handoff_do_not() {
        let now = Instant::now();
        let mut status = HerdrAppStatus::new(Some(Vec::new()));
        status.session_open(false);
        for reason in ["error", "aborted"] {
            status.event(&json!({"type":"turn_start"}), false, now);
            status.event(
                &json!({"type":"message_end","message":{"role":"assistant","stopReason":reason}}),
                false,
                now,
            );
            status.event(
                &json!({"type":"agent_end","error":"not a provider classifier"}),
                false,
                now,
            );
            status.settle(now + Duration::from_millis(2500));
        }
        status.event(&json!({"type":"turn_start"}), false, now);
        status.idle_for_handoff();
        status.settle(now + Duration::from_secs(30));
        status.drain_after_paint();
        assert_eq!(
            String::from_utf8(status.writer.take().unwrap()).unwrap(),
            concat!(
                "\x1b]7501;state=idle:app=prime-agent\x1b\\",
                "\x1b]7501;state=working:app=prime-agent\x1b\\",
                "\x1b]7501;state=blocked:app=prime-agent\x1b\\",
                "\x1b]7501;state=working:app=prime-agent\x1b\\",
                "\x1b]7501;state=idle:app=prime-agent\x1b\\",
                "\x1b]7501;state=working:app=prime-agent\x1b\\",
                "\x1b]7501;state=idle:app=prime-agent\x1b\\"
            )
        );
    }
}
