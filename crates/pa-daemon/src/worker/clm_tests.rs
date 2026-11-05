//! CLM through the worker command dispatcher, durable owner, real agent and Python tool.

#[test]
fn clm_worker_configured_default_does_not_enable_resumed_history() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"clm":{"enabled":true}}"#,
        )
        .unwrap();
        let sessions = dir.path().join("sessions");
        let mut prior = pa_core::session::manager::SessionManager::persisted(dir.path(), &sessions);
        prior
            .append_message(
                serde_json::from_value(json!({
                    "role":"user", "content":"ordinary saved history", "timestamp":1
                }))
                .unwrap(),
            )
            .unwrap();
        prior.flush_now().unwrap();
        let path = prior.get_session_file().unwrap().to_path_buf();
        let original = std::fs::read(&path).unwrap();
        let worker = worker(dir.path(), "resumed-default");
        command(
            &worker,
            "create",
            json!({"cwd":dir.path(),"sessionPath":path}),
        )
        .await;
        let status = command(&worker, "get_clm_status", json!({})).await;
        assert_eq!(
            status["mode"], "off",
            "resuming is not configured-default consent"
        );
        assert!(status["epochId"].is_null());
        let after = std::fs::read(&path).unwrap();
        assert!(after.starts_with(&original));
        assert!(!pa_core::session::clm::contains_clm_records(&after));
        worker.agent_engine.as_ref().unwrap().dispose_kernel().await;
    });
}

#[test]
fn clm_worker_configured_default_is_consumed_before_runtime_rebuild() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"clm":{"enabled":false}}"#,
        )
        .unwrap();
        let worker = worker(dir.path(), "rebuilt-default");
        command(
            &worker,
            "create",
            json!({"cwd":dir.path(),"sessionDir":dir.path().join("sessions")}),
        )
        .await;
        assert_eq!(
            command(&worker, "get_clm_status", json!({})).await["mode"],
            "off"
        );
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"clm":{"enabled":true}}"#,
        )
        .unwrap();
        worker
            .agent_engine
            .as_ref()
            .unwrap()
            .retire_session_runtime()
            .await;
        let rebuilt = command(&worker, "get_clm_status", json!({})).await;
        assert_eq!(
            rebuilt["mode"], "off",
            "an already consumed default must not reapply on rebuild"
        );
        assert!(rebuilt["epochId"].is_null());
        command(&worker, "new_session", json!({})).await;
        let fresh = command(&worker, "get_clm_status", json!({})).await;
        assert_eq!(
            fresh["mode"], "on",
            "explicit new_session renews eligibility"
        );
        assert_eq!(fresh["modeSource"], "configured-default");
        assert!(fresh["epochId"].is_string());
        worker.agent_engine.as_ref().unwrap().dispose_kernel().await;
    });
}

use super::*;
use pa_ai::faux::{faux_assistant_text_message, FauxAssistantMessageOptions, FauxResponseStep};

