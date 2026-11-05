use super::*;

async fn configured_default_leaves_prior_session_off(loaded: bool, seeded: bool) {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"clm":{"enabled":true}}"#,
    )
    .unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let mut owner = SessionManager::persisted(dir.path(), &sessions);
    owner
        .append_model_change(&model.provider, &model.id)
        .unwrap();
    if seeded {
        owner
            .append_message(
                serde_json::from_value(serde_json::json!({
                    "role":"user", "content":"ordinary history without CLM consent", "timestamp":1
                }))
                .unwrap(),
            )
            .unwrap();
    }
    owner.flush_now().unwrap();
    let path = owner.get_session_file().unwrap().to_path_buf();
    if loaded {
        owner = SessionManager::open(dir.path(), &sessions, &path);
    }
    let before = std::fs::read(&path).unwrap();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir,
        session_manager: Some(owner),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None, model,
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let status = engine
        .session
        .clm
        .as_ref()
        .unwrap()
        .status(true)
        .await
        .unwrap();
    assert_eq!(
        status.mode,
        ClmMode::Off,
        "loaded={loaded}, seeded={seeded}"
    );
    assert!(status.epoch_id.is_none());
    let after = std::fs::read(&path).unwrap();
    assert!(
        after.starts_with(&before),
        "default must not rewrite ordinary history"
    );
    assert!(!crate::session::clm::contains_clm_records(&after));
    provider.unregister();
}

#[tokio::test]
async fn clm_configured_default_leaves_loaded_history_off() {
    configured_default_leaves_prior_session_off(true, true).await;
}

#[tokio::test]
async fn clm_configured_default_leaves_loaded_empty_session_off() {
    configured_default_leaves_prior_session_off(true, false).await;
}

#[tokio::test]
async fn clm_configured_default_leaves_seeded_session_off() {
    configured_default_leaves_prior_session_off(false, true).await;
}