fn worker(dir: &std::path::Path, active_id: &str) -> Worker {
    Worker::new(
        WorkerConfig {
            socket_path: dir.join(format!("{active_id}.sock")),
            supervisor_socket_path: dir.join("unused-supervisor.sock"),
            token: "clm-test".into(),
            worker_instance_id: active_id.into(),
            active_session_id: active_id.into(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join(format!("{active_id}-recovery.jsonl")),
            telemetry_disabled: Some(true),
            script: Some(
                json!({"engine":"faux", "responses":["first", "second", "reopened"], "contextWindow":128_000}),
            ),
        },
        None,
    )
}

async fn command(worker: &Worker, kind: &str, payload: Value) -> Value {
    let response = tokio::time::timeout(Duration::from_secs(60), worker.dispatch(kind, &payload))
        .await
        .expect("worker command deadline");
    assert!(response.success, "{kind}: {response:?}");
    response.data.unwrap_or(Value::Null)
}

#[test]
fn clm_worker_python_edit_is_durable_and_reaches_the_next_provider_request() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
    let dir = tempfile::Builder::new().prefix("clm-public-tracer-").tempdir().unwrap().keep();
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("settings.json"), r#"{"compaction":{"enabled":false},"retry":{"enabled":false}}"#).unwrap();
    let worker = worker(&dir, "clm-tracer");
    command(&worker, "create", json!({"cwd":dir,"sessionDir":dir.join("sessions")})).await;
    for _ in 0..2 {
        command(&worker, "prompt_and_wait", json!({"message":"FORGOTTEN_SENTINEL"})).await;
    }
    let path = worker.core.lock().unwrap().store.as_ref().unwrap().path.clone();
    let before = SessionFile::open(&path).unwrap();
    let ids: Vec<_> = before.entries().iter().filter(|row| row.fields["message"]["role"] == "user")
        .map(|row|row.id.clone()).collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "identical deliveries have authoritative distinct IDs");
    assert_eq!(before.entries().iter().filter(|row|row.fields["customType"]=="clm_usage_v1" && row.fields["data"]["pin"]["mode"]=="off").count(),2,"off-mode dispatches retain their original usage pins");
    println!("tracer_session={} delivery_ids={ids:?}", path.display());
    let on = command(&worker, "set_clm_mode", json!({"enabled":true})).await;
    assert_eq!(on["revision"], 0);
    let context_path = on["path"].as_str().expect("absolute CLM path");
    assert!(std::path::Path::new(context_path).is_absolute());
    // Await a real filesystem edit in the public Python cell. The active delivery
    // is protected; only the two already settled, identical deliveries are removed.
    let code = format!(r"import asyncio, pathlib, re
p = pathlib.Path({})
async def shorten():
    text = await asyncio.to_thread(p.read_text)
    edited = re.sub(r'\[\[CTX_TURN id=(?:{}|{}) role=user\]\]\n.*?\n\[\[CTX_END\]\]\n', '', text, flags=re.S)
    assert edited != text
    await asyncio.to_thread(p.write_text, edited)
await shorten()
print('edit awaited')", serde_json::to_string(context_path).unwrap(), ids[0], ids[1]);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    worker.agent_engine.as_ref().unwrap().provider_target.write().unwrap().as_mut().unwrap().model.api = provider.api.clone();
    let tool = pa_ai::faux::faux_assistant_message(vec![pa_ai::faux::faux_tool_call(
        "python_repl", &json!({"code":code}), Some("clm-awaited-edit"))],
        FauxAssistantMessageOptions {stop_reason:Some(pa_types::ai::StopReason::ToolUse), ..Default::default()});
    provider.set_responses(vec![FauxResponseStep::Message(tool), FauxResponseStep::Factory({
        let requests = requests.clone();
        Arc::new(move |context, _, _, _| {
            requests.lock().unwrap().push(serde_json::to_value(context).unwrap());
            Ok(faux_assistant_text_message("shortened", pa_ai::faux::FauxAssistantMessageOptions::default()))
        })
    })]);
    command(&worker, "prompt_and_wait", json!({"message":"shorten settled history"})).await;
    let observed = requests.lock().unwrap().clone();
    assert_eq!(observed.len(), 1, "post-tool provider request must arrive");
    assert!(!observed[0].to_string().contains("FORGOTTEN_SENTINEL"), "removed audit leaked: {}", observed[0]);
    let status = command(&worker, "get_clm_status", json!({})).await;
    assert_eq!(status["revision"], 1);
    let durable = SessionFile::open(&path).unwrap();
    assert!(std::fs::read_to_string(&path).unwrap().contains("FORGOTTEN_SENTINEL"), "audit retained");
    assert!(!serde_json::to_string(&durable.model_context_checked().unwrap()).unwrap().contains("FORGOTTEN_SENTINEL"));
    let receipts: Vec<_> = durable.entries().iter().filter(|row| row.fields["customType"].as_str().is_some_and(|kind| kind.starts_with("clm_usage_")) && row.fields["data"]["pin"]["mode"]=="on").collect();
    assert_eq!(receipts.len(), 2, "one provider receipt per actual attempt");
    assert_eq!(receipts[0].fields["data"]["pin"]["revision"], 0);
    assert_eq!(receipts[1].fields["data"]["pin"]["revision"], 1);
    for receipt in receipts {
        let id = receipt.fields["data"]["messageEntryId"].as_str().expect("canonical assistant id");
        assert!(durable.entries().iter().any(|row| row.id == id && row.fields["message"]["role"] == "assistant"));
    }
    std::fs::write(dir.join("receiver-request.json"), serde_json::to_vec_pretty(&observed[0]).unwrap()).unwrap();
    println!("post_edit_status={status} receiver_request_file={}", dir.join("receiver-request.json").display());
    let off = command(&worker, "set_clm_mode", json!({"enabled":false})).await;
    let built = worker.agent_engine.as_ref().unwrap().session.lock().await.clone().unwrap();
    let parent_before = serde_json::to_value(built.session.agent().state().await.messages).unwrap();
    assert!(parent_before.to_string().contains("FORGOTTEN_SENTINEL"), "audit-backed live state remains distinct from accepted projection");
    let journal_before = std::fs::read(&path).unwrap();
    let (side_sent, side_received) = tokio::sync::oneshot::channel();
    let side_sent = Arc::new(Mutex::new(Some(side_sent)));
    provider.set_responses(vec![FauxResponseStep::Factory(Arc::new(move |context, _, _, _| {
        if let Some(sent) = side_sent.lock().unwrap().take() {
            let _ = sent.send(serde_json::to_string(context).unwrap());
        }
        Ok(faux_assistant_text_message("side answer", FauxAssistantMessageOptions::default()))
    }))]);
    let mut side_events = worker.events.subscribe();
    command(&worker, "start_side_question", json!({
        "sideQuestionId":"post-clm-edit", "question":"Follow up on accepted context",
        "previousTurns":[{"question":"Earlier side question", "answer":"Earlier side answer"}]
    })).await;
    let side_request = tokio::time::timeout(Duration::from_secs(10), side_received).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = side_events.recv().await.unwrap();
            let event: Value = serde_json::from_slice(&frame.payload).unwrap();
            if event["type"] == "side_question_event" && event["event"]["id"] == "post-clm-edit" && event["event"]["status"] != "running" {
                assert_eq!(event["event"]["status"], "complete", "{event}");
                break;
            }
        }
    }).await.unwrap();
    assert!(!side_request.contains("FORGOTTEN_SENTINEL"), "side question leaked removed audit: {side_request}");
    assert!(side_request.contains("shorten settled history"));
    assert!(side_request.contains("Earlier side question") && side_request.contains("Earlier side answer"));
    assert!(side_request.contains("Follow up on accepted context"));
    assert_eq!(serde_json::to_value(built.session.agent().state().await.messages).unwrap(), parent_before);
    assert_eq!(std::fs::read(&path).unwrap(), journal_before, "side thread does not change parent audit");
    worker.engine.teardown_for_replacement().await;
    worker.core.lock().unwrap().shutdown_requested = true;
    worker.work_notify.notify_one();
    worker.core.lock().unwrap().store.take();
    let reopened = self::worker(&dir, "clm-reopened");
    command(&reopened, "create", json!({"cwd":dir,"sessionPath":path,"provider":"faux","model":"faux-1"})).await;
    let view = command(&reopened, "get_clm_model_snapshot", json!({})).await;
    assert_eq!(view["revision"], off["revision"]);
    assert!(!view.to_string().contains("FORGOTTEN_SENTINEL"));
    reopened.engine.teardown_for_replacement().await;
    reopened.core.lock().unwrap().shutdown_requested = true;
    reopened.work_notify.notify_one();
    provider.unregister();
    });
}