#[tokio::test]
async fn clm_standalone_manager_protects_deliveries_and_records_provider_receipts() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::{
        engine::{create_session, SessionEngineConfig},
        PromptOptions,
    };
    use pa_ai::faux::{FauxAssistantMessageOptions, FauxResponseStep};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let observed = Arc::new(Mutex::new(None));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let path = Arc::new(Mutex::new(None::<std::path::PathBuf>));
    let tool = crate::session_engine::tool_bridge::bridge_tool(
        crate::tools::tool_definition::ToolDefinition {
            name: "inspect".into(),
            label: "inspect".into(),
            description: "inspect CLM".into(),
            prompt_snippet: String::new(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
            execution_mode: None,
            prepare_arguments: None,
            execute: Arc::new({
                let observed = observed.clone();
                let path = path.clone();
                let calls = calls.clone();
                move |_, _, _, _| {
                    let observed = observed.clone();
                    let path = path.clone();
                    let calls = calls.clone();
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let text = std::fs::read_to_string(path.lock().unwrap().as_ref().unwrap())?;
                        *observed.lock().unwrap() = Some(text);
                        Ok(crate::tools::tool_definition::ToolExecutionResult::text(
                            "inspected",
                        ))
                    })
                }
            }),
        },
    );
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        model_info: Some(model.clone()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None, model,
        )),
        session_manager: Some(SessionManager::persisted(
            dir.path(),
            &dir.path().join("sessions"),
        )),
        tools: vec![tool],
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    let status = runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    *path.lock().unwrap() = Some(std::path::PathBuf::from(status.path));
    provider.set_responses(vec![
        FauxResponseStep::Message(pa_ai::faux::faux_assistant_message(
            vec![
                pa_ai::faux::faux_tool_call("inspect", &serde_json::json!({}), Some("inspect-1")),
                pa_ai::faux::faux_tool_call("inspect", &serde_json::json!({}), Some("inspect-2")),
            ],
            FauxAssistantMessageOptions {
                stop_reason: Some(pa_types::ai::StopReason::ToolUse),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            "done",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        )),
    ]);
    engine
        .prompt(
            "identical",
            PromptOptions {
                batch: vec![crate::session_engine::PromptBatchRow {
                    text: "identical".into(),
                    images: vec![],
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "CLM serializes all calls in the provider tool group"
    );
    let entries = engine.session.entries().await;
    let users: Vec<_> = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                pa_types::session::FileEntry::Message {
                    message: pa_types::session::AgentMessage::User(_),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(users.len(), 2);
    assert_ne!(users[0].id(), users[1].id());
    let text = observed
        .lock()
        .unwrap()
        .clone()
        .expect("tool read the mirror");
    for entry in users {
        assert!(
            text.contains(&format!("[[CTX_PROTECTED id={}]]", entry.id().unwrap())),
            "each durable delivery is protected"
        );
    }
    assert_eq!(entries.iter().filter(|entry| matches!(entry,pa_types::session::FileEntry::Custom{payload,..} if payload.custom_type.starts_with("clm_usage_"))).count(),2);
    provider.unregister();
}

#[tokio::test]
async fn clm_configured_default_is_materialized_once_and_reload_does_not_switch_mode() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("settings.json"), r#"{"clm":{"enabled":true}}"#).unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: agent.clone(),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None, model,
        )),
        session_manager: Some(SessionManager::persisted(
            dir.path(),
            &dir.path().join("sessions"),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    let status = runtime.status(true).await.unwrap();
    assert_eq!(status.mode, ClmMode::On);
    assert_eq!(
        status.mode_source,
        pa_types::clm::ClmStatusSource::ConfiguredDefault
    );
    std::fs::write(agent.join("settings.json"), r#"{"clm":{"enabled":false}}"#).unwrap();
    assert_eq!(
        runtime.status(false).await.unwrap().epoch_id,
        status.epoch_id
    );
    assert_eq!(runtime.status(false).await.unwrap().mode, ClmMode::On);
    provider.unregister();
}

#[tokio::test]
async fn clm_request_reminder_uses_actual_budget_without_blocking_a_valid_request() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let mut model = provider.models[0].clone();
    model.context_window = 1100;
    model.max_tokens = 100;
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None,
            model.clone(),
        )),
        session_manager: Some(SessionManager::persisted(
            dir.path(),
            &dir.path().join("sessions"),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let response = pa_ai::faux::FauxResponseStep::Factory(Arc::new({
        let prompts = prompts.clone();
        move |context, _, _, _| {
            prompts.lock().unwrap().push(context.system_prompt.clone());
            Ok(pa_ai::faux::faux_assistant_text_message(
                "done",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        }
    }));
    provider.set_responses(vec![
        response.clone(),
        response.clone(),
        response.clone(),
        response,
    ]);
    for (characters, warning) in [(3040, true), (3996, false), (2996, false)] {
        let prompt = "x".repeat(characters);
        let context = pa_agent::stream::LlmContext {
            system_prompt: Some(prompt.clone()),
            ..Default::default()
        };
        REQUEST_CLM
            .scope(runtime.clone(), async {
                let mut stream = crate::session_engine::provider_adapter::stream_once(
                    &model,
                    None,
                    None,
                    None,
                    context,
                    pa_agent::stream::StreamRequestOptions::default(),
                )
                .unwrap();
                stream.result().await.unwrap();
            })
            .await;
        let sent = prompts.lock().unwrap().last().unwrap().clone().unwrap();
        assert_eq!(sent.contains("Aim below 60%"), warning);
        assert!(sent.starts_with(&prompt));
        let occupancy = runtime
            .status(false)
            .await
            .unwrap()
            .estimated_occupancy
            .unwrap();
        assert_eq!(
            occupancy.estimated_input_tokens,
            (sent.len() as u64).div_ceil(4)
        );
        assert_eq!(occupancy.max_input_tokens, Some(1000));
    }
    let before = prompts.lock().unwrap().len();
    REQUEST_CLM
        .scope(runtime.clone(), async {
            let context = pa_agent::stream::LlmContext {
                system_prompt: Some("x".repeat(4004)),
                ..Default::default()
            };
            assert!(crate::session_engine::provider_adapter::stream_once(
                &model,
                None,
                None,
                None,
                context,
                pa_agent::stream::StreamRequestOptions::default()
            )
            .is_err());
        })
        .await;
    assert_eq!(prompts.lock().unwrap().len(), before);
    assert_eq!(
        runtime
            .status(false)
            .await
            .unwrap()
            .estimated_occupancy
            .unwrap()
            .estimated_input_tokens,
        1001
    );
    provider.unregister();
}

#[tokio::test]
async fn clm_aborted_provider_does_not_manufacture_a_usage_receipt() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None, model,
        )),
        session_manager: Some(SessionManager::persisted(
            dir.path(),
            &dir.path().join("sessions"),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    engine
        .session
        .clm
        .as_ref()
        .unwrap()
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    provider.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "aborted",
            pa_ai::faux::FauxAssistantMessageOptions {
                stop_reason: Some(pa_types::ai::StopReason::Aborted),
                ..Default::default()
            },
        ),
    )]);
    let _ = engine
        .prompt("first", crate::session_engine::PromptOptions::default())
        .await;
    let entries = engine.session.entries().await;
    assert!(entries.iter().any(|row|matches!(row,pa_types::session::FileEntry::Message {message:pa_types::session::AgentMessage::Assistant(message),..} if message.stop_reason==pa_types::ai::StopReason::Aborted)));
    assert_eq!(entries.iter().filter(|row|matches!(row,pa_types::session::FileEntry::Custom {payload,..} if payload.custom_type.starts_with("clm_usage_"))).count(),0);
    provider.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "done",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    engine
        .prompt("second", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    let entries = engine.session.entries().await;
    let receipts:Vec<_>=entries.iter().filter(|row|matches!(row,pa_types::session::FileEntry::Custom {payload,..} if payload.custom_type.starts_with("clm_usage_"))).collect();
    assert_eq!(receipts.len(), 1);
    provider.unregister();
}

#[tokio::test]
async fn clm_guidance_names_the_current_working_file_and_leaves_off_requests_unchanged() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let response = pa_ai::faux::FauxResponseStep::Factory(Arc::new({
        let sent = sent.clone();
        move |context, _, _, _| {
            sent.lock()
                .unwrap()
                .push(context.system_prompt.clone().unwrap_or_default());
            Ok(pa_ai::faux::faux_assistant_text_message(
                "done",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        }
    }));
    provider.set_responses(vec![
        response.clone(),
        response.clone(),
        response.clone(),
        response,
    ]);
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None, model,
        )),
        session_manager: Some(SessionManager::persisted(
            dir.path(),
            &dir.path().join("sessions"),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    engine
        .prompt("off", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    let status = runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    engine
        .prompt("on", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    engine
        .prompt("still on", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    runtime
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    engine
        .prompt("off again", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    let prompts = sent.lock().unwrap();
    assert!(!prompts[0].contains("CLM working context:"));
    for prompt in &prompts[1..3] {
        assert!(prompt.contains(&format!("CLM working context: {}", status.path)));
        assert!(prompt
            .contains("Edit this file only within one awaited tool action. Use an atomic save."));
        assert!(prompt.contains("Preserve every CTX_PROTECTED reference."));
        assert_eq!(prompt.matches("CLM working context:").count(), 1);
    }
    assert!(!prompts[3].contains("CLM working context:"));
    provider.unregister();
}

#[tokio::test]
async fn clm_refinement_planner_reads_accepted_context_on_and_after_off() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::{
        engine::{create_session, SessionEngineConfig},
        refine::{RefineOptions, RefinementSource},
    };
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let mut owner = SessionManager::persisted(dir.path(), &dir.path().join("sessions"));
    owner.append_message(serde_json::from_value(serde_json::json!({"role":"user","content":"FORGOTTEN_REFINEMENT_AUDIT","timestamp":1})).unwrap()).unwrap();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        session_manager: Some(owner),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None,
            model.clone(),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    let status = runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    runtime.controller.prepare_tool("edit").await.unwrap();
    let working = std::fs::read_to_string(&status.path).unwrap();
    std::fs::write(
        &status.path,
        working.replace("FORGOTTEN_REFINEMENT_AUDIT", "ACCEPTED_REFINEMENT_CONTEXT"),
    )
    .unwrap();
    runtime
        .controller
        .settle_tool(
            "edit",
            ClmToolOutcome::Completed,
            &ClmRequestInputs {
                system_prompt: String::new(),
                tools: vec![],
                model: model.clone(),
            },
        )
        .await
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let owner = engine.session.session_handle().clone();
    let response = pa_ai::faux::FauxResponseStep::Factory(Arc::new({
        let requests = requests.clone();
        move |context, _, _, _| {
            if owner.try_lock().is_err() {
                return Err(
                    "provider work must not hold the receipt acknowledgment owner lock".to_string(),
                );
            }
            requests
                .lock()
                .unwrap()
                .push(serde_json::to_string(context).unwrap());
            Ok(pa_ai::faux::faux_assistant_text_message(
                r#"{"summary":"No changes","edits":[],"shouldRefine":false,"rationale":"No changes"}"#,
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        }
    }));
    provider.set_responses(vec![
        response.clone(),
        response.clone(),
        response.clone(),
        response,
    ]);
    for enabled in [true, false] {
        if !enabled {
            runtime
                .set_mode(false, ClmModeSource::SessionOverride)
                .await
                .unwrap();
        }
        engine
            .session
            .refine(
                &RefineOptions::default(),
                RefinementSource::User,
                &model,
                None,
                dir.path().join("global-harness"),
            )
            .await
            .unwrap();
        assert!(engine
            .session
            .review_compact_auto_refine(&model, None, &dir.path().join("global-harness"), 1, 0)
            .await
            .unwrap()
            .is_none());
    }
    let entries = engine.session.entries().await;
    let receipts: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            pa_types::session::FileEntry::Custom { payload, .. }
                if payload.custom_type == "clm_usage_v1" =>
            {
                payload.data.as_ref()
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        receipts.len(),
        4,
        "each planner/review response is acknowledged before returning"
    );
    assert_eq!(
        receipts
            .iter()
            .map(|receipt| receipt["pin"]["mode"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["on", "on", "off", "off"]
    );
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.get("messageEntryId").is_none()),
        "helpers do not invent assistant journal rows"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in requests.iter() {
        assert!(
            request.contains("ACCEPTED_REFINEMENT_CONTEXT"),
            "planner must receive accepted replacement"
        );
        assert!(
            !request.contains("FORGOTTEN_REFINEMENT_AUDIT"),
            "planner must not receive superseded audit"
        );
    }
    provider.unregister();
}

#[tokio::test]
async fn clm_real_python_frame_uses_distinct_prebound_fences_for_repeated_tool_ids() {
    use crate::kernel::shared::{host_handler, HostRequestHandlers};
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let origins = Arc::new(Mutex::new(Vec::new()));
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "identity_probe",
        host_handler({
            let origins = origins.clone();
            move |payload| {
                origins.lock().unwrap().push((
                    payload.verified_tool_call_id,
                    payload.verified_tool_execution_identity,
                ));
                async { Ok(serde_json::json!({"observed":true})) }
            }
        }),
    );
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        session_manager: Some(SessionManager::persisted(
            dir.path(),
            &dir.path().join("sessions"),
        )),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None,
            model.clone(),
        )),
        extra_host_handlers: Some(handlers),
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    let tool = pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_message(
        vec![pa_ai::faux::faux_tool_call(
            "ipython",
            &serde_json::json!({"code":"from rlm.repl import host_request\nawait host_request({'type': 'identity_probe'})"}),
            Some("repeated-id"),
        )],
        pa_ai::faux::FauxAssistantMessageOptions {
            stop_reason: Some(pa_types::ai::StopReason::ToolUse),
            ..Default::default()
        },
    ));
    let done = pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
        "done",
        pa_ai::faux::FauxAssistantMessageOptions::default(),
    ));
    provider.set_responses(vec![
        tool.clone(),
        done.clone(),
        tool.clone(),
        done.clone(),
        tool,
        done,
    ]);
    engine
        .prompt("first", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    engine
        .prompt("second", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    runtime
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    engine
        .prompt("unowned", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    {
        let origins = origins.lock().unwrap();
        assert_eq!(origins.len(), 3);
        assert!(origins
            .iter()
            .all(|(id, _)| id.as_deref() == Some("repeated-id")));
        let first = origins[0]
            .1
            .as_ref()
            .expect("first prepared fence reaches real frame");
        let second = origins[1]
            .1
            .as_ref()
            .expect("second prepared fence reaches real frame");
        assert!(!first.same_instance(second));
        assert!(origins[2].1.is_none());
    }
    provider.unregister();
}

#[tokio::test]
async fn clm_child_admission_keeps_captured_instance_and_one_mode_snapshot() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        session_manager: Some(SessionManager::persisted(
            dir.path(),
            &dir.path().join("sessions"),
        )),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None,
            model.clone(),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    let idle = runtime.admit_child(None, None).await.unwrap();
    assert!(!idle.inherited_mode());
    runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    runtime.controller.prepare_tool("repeat").await.unwrap();
    let first = Arc::new(ClmActionFence {
        tool_call_id: "repeat".into(),
        identity: crate::kernel::shared::ToolExecutionIdentity::new(),
    });
    *runtime.active_tool.lock().unwrap() = Some(first.clone());
    let captured = runtime
        .admit_child(Some("repeat"), Some(&first.identity))
        .await
        .unwrap();
    assert!(captured.inherited_mode());
    captured.recheck().await.unwrap();
    assert!(runtime.admit_child(Some("repeat"), None).await.is_err());
    runtime
        .controller
        .settle_tool(
            "repeat",
            ClmToolOutcome::Completed,
            &ClmRequestInputs {
                system_prompt: String::new(),
                tools: vec![],
                model: model.clone(),
            },
        )
        .await
        .unwrap();
    *runtime.active_tool.lock().unwrap() = None;
    runtime
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    captured.recheck().await.unwrap();
    assert!(
        captured.inherited_mode(),
        "later checks must not recapture inherited mode"
    );
    runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    runtime.controller.prepare_tool("repeat").await.unwrap();
    let second = Arc::new(ClmActionFence {
        tool_call_id: "repeat".into(),
        identity: crate::kernel::shared::ToolExecutionIdentity::new(),
    });
    *runtime.active_tool.lock().unwrap() = Some(second.clone());
    assert!(captured.recheck().await.is_err());
    assert!(runtime
        .admit_child(Some("repeat"), Some(&first.identity))
        .await
        .is_err());
    let current = runtime
        .admit_child(Some("repeat"), Some(&second.identity))
        .await
        .unwrap();
    runtime.cancel_tool("repeat");
    assert!(current.recheck().await.is_err());
    provider.unregister();
}

#[tokio::test]
async fn clm_inherited_false_overrides_default_and_children_own_distinct_epochs() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"clm":{"enabled":true}}"#,
    )
    .unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let make = |name: &str, inherited_clm_mode: Option<bool>| {
        create_session(SessionEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: agent_dir.clone(),
            session_manager: Some(SessionManager::persisted(
                dir.path(),
                &dir.path().join(name),
            )),
            model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
            stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
                None,
                model.clone(),
            )),
            inherited_clm_mode,
            ..Default::default()
        })
    };
    let parent = make("parent", None).await.unwrap();
    let child_off = make("child-off", Some(false)).await.unwrap();
    let child_on = make("child-on", Some(true)).await.unwrap();
    let parent = parent.session.clm.as_ref().unwrap();
    let child_off = child_off.session.clm.as_ref().unwrap();
    let child_on = child_on.session.clm.as_ref().unwrap();
    assert!(parent.enabled());
    assert!(!child_off.enabled());
    assert!(child_on.enabled());
    let parent_status = parent.status(true).await.unwrap();
    let child_status = child_on.status(true).await.unwrap();
    assert_ne!(parent_status.epoch_id, child_status.epoch_id);
    assert_ne!(parent_status.path, child_status.path);
    assert_eq!(
        child_status.mode_source,
        pa_types::clm::ClmStatusSource::InheritedChild
    );
    assert_eq!(
        child_off.status(true).await.unwrap().mode_source,
        pa_types::clm::ClmStatusSource::InheritedChild
    );
    parent
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    assert!(child_on.enabled());
    assert!(!child_off.enabled());
    provider.unregister();
}