#[test]
fn clm_worker_rejects_actual_target_budget_before_provider_dispatch() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = tempfile::Builder::new()
            .prefix("clm-budget-")
            .tempdir()
            .unwrap()
            .keep();
        let worker = worker(&dir, "clm-budget");
        command(&worker, "create", json!({"cwd":dir})).await;
        command(&worker, "set_clm_mode", json!({"enabled":true})).await;
        let engine = worker.agent_engine.as_ref().unwrap();
        {
            let mut target = engine.provider_target.write().unwrap();
            let model = &mut target.as_mut().unwrap().model;
            model.context_window = 64;
            model.max_tokens = 8;
        }
        let provider = pa_ai::faux::register_faux_provider(
            pa_ai::faux::RegisterFauxProviderOptions::default(),
        );
        engine
            .provider_target
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .model
            .api = provider.api.clone();
        provider.set_responses(vec![FauxResponseStep::Message(
            faux_assistant_text_message(
                "must not run",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ),
        )]);
        let _result = worker
            .dispatch("prompt_and_wait", &json!({"message":"budget guarded"}))
            .await;
        assert_eq!(
            provider.call_count(),
            0,
            "effective target budget rejects before dispatch"
        );
        let status = command(&worker, "get_clm_status", json!({})).await;
        assert_eq!(status["estimatedOccupancy"]["maxInputTokens"], 56);
        assert!(
            status["estimatedOccupancy"]["estimatedInputTokens"]
                .as_u64()
                .unwrap()
                > 56
        );
        worker.engine.teardown_for_replacement().await;
        worker.core.lock().unwrap().shutdown_requested = true;
        worker.work_notify.notify_one();
        provider.unregister();
    });
}

#[test]
fn clm_worker_failed_provider_cleanup_retains_audit_and_dispatch_pin() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = tempfile::Builder::new()
            .prefix("clm-cleanup-")
            .tempdir()
            .unwrap()
            .keep();
        std::fs::create_dir_all(dir.join("agent")).unwrap();
        std::fs::write(
            dir.join("agent/settings.json"),
            r#"{"compaction":{"enabled":false},"retry":{"enabled":false}}"#,
        )
        .unwrap();
        let worker = worker(&dir, "clm-cleanup");
        command(&worker, "create", json!({"cwd":dir})).await;
        command(&worker, "set_clm_mode", json!({"enabled":true})).await;
        let provider = pa_ai::faux::register_faux_provider(
            pa_ai::faux::RegisterFauxProviderOptions::default(),
        );
        worker
            .agent_engine
            .as_ref()
            .unwrap()
            .provider_target
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .model
            .api = provider.api.clone();
        let mut failed = faux_assistant_text_message(
            "FAILED_PROVIDER_AUDIT",
            FauxAssistantMessageOptions {
                stop_reason: Some(pa_types::ai::StopReason::Error),
                error_message: Some("synthetic failure".into()),
                ..Default::default()
            },
        );
        failed.usage.input = 23;
        failed.usage.total_tokens = 23;
        provider.set_responses(vec![FauxResponseStep::Message(failed)]);
        let _ = worker
            .dispatch(
                "prompt_and_wait",
                &json!({"message":"owned cleanup delivery"}),
            )
            .await;
        let status = command(&worker, "get_clm_status", json!({})).await;
        assert_eq!(
            status["revision"], 1,
            "failed provider cleanup commits a real revision"
        );
        let path = worker
            .core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .path
            .clone();
        let durable = SessionFile::open(&path).unwrap();
        let failed = durable
            .entries()
            .iter()
            .find(|row| row.fields["message"]["stopReason"] == "error")
            .unwrap();
        let receipt = durable
            .entries()
            .iter()
            .find(|row| row.fields["customType"] == "clm_usage_v1")
            .unwrap();
        assert_eq!(receipt.fields["data"]["pin"]["revision"], 0);
        assert_eq!(receipt.fields["data"]["messageEntryId"], failed.id);
        assert_eq!(
            receipt.fields["data"]["usage"],
            failed.fields["message"]["usage"]
        );
        let observed = Arc::new(Mutex::new(None));
        provider.set_responses(vec![FauxResponseStep::Factory({
            let observed = observed.clone();
            Arc::new(move |context, _, _, _| {
                *observed.lock().unwrap() = Some(serde_json::to_value(context).unwrap());
                Ok(faux_assistant_text_message(
                    "recovered",
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ))
            })
        })]);
        command(
            &worker,
            "prompt_and_wait",
            json!({"message":"continue after cleanup"}),
        )
        .await;
        let request = observed.lock().unwrap().clone().unwrap().to_string();
        assert!(!request.contains("FAILED_PROVIDER_AUDIT"));
        assert!(request.contains("owned cleanup delivery"));
        assert!(std::fs::read_to_string(path)
            .unwrap()
            .contains("FAILED_PROVIDER_AUDIT"));
        worker.engine.teardown_for_replacement().await;
        worker.core.lock().unwrap().shutdown_requested = true;
        worker.work_notify.notify_one();
        provider.unregister();
    });
}