#[tokio::test]
async fn clm_compaction_retry_records_each_observed_response_before_failure() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    use pa_ai::faux::{FauxAssistantMessageOptions, FauxResponseStep};
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"compaction":{"keepRecentTokens":20}}"#,
    )
    .unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let mut owner = SessionManager::persisted(dir.path(), &dir.path().join("sessions"));
    for i in 0..3 {
        owner.append_message(serde_json::from_value(serde_json::json!({"role":"user","content":format!("turn {i} message with some words"),"timestamp":i})).unwrap()).unwrap();
        let mut assistant = pa_ai::faux::faux_assistant_text_message(
            &format!("answer {i}"),
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        );
        assistant.usage = pa_types::ai::Usage {
            input: 100,
            output: 20,
            total_tokens: 120,
            ..Default::default()
        };
        owner
            .append_message(pa_types::session::AgentMessage::Assistant(assistant))
            .unwrap();
    }
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir,
        session_manager: Some(owner),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None,
            model.clone(),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    provider.set_responses(vec![
        FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            "capped",
            FauxAssistantMessageOptions {
                stop_reason: Some(pa_types::ai::StopReason::Length),
                ..Default::default()
            },
        )),
        FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            "failed",
            FauxAssistantMessageOptions {
                stop_reason: Some(pa_types::ai::StopReason::Error),
                error_message: Some("observed failure".into()),
                ..Default::default()
            },
        )),
    ]);
    let error = engine
        .session
        .compact(None, &model, None, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("observed failure"), "{error:#}");
    assert_eq!(provider.call_count(), 2);
    let entries = engine.session.entries().await;
    let receipts: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            pa_types::session::FileEntry::Custom { payload, .. }
                if payload.custom_type == "clm_usage_v1" =>
            {
                Some((entry.id().unwrap(), payload.data.as_ref().unwrap()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        receipts.len(),
        2,
        "both capped and failed observed attempts must persist"
    );
    assert_eq!(
        receipts[1].1["pin"]["branchId"], receipts[0].0,
        "retry captures its own dispatch pin after the prior receipt"
    );
    assert!(receipts
        .iter()
        .all(|(_, receipt)| receipt["pin"]["mode"] == "off"
            && receipt.get("messageEntryId").is_none()));
    provider.unregister();
}

#[tokio::test]
async fn clm_helper_callbacks_keep_original_mode_and_reject_replaced_owner() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        session_manager: Some(SessionManager::persisted(
            dir.path(),
            &dir.path().join("sessions"),
        )),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None,
            model.clone(),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    let context = pa_types::ai::Context {
        system_prompt: None,
        messages: vec![],
        tools: None,
    };
    let on = runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    let unobserved = runtime
        .on_observed_response(&model, &context)
        .await
        .unwrap();
    drop(unobserved);
    let callback = runtime
        .on_observed_response(&model, &context)
        .await
        .unwrap();
    runtime
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    let mut response = pa_ai::faux::faux_assistant_text_message(
        "observed",
        pa_ai::faux::FauxAssistantMessageOptions::default(),
    );
    response.usage = pa_types::ai::Usage {
        input: 31,
        output: 7,
        total_tokens: 38,
        ..Default::default()
    };
    callback(response.clone()).await.unwrap();
    let entries = engine.session.entries().await;
    let receipts: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            pa_types::session::FileEntry::Custom { payload, .. }
                if payload.custom_type == "clm_usage_v1" =>
            {
                payload.data.as_ref()
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        receipts.len(),
        1,
        "dropping a callback before any observed response creates no receipt"
    );
    assert_eq!(receipts[0]["pin"]["mode"], "on");
    assert_eq!(
        receipts[0]["pin"]["epochId"],
        serde_json::json!(on.epoch_id)
    );
    assert!(
        runtime
            .status(false)
            .await
            .unwrap()
            .measured_usage
            .is_none(),
        "off status must not adopt late on usage"
    );
    let stale = runtime
        .on_observed_response(&model, &context)
        .await
        .unwrap();
    *engine.session.shared_persistence().lock().await =
        SessionManager::persisted(dir.path(), &dir.path().join("replacement"));
    let error = stale(response).await.unwrap_err();
    assert!(error.to_string().contains("replaced session"), "{error:#}");
    assert!(runtime.check_failure().is_err());
    assert!(!engine.session.entries().await.iter().any(|entry|matches!(entry,pa_types::session::FileEntry::Custom {payload,..} if payload.custom_type=="clm_usage_v1")));
    provider.unregister();
}