#[test]
fn clm_worker_controls_block_context_replacement_before_side_effects() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = tempfile::Builder::new()
            .prefix("clm-controls-")
            .tempdir()
            .unwrap()
            .keep();
        let worker = worker(&dir, "clm-controls");
        command(&worker, "create", json!({"cwd":dir})).await;
        let on = command(&worker, "set_clm_mode", json!({"enabled":true})).await;
        let before = worker
            .core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .path
            .clone();
        for kind in [
            "compact",
            "navigate_tree",
            "fork",
            "start_side_question",
            "new_session",
            "switch_session",
            "import_jsonl",
        ] {
            let response = worker.dispatch(kind, &json!({})).await;
            assert!(
                !response.success
                    && response
                        .error
                        .as_deref()
                        .is_some_and(|error| error.contains("CLM")),
                "{kind}: {response:?}"
            );
            assert_eq!(
                worker.core.lock().unwrap().store.as_ref().unwrap().path,
                before
            );
        }
        let off = command(&worker, "set_clm_mode", json!({"enabled":false})).await;
        assert_eq!(off["mode"], "off");
        let frozen = command(&worker, "get_clm_model_snapshot", json!({})).await;
        assert_eq!(frozen["revision"], off["revision"]);
        let on2 = command(&worker, "set_clm_mode", json!({"enabled":true})).await;
        assert_ne!(on2["epochId"], on["epochId"]);
        let old_epoch = worker
            .dispatch(
                "restore_clm_revision",
                &json!({"epochId":on["epochId"],"revision":0}),
            )
            .await;
        assert!(!old_epoch.success);
        assert!(old_epoch.error.unwrap().contains("active epoch"));
        let unchanged = command(&worker, "get_clm_model_snapshot", json!({})).await;
        assert_eq!(unchanged["epochId"], on2["epochId"]);
        assert_eq!(unchanged["revision"], on2["revision"]);
        let restored = command(
            &worker,
            "restore_clm_revision",
            json!({"epochId":on2["epochId"],"revision":0}),
        )
        .await;
        assert_eq!(restored["revision"], 1);
        worker
            .core
            .lock()
            .unwrap()
            .store
            .as_mut()
            .unwrap()
            .clm_error = Some("synthetic uncertain owner write".into());
        let unavailable = worker.dispatch("get_session_context", &json!({})).await;
        assert!(
            !unavailable.success,
            "model reads must propagate the owner's uncertainty"
        );
        assert!(unavailable
            .error
            .unwrap()
            .contains("synthetic uncertain owner write"));
        worker.engine.teardown_for_replacement().await;
        worker.core.lock().unwrap().shutdown_requested = true;
        worker.work_notify.notify_one();
    });
}