#[tokio::test]
async fn clm_helper_budget_preserves_main_inputs_and_refinement_filters_usage() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::{
        engine::{create_session, SessionEngineConfig},
        refine::{RefineOptions, RefinementSource},
    };
    use pa_ai::faux::{FauxAssistantMessageOptions, FauxResponseStep};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        session_manager: Some(SessionManager::persisted(
            dir.path(),
            &dir.path().join("sessions"),
        )),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None,
            model.clone(),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    let on = runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    engine.session.agent.set_system_prompt("main prefix").await;
    runtime
        .record_inputs(
            &ClmRequestInputs {
                system_prompt: "main prefix".into(),
                tools: vec![],
                model: model.clone(),
            },
            &[],
        )
        .unwrap();
    let mut helper_model = model.clone();
    helper_model.context_window = 128;
    helper_model.max_tokens = 8;
    let options = RefineOptions::default();
    let error = crate::session_engine::compaction_exec::complete_summary_call_observed(
        &helper_model,
        "helper",
        None,
        None,
        8,
        vec![serde_json::from_value(
            serde_json::json!({"role":"user","content":"x".repeat(4096),"timestamp":0}),
        )
        .unwrap()],
        None,
        "summary failed",
        Some(runtime),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("CLM helper request exceeds"),
        "{error:#}"
    );
    assert_eq!(
        provider.call_count(),
        0,
        "reject the actual helper prompt before dispatch"
    );
    let estimate = runtime
        .status(false)
        .await
        .unwrap()
        .estimated_occupancy
        .unwrap();
    assert!(estimate.estimated_input_tokens > estimate.max_input_tokens.unwrap());
    assert_eq!(
        engine.session.agent.state().await.system_prompt,
        "main prefix",
        "helper input must not replace main request input"
    );
    runtime
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    provider.set_responses(vec![
        FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            r#"{"summary":"No changes","edits":[]}"#,
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        )),
        FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
            "aborted",
            FauxAssistantMessageOptions {
                stop_reason: Some(pa_types::ai::StopReason::Aborted),
                ..Default::default()
            },
        )),
    ]);
    engine
        .session
        .refine(
            &options,
            RefinementSource::User,
            &model,
            None,
            dir.path().join("global"),
        )
        .await
        .unwrap();
    assert!(engine
        .session
        .refine(
            &options,
            RefinementSource::User,
            &model,
            None,
            dir.path().join("global")
        )
        .await
        .is_err());
    assert!(engine
        .session
        .refine(
            &options,
            RefinementSource::User,
            &model,
            None,
            dir.path().join("global")
        )
        .await
        .is_err());
    assert_eq!(provider.call_count(), 3);
    let entries = engine.session.entries().await;
    assert_eq!(entries.iter().filter(|entry|matches!(entry,pa_types::session::FileEntry::Custom {payload,..} if payload.custom_type=="clm_usage_v1")).count(),1,"off observed response retained; aborted and faux exhaustion excluded");
    runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    std::fs::write(&on.path, "invalid marker file").unwrap();
    assert!(engine
        .session
        .refine(
            &options,
            RefinementSource::User,
            &model,
            None,
            dir.path().join("global")
        )
        .await
        .is_err());
    assert_eq!(
        provider.call_count(),
        3,
        "invalid working markers must block helper dispatch"
    );
    provider.unregister();
}

#[tokio::test]
async fn clm_tool_settlement_rechecks_live_prompt_before_accepting_edit() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    use pa_ai::faux::{FauxAssistantMessageOptions, FauxResponseStep};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let mut model = provider.models[0].clone();
    model.context_window = 20_000;
    model.max_tokens = 1024;
    let agent = Arc::new(Mutex::new(None::<std::sync::Weak<pa_agent::agent::Agent>>));
    let path = Arc::new(Mutex::new(None::<std::path::PathBuf>));
    let tool = crate::session_engine::tool_bridge::bridge_tool(
        crate::tools::tool_definition::ToolDefinition {
            name: "change_prompt".into(),
            label: "change_prompt".into(),
            description: "update prompt and context".into(),
            prompt_snippet: String::new(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
            execution_mode: None,
            prepare_arguments: None,
            execute: Arc::new({
                let agent = agent.clone();
                let path = path.clone();
                move |_, _, _, _| {
                    let agent = agent.lock().unwrap().as_ref().unwrap().upgrade().unwrap();
                    let path = path.lock().unwrap().as_ref().unwrap().clone();
                    Box::pin(async move {
                        agent.set_system_prompt("x".repeat(100_000)).await;
                        let text = std::fs::read_to_string(&path)?;
                        std::fs::write(path, text.replace("old detail", "edited detail"))?;
                        Ok(crate::tools::tool_definition::ToolExecutionResult::text(
                            "edited",
                        ))
                    })
                }
            }),
        },
    );
    let mut owner = SessionManager::persisted(dir.path(), &dir.path().join("sessions"));
    owner
        .append_message(pa_types::session::AgentMessage::Assistant(
            pa_ai::faux::faux_assistant_text_message(
                "old detail",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ),
        ))
        .unwrap();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        session_manager: Some(owner),
        tools: vec![tool],
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None, model,
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    *agent.lock().unwrap() = Some(Arc::downgrade(&engine.session.agent));
    let runtime = engine.session.clm.as_ref().unwrap();
    let initial = runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    *path.lock().unwrap() = Some(std::path::PathBuf::from(&initial.path));
    provider.set_responses(vec![FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_message(
            vec![pa_ai::faux::faux_tool_call(
                "change_prompt",
                &serde_json::json!({}),
                Some("edit"),
            )],
            FauxAssistantMessageOptions {
                stop_reason: Some(pa_types::ai::StopReason::ToolUse),
                ..Default::default()
            },
        ),
    )]);
    let _ = engine
        .prompt(
            "refresh prompt before accepting context",
            crate::session_engine::PromptOptions::default(),
        )
        .await;
    assert!(!engine.session.entries().await.iter().any(|entry|matches!(entry,
        pa_types::session::FileEntry::ClmRevision {payload,..} if payload.observed_action_id=="edit"
    )),"the edit itself must not commit; terminal failure cleanup is a separate revision");
    let accepted = serde_json::to_string(&runtime.view().await.unwrap()).unwrap();
    assert!(accepted.contains("old detail"));
    assert!(!accepted.contains("edited detail"));
    provider.unregister();
}