#[test]
fn clm_worker_custom_delivery_uses_canonical_owner_identity_and_protection() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir=tempfile::Builder::new().prefix("clm-custom-").tempdir().unwrap().keep();
        let worker=worker(&dir,"clm-custom");
        command(&worker,"create",json!({"cwd":dir,"provider":"faux","model":"faux-1"})).await;
        let status=command(&worker,"set_clm_mode",json!({"enabled":true})).await;
        let log=worker.core.lock().unwrap().store.as_ref().unwrap().path.clone();
        let code=format!("from pathlib import Path\nimport json\nrows=[json.loads(line) for line in Path({}).read_text().splitlines()]\nrow=next(row for row in rows if row.get('customType')=='synthetic_delivery')\ntext=Path({}).read_text()\nassert '[[CTX_PROTECTED id='+row['id']+']]' in text\nprint('delivery-protected')",serde_json::to_string(&log).unwrap(),status["path"]);
        let provider=pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
        worker.agent_engine.as_ref().unwrap().provider_target.write().unwrap().as_mut().unwrap().model.api=provider.api.clone();
        provider.set_responses(vec![
            pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_message(
                vec![pa_ai::faux::faux_tool_call("python_repl",&json!({"code":code}),Some("inspect-custom"))],
                pa_ai::faux::FauxAssistantMessageOptions {stop_reason:Some(pa_types::ai::StopReason::ToolUse),..Default::default()})),
            pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message("done",pa_ai::faux::FauxAssistantMessageOptions::default()))]);
        command(&worker,"prompt_and_wait",json!({"message":"injected","customMessage":{"role":"custom","customType":"synthetic_delivery","content":"injected","display":true,"timestamp":1}})).await;
        let store=SessionFile::open(&log).unwrap();
        let result=store.entries().iter().find(|row|row.fields["message"]["role"]=="toolResult").expect("Python result persisted");
        assert_eq!(result.fields["message"]["isError"],false);
        assert!(result.fields["message"].to_string().contains("delivery-protected"),"custom delivery must bind to its durable protected row: {}",result.fields["message"]);
        worker.engine.teardown_for_replacement().await;
        worker.core.lock().unwrap().shutdown_requested=true;
        worker.work_notify.notify_one();
        provider.unregister();
    });
}

#[test]
fn clm_worker_post_off_branch_summary_rejects_before_exposing_historical_audit() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = tempfile::Builder::new()
            .prefix("clm-branch-guard-")
            .tempdir()
            .unwrap()
            .keep();
        let worker = worker(&dir, "clm-branch-guard");
        command(&worker, "create", json!({"cwd":dir})).await;
        command(
            &worker,
            "prompt_and_wait",
            json!({"message":"older branch secret"}),
        )
        .await;
        command(
            &worker,
            "prompt_and_wait",
            json!({"message":"newer branch work"}),
        )
        .await;
        let target = worker
            .core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .entries()
            .iter()
            .find(|entry| entry.fields["message"]["role"] == "user")
            .unwrap()
            .id
            .clone();
        command(&worker, "set_clm_mode", json!({"enabled":true})).await;
        command(&worker, "set_clm_mode", json!({"enabled":false})).await;
        let (path, leaf) = {
            let core = worker.core.lock().unwrap();
            let store = core.store.as_ref().unwrap();
            (store.path.clone(), store.leaf_id().map(str::to_owned))
        };
        let before = std::fs::read(&path).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = pa_ai::faux::register_faux_provider(
            pa_ai::faux::RegisterFauxProviderOptions::default(),
        );
        worker
            .agent_engine
            .as_ref()
            .unwrap()
            .provider_target
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .model
            .api = provider.api.clone();
        provider.set_responses(vec![FauxResponseStep::Factory(Arc::new({
            let calls = calls.clone();
            move |_, _, _, _| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(faux_assistant_text_message(
                    "historical branch summary",
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ))
            }
        }))]);
        let response = worker
            .dispatch(
                "navigate_tree",
                &json!({"targetId":target,"summarize":true}),
            )
            .await;
        assert!(
            !response.success
                && response
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("historical branch preparation")),
            "{response:?}"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            worker
                .core
                .lock()
                .unwrap()
                .store
                .as_ref()
                .unwrap()
                .leaf_id()
                .map(str::to_owned),
            leaf
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
        worker.engine.teardown_for_replacement().await;
        worker.core.lock().unwrap().shutdown_requested = true;
        worker.work_notify.notify_one();
        provider.unregister();
    });
}