#[tokio::test]
async fn clm_runtime_cancellation_blocks_suspended_read_and_late_owner_ack() {
    use crate::session::manager::SessionManager;
    use pa_agent::types::{
        AfterToolCallContext, AgentContext, AgentToolResult, BeforeToolCallContext,
    };
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    for pause_commit_ack in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::persisted(dir.path(), &dir.path().join("sessions"));
        manager
            .append_message(pa_types::session::AgentMessage::Assistant(
                pa_ai::faux::faux_assistant_text_message(
                    "old detail",
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ),
            ))
            .unwrap();
        let file = manager.get_session_file().map(std::path::PathBuf::from);
        let owner = Arc::new(tokio::sync::Mutex::new(manager));
        let armed = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let journal: ClmJournalFn = Arc::new({
            let owner = owner.clone();
            let armed = armed.clone();
            let entered = entered.clone();
            let release = release.clone();
            move |op| {
                let owner = owner.clone();
                let armed = armed.clone();
                let entered = entered.clone();
                let release = release.clone();
                Box::pin(async move {
                    let selected = if pause_commit_ack {
                        matches!(&op, ClmJournalOp::Commit(_))
                    } else {
                        matches!(&op, ClmJournalOp::ReadAfterEvents)
                    };
                    let pause = selected && armed.swap(false, Ordering::SeqCst);
                    if pause && !pause_commit_ack {
                        entered.notify_one();
                        release.notified().await;
                    }
                    let reply = owner
                        .lock()
                        .await
                        .clm_journal(op, std::collections::BTreeMap::default())?;
                    if pause && pause_commit_ack {
                        entered.notify_one();
                        release.notified().await;
                    }
                    Ok(reply)
                })
            }
        });
        let artifacts = dir.path().join("artifacts");
        let protected = Arc::new(Mutex::new(crate::clm::ProtectedActions::new()));
        let runtime = ClmRuntime::new(
            journal.clone(),
            file.clone(),
            artifacts.clone(),
            protected.clone(),
        )
        .await
        .unwrap();
        let agent = Arc::new(pa_agent::agent::Agent::new(pa_agent::agent::AgentOptions {
            initial_state: pa_agent::agent::AgentInitialState {
                model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
                ..Default::default()
            },
            ..Default::default()
        }));
        runtime.bind_agent(&agent);
        let status = runtime
            .set_mode(true, ClmModeSource::SessionOverride)
            .await
            .unwrap();
        let tool_call = serde_json::from_value(
            serde_json::json!({"type":"toolCall","id":"writer","name":"edit","arguments":{}}),
        )
        .unwrap();
        let assistant_message = serde_json::from_value(
            serde_json::to_value(pa_ai::faux::faux_assistant_text_message(
                "tool",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
            .unwrap(),
        )
        .unwrap();
        let context = AgentContext {
            system_prompt: String::new(),
            messages: vec![],
            tools: vec![],
        };
        let before = BeforeToolCallContext {
            assistant_message,
            tool_call,
            args: serde_json::json!({}),
            context,
        };
        let abort = pa_agent::abort::AbortController::new();
        runtime.before_tool()(before.clone(), abort.signal())
            .await
            .unwrap();
        let old = std::fs::read_to_string(&status.path).unwrap();
        std::fs::write(&status.path, old.replace("old detail", "edited detail")).unwrap();
        let after = AfterToolCallContext {
            assistant_message: before.assistant_message,
            tool_call: before.tool_call,
            args: before.args,
            context: before.context,
            result: AgentToolResult::text("edited"),
            is_error: false,
        };
        armed.store(true, Ordering::SeqCst);
        let settling = tokio::spawn(runtime.after_tool()(after, abort.signal()));
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        abort.abort();
        let cancelled_bytes = "bytes written by a cancelled writer";
        std::fs::write(&status.path, cancelled_bytes).unwrap();
        release.notify_one();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), settling)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(runtime.check_failure().is_err());
        assert!(runtime.controller.project_request().await.is_err());
        assert_eq!(
            std::fs::read_to_string(&status.path).unwrap(),
            cancelled_bytes,
            "late ACK must not republish or repair a cancelled writer"
        );
        let revisions = owner
            .lock()
            .await
            .get_entries()
            .iter()
            .filter(|entry| matches!(entry, pa_types::session::FileEntry::ClmRevision { .. }))
            .count();
        assert_eq!(
            revisions,
            usize::from(pause_commit_ack),
            "already submitted owner commit may finish; a cancelled read must not submit one"
        );
        assert!(
            ClmRuntime::new(journal, file, artifacts, protected)
                .await
                .is_err(),
            "reopen must retain the unresolved writer marker"
        );
    }
    provider.unregister();
}