#[test]
fn clm_worker_create_preserves_explicit_inherited_mode_and_artifact_isolation() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"clm":{"enabled":true},"compaction":{"enabled":false}}"#,
        )
        .unwrap();
        let off = worker(dir.path(), "inherited-off");
        command(
            &off,
            "create",
            json!({"cwd":dir.path(),"sessionDir":dir.path().join("off"),"inheritedClmMode":false}),
        )
        .await;
        let off_status = command(&off, "get_clm_status", json!({})).await;
        assert_eq!(off_status["mode"], "off");
        assert_eq!(off_status["modeSource"], "inherited-child");
        let first = worker(dir.path(), "inherited-one");
        let second = worker(dir.path(), "inherited-two");
        command(
            &first,
            "create",
            json!({"cwd":dir.path(),"sessionDir":dir.path().join("one"),"inheritedClmMode":true}),
        )
        .await;
        command(
            &second,
            "create",
            json!({"cwd":dir.path(),"sessionDir":dir.path().join("two"),"inheritedClmMode":true}),
        )
        .await;
        let first_status = command(&first, "get_clm_status", json!({})).await;
        let second_status = command(&second, "get_clm_status", json!({})).await;
        assert_eq!(first_status["mode"], "on");
        assert_eq!(first_status["modeSource"], "inherited-child");
        assert_ne!(first_status["epochId"], second_status["epochId"]);
        assert_ne!(first_status["path"], second_status["path"]);
        let first_path = std::path::Path::new(first_status["path"].as_str().unwrap());
        let second_path = std::path::Path::new(second_status["path"].as_str().unwrap());
        assert!(first_path.exists());
        assert!(second_path.exists());
        command(&first, "set_clm_mode", json!({"enabled":false})).await;
        assert_eq!(
            command(&second, "get_clm_status", json!({})).await["mode"],
            "on"
        );
    });
}

#[test]
fn clm_worker_refinement_receipt_precedes_invalid_plan_failure_and_reaches_mirror() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir=tempfile::tempdir().unwrap();
        let worker=worker(dir.path(),"helper-receipts");
        command(&worker,"create",json!({"cwd":dir.path(),"sessionDir":dir.path().join("sessions")})).await;
        for enabled in [true,false] {
            command(&worker,"set_clm_mode",json!({"enabled":enabled})).await;
            let response=tokio::time::timeout(Duration::from_secs(15),worker.dispatch("refine",&json!({}))).await.expect("helper acknowledgment must not deadlock");
            assert!(!response.success,"scripted plain text is not a refinement plan");
        }
        let durable={
            let core=worker.core.lock().unwrap();
            core.store.as_ref().unwrap().branch_file_entries_checked().unwrap()
        };
        let receipts:Vec<_>=durable.iter().filter(|entry|matches!(entry,pa_types::session::FileEntry::Custom {payload,..} if payload.custom_type=="clm_usage_v1")).collect();
        assert_eq!(receipts.len(),2,"observed responses persist before invalid-plan failure");
        let built=worker.agent_engine.as_ref().unwrap().session.lock().await.clone().unwrap();
        let mirror=built.session.entries().await;
        for receipt in receipts {
            assert!(mirror.iter().any(|row| row==receipt),"mirror has the exact authoritative receipt");
        }
        worker.engine.teardown_for_replacement().await;
    });
}

#[test]
fn clm_pre_epoch_branch_summary_records_original_pin_before_navigation() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let worker = worker(dir.path(), "branch-helper");
        command(
            &worker,
            "create",
            json!({"cwd":dir.path(),"sessionDir":dir.path().join("sessions")}),
        )
        .await;
        command(
            &worker,
            "prompt_and_wait",
            json!({"message":"first ordinary turn"}),
        )
        .await;
        command(
            &worker,
            "prompt_and_wait",
            json!({"message":"second ordinary turn"}),
        )
        .await;
        let (target, old_leaf) = {
            let core = worker.core.lock().unwrap();
            let store = core.store.as_ref().unwrap();
            (
                store
                    .entries()
                    .iter()
                    .find(|entry| entry.fields["message"]["role"] == "user")
                    .unwrap()
                    .id
                    .clone(),
                store.leaf_id().unwrap().to_owned(),
            )
        };
        command(
            &worker,
            "navigate_tree",
            json!({"targetId":target,"summarize":true}),
        )
        .await;
        {
            let core = worker.core.lock().unwrap();
            let store = core.store.as_ref().unwrap();
            let receipts: Vec<_> = store
                .entries()
                .iter()
                .filter(|row| row.fields["customType"] == "clm_usage_v1")
                .collect();
            assert_eq!(
                receipts.len(),
                3,
                "two turn receipts plus one observed branch-summary response"
            );
            assert_eq!(receipts[2].fields["data"]["pin"]["branchId"], old_leaf);
            assert_eq!(receipts[2].fields["data"]["pin"]["mode"], "off");
            assert!(receipts[2].fields["data"].get("messageEntryId").is_none());
            assert!(store
                .entries()
                .iter()
                .any(|row| row.type_ == "branch_summary"));
        }
        worker.engine.teardown_for_replacement().await;
    });
}