#[tokio::test]
async fn clm_standalone_helper_unknown_usage_preserves_owner_and_reopen() {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};

    struct UnknownUsageProvider(String);
    impl pa_ai::Provider for UnknownUsageProvider {
        fn api(&self) -> &str {
            &self.0
        }
        fn stream(
            &self,
            model: &pa_types::ai::Model,
            _context: &pa_types::ai::Context,
            _options: Option<&pa_ai::types::StreamOptions>,
        ) -> pa_ai::AssistantMessageEventStream {
            let (writer, stream) = pa_ai::AssistantMessageEventStream::new();
            let mut message = pa_ai::faux::faux_assistant_text_message(
                "helper summary",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            );
            message.api.clone_from(&model.api);
            message.provider.clone_from(&model.provider);
            message.model.clone_from(&model.id);
            message.usage = pa_types::ai::Usage::default();
            writer.push(pa_types::ai::AssistantMessageEvent::Done {
                reason: pa_types::ai::DoneStopReason::Stop,
                message,
            });
            stream
        }
        fn stream_simple(
            &self,
            model: &pa_types::ai::Model,
            context: &pa_types::ai::Context,
            _options: Option<&pa_ai::types::SimpleStreamOptions>,
        ) -> pa_ai::AssistantMessageEventStream {
            self.stream(model, context, None)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    pa_ai::registry::register_api_provider(
        Arc::new(UnknownUsageProvider(provider.api.clone())),
        Some(&provider.api),
    );
    let sessions = dir.path().join("sessions");
    let mut owner = SessionManager::persisted(dir.path(), &sessions);
    owner
        .append_message(
            serde_json::from_value(serde_json::json!({
                "role":"user", "content":"existing helper context", "timestamp":1,
            }))
            .unwrap(),
        )
        .unwrap();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        session_manager: Some(owner),
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None,
            model.clone(),
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    for (index, enabled) in [true, false].into_iter().enumerate() {
        runtime
            .set_mode(enabled, ClmModeSource::SessionOverride)
            .await
            .unwrap();
        let result = crate::session_engine::compaction_exec::complete_summary_call_observed(
            &model,
            &engine.session.session_id().await,
            None,
            None,
            64,
            vec![serde_json::from_value(serde_json::json!({
                "role":"user", "content":"summarize the context", "timestamp":2,
            }))
            .unwrap()],
            None,
            "summary failed",
            Some(runtime),
        )
        .await;
        assert!(
            result.is_ok(),
            "unknown helper usage must remain readable: {result:?}"
        );
        assert_eq!(result.unwrap().summary, "helper summary");
        let entries = engine.session.entries().await;
        let receipts: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                pa_types::session::FileEntry::Custom { payload, .. }
                    if payload.custom_type.starts_with("clm_usage_") =>
                {
                    Some(payload)
                }
                _ => None,
            })
            .collect();
        assert_eq!(receipts.len(), index + 1);
        assert_eq!(receipts[index].custom_type, "clm_usage_unknown_v1");
        let data = receipts[index].data.as_ref().unwrap();
        assert!(data.get("usage").is_none());
        assert!(data.get("messageEntryId").is_none());
        assert!(runtime
            .status(false)
            .await
            .unwrap()
            .measured_usage
            .is_none());
        assert!(runtime.view().await.is_ok());
        let path = engine
            .session
            .session_handle()
            .lock()
            .await
            .get_session_file()
            .unwrap()
            .to_owned();
        let reopened = SessionManager::open(dir.path(), &sessions, &path);
        let context = reopened
            .active_context_checked()
            .expect("unknown receipt must not corrupt reopen");
        assert!(serde_json::to_string(&context.messages)
            .unwrap()
            .contains("existing helper context"));
    }
    pa_ai::registry::unregister_api_providers(&provider.api);
    provider.unregister();
}