#[test]
fn clm_off_worker_allows_explicit_pre_epoch_fork_new_switch_and_side_question() {
    let _faux = crate::agent_engine::FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let worker = worker(dir.path(), "pre-epoch-lifecycle");
        command(
            &worker,
            "create",
            json!({"cwd":dir.path(),"sessionDir":dir.path().join("sessions")}),
        )
        .await;
        command(
            &worker,
            "prompt_and_wait",
            json!({"message":"ordinary baseline"}),
        )
        .await;
        let (original, target) = {
            let core = worker.core.lock().unwrap();
            let store = core.store.as_ref().unwrap();
            (
                store.path.clone(),
                store
                    .entries()
                    .iter()
                    .find(|row| row.fields["message"]["role"] == "user")
                    .unwrap()
                    .id
                    .clone(),
            )
        };
        command(&worker, "set_clm_mode", json!({"enabled":true})).await;
        command(&worker, "set_clm_mode", json!({"enabled":false})).await;
        command(&worker, "fork", json!({"entryId":target,"position":"at"})).await;
        let fork = worker
            .core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .path
            .clone();
        assert_ne!(fork, original);
        let status = command(&worker, "get_clm_status", json!({})).await;
        assert_eq!(status["mode"], "off");
        assert!(
            status["epochId"].is_null(),
            "explicit pre-epoch fork does not invent an accepted epoch"
        );
        command(&worker, "new_session", json!({})).await;
        assert_ne!(
            worker.core.lock().unwrap().store.as_ref().unwrap().path,
            fork
        );
        command(&worker, "switch_session", json!({"sessionPath":original})).await;
        assert_eq!(
            worker.core.lock().unwrap().store.as_ref().unwrap().path,
            original
        );
        let status = command(&worker, "get_clm_status", json!({})).await;
        assert_eq!(status["mode"], "off");
        let provider = pa_ai::faux::register_faux_provider(
            pa_ai::faux::RegisterFauxProviderOptions::default(),
        );
        worker
            .agent_engine
            .as_ref()
            .unwrap()
            .provider_target
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .model
            .api = provider.api.clone();
        let (sent, received) = tokio::sync::oneshot::channel();
        let sent = Arc::new(std::sync::Mutex::new(Some(sent)));
        provider.set_responses(vec![FauxResponseStep::Factory(Arc::new(
            move |context, _, _, _| {
                if let Some(sent) = sent.lock().unwrap().take() {
                    let _ = sent.send(serde_json::to_string(context).unwrap());
                }
                Ok(faux_assistant_text_message(
                    "side answer",
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ))
            },
        ))]);
        command(
            &worker,
            "start_side_question",
            json!({"sideQuestionId":"side-proof","question":"What did we discuss?"}),
        )
        .await;
        let request = tokio::time::timeout(Duration::from_secs(10), received)
            .await
            .unwrap()
            .unwrap();
        assert!(
            request.contains("ordinary baseline"),
            "off side question receives accepted conversation context"
        );
        worker.engine.teardown_for_replacement().await;
        worker.core.lock().unwrap().shutdown_requested = true;
        worker.work_notify.notify_one();
        provider.unregister();
    });
}