#[tokio::test]
async fn clm_off_side_stream_failure_keeps_main_receipt() {
    off_side_overlap_keeps_main_receipt(true).await;
}

#[tokio::test]
async fn clm_off_side_response_keeps_main_receipt() {
    off_side_overlap_keeps_main_receipt(false).await;
}

async fn off_side_overlap_keeps_main_receipt(side_fails: bool) {
    use crate::session::manager::SessionManager;
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    let dir = tempfile::tempdir().unwrap();
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let response = pa_ai::faux::FauxResponseStep::Factory(Arc::new(|context, _, _, _| {
        let side = serde_json::to_string(context)
            .unwrap()
            .contains("<side_question>");
        Ok(pa_ai::faux::faux_assistant_text_message(
            if side { "side answer" } else { "main answer" },
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ))
    }));
    provider.set_responses(vec![response.clone(), response]);
    let base_stream = crate::session_engine::provider_adapter::real_stream_fn(None, model.clone());
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered)));
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let stream: pa_agent::stream::StreamFn = Arc::new(move |model, context, options| {
        let side = serde_json::to_string(&context)
            .unwrap()
            .contains("<side_question>");
        let base_stream = base_stream.clone();
        let entered = entered.clone();
        let release_rx = release_rx.clone();
        Box::pin(async move {
            if side && side_fails {
                anyhow::bail!("side stream construction failed");
            }
            if !side {
                let _ = entered.lock().unwrap().take().unwrap().send(());
                let release = release_rx.lock().unwrap().take().unwrap();
                release.await.unwrap();
            }
            base_stream(model, context, options).await
        })
    });
    let engine = Arc::new(
        create_session(SessionEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            session_manager: Some(SessionManager::persisted(
                dir.path(),
                &dir.path().join("sessions"),
            )),
            model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
            stream_fn: Some(stream),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let runtime = engine.session.clm.as_ref().unwrap();
    runtime
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    let off = runtime
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    let main = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .prompt(
                    "main overlap",
                    crate::session_engine::PromptOptions::default(),
                )
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let (main_leaf, path) = {
        let mut owner = engine.session.session_handle().lock().await;
        let main_leaf = owner.get_leaf_id().unwrap().to_owned();
        // An ordinary audit-only owner append can land while the main provider is in flight.
        owner
            .append_custom_entry("overlap_marker", Some(serde_json::json!({"during":"main"})))
            .unwrap();
        (main_leaf, owner.get_session_file().unwrap().to_owned())
    };
    let journal_before_side = std::fs::read(&path).unwrap();
    let messages_before_side =
        serde_json::to_value(engine.session.agent.state().await.messages).unwrap();
    let signal = pa_agent::abort::AbortController::new();
    let sink: crate::session_engine::side_question::SideQuestionSink = Arc::new(|_| true);
    let side = engine
        .session
        .run_side_question(
            "concurrent side question",
            &[],
            &crate::session_engine::provider_retry::ProviderRetryPolicy {
                enabled: false,
                ..crate::session_engine::provider_retry::DEFAULT_PROVIDER_RETRY_POLICY
            },
            &signal.signal(),
            &sink,
        )
        .await;
    assert_eq!(
        side.status,
        if side_fails {
            crate::session_engine::side_question::SideQuestionStatus::Error
        } else {
            crate::session_engine::side_question::SideQuestionStatus::Complete
        }
    );
    assert_eq!(std::fs::read(&path).unwrap(), journal_before_side);
    assert_eq!(
        serde_json::to_value(engine.session.agent.state().await.messages).unwrap(),
        messages_before_side
    );
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), main)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let entries = engine.session.entries().await;
    let receipts: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            pa_types::session::FileEntry::Custom { payload, .. }
                if payload.custom_type.starts_with("clm_usage_") =>
            {
                Some(payload)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        receipts.len(),
        1,
        "side request must neither consume nor create parent receipts"
    );
    let receipt = receipts[0].data.as_ref().unwrap();
    assert_eq!(receipts[0].custom_type, "clm_usage_v1");
    assert_eq!(
        receipt["pin"]["branchId"], main_leaf,
        "keep the original main dispatch pin"
    );
    assert_eq!(receipt["pin"]["mode"], "off");
    assert_eq!(
        receipt["pin"]["epochId"],
        serde_json::to_value(off.epoch_id).unwrap()
    );
    assert_eq!(
        receipt["pin"]["revision"],
        serde_json::to_value(off.revision).unwrap()
    );
    let assistant = entries
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap())
        .find(|entry| entry["id"] == receipt["messageEntryId"])
        .unwrap();
    assert_eq!(assistant["message"]["role"], "assistant");
    assert!(assistant["message"].to_string().contains("main answer"));
    assert_eq!(receipt["usage"], assistant["message"]["usage"]);
    assert!(receipt["usage"]["totalTokens"].as_u64().unwrap() > 0);
    assert_eq!(provider.call_count(), if side_fails { 1 } else { 2 });
    provider.unregister();
}
