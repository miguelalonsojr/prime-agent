//! Compact-session tests.
use super::*;
use pa_types::ai::{AssistantMessage, UserContent};

fn session_with_turns(cwd: &std::path::Path, turns: usize) -> SessionManager {
    let mut session = SessionManager::in_memory(cwd);
    for i in 0..turns {
        session
            .append_message(AgentMessage::User(pa_types::ai::UserMessage {
                content: UserContent::Text(format!("turn {i} message with some words")),
                timestamp: 0,
                rest: serde_json::Map::default(),
            }))
            .unwrap();
        session
            .append_message(AgentMessage::Assistant(AssistantMessage {
                content: vec![pa_types::ai::AssistantContentBlock::Text(
                    pa_types::ai::TextContent {
                        text: format!("reply {i}"),
                        text_signature: None,
                        rest: serde_json::Map::default(),
                    },
                )],
                api: "openai-completions".to_string(),
                provider: "test".to_string(),
                model: "m".to_string(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: pa_types::ai::Usage {
                    input: 100,
                    output: 20,
                    cache_read: 0,
                    cache_write: 0,
                    total_tokens: 120,
                    cost: pa_types::ai::UsageCost::default(),
                },
                stop_reason: pa_types::ai::StopReason::Stop,
                stop_reason_raw: None,
                error_message: None,
                timestamp: 0,
                rest: serde_json::Map::default(),
            }))
            .unwrap();
    }
    session
}

/// The faux provider with one scripted summarizer response. The faux
/// seam is process-global, so every registration unregisters on drop.
fn faux_registration() -> pa_ai::faux::FauxProviderRegistration {
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "compact-m".to_string(),
                name: Some("Compact Model".to_string()),
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(1_000),
                max_tokens: Some(256),
            }]),
            ..Default::default()
        });
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "## Goal\nsummarized goal",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    registration
}

fn receipt_projection_preserves_turn_cut(scenario: &str) {
    use crate::clm::journal::{ClmCommit, ClmJournalOp, ClmJournalReply};
    use pa_types::clm::{
        ClmBoundary, ClmMode, ClmOffBoundaryEntry, ClmOnEntry, ClmSnapshot, ClmUsagePin,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut owner = SessionManager::persisted(dir.path(), dir.path());
    let mut receipt_ids = Vec::new();
    let mut message_ids = Vec::new();
    let mut off_entry_id = None;
    let add_boundary = |owner: &mut SessionManager| {
        let blocks = owner
            .retained_entries()
            .iter()
            .filter_map(|row| {
                let FileEntry::Message { message, .. } = row else {
                    return None;
                };
                Some(pa_types::clm::ClmBlock {
                    id: row.id().unwrap().to_owned(),
                    message: serde_json::to_value(message).unwrap(),
                })
            })
            .collect();
        let boundary = ClmBoundary {
            epoch_id: "receipt-projection".into(),
            revision: 0,
            snapshot: ClmSnapshot::from_blocks(blocks),
            audit_watermark: owner
                .retained_entries()
                .iter()
                .rev()
                .find(|entry| matches!(entry, FileEntry::Message { .. }))
                .and_then(FileEntry::id)
                .map(str::to_owned),
        };
        for revision in [0, 1] {
            let ClmJournalReply::View(expected) = owner
                .clm_journal(
                    ClmJournalOp::ReadAfterEvents,
                    crate::clm::ProtectedActions::new(),
                )
                .unwrap()
            else {
                panic!("owner view expected");
            };
            let mut boundary = boundary.clone();
            boundary.revision = revision;
            let base = pa_types::session::EntryBase {
                id: None,
                parent_id: None,
                timestamp: None,
                rest: serde_json::Map::default(),
            };
            let row = if revision == 0 {
                FileEntry::ClmOn {
                    base,
                    payload: ClmOnEntry {
                        boundary,
                        mode_source: None,
                    },
                }
            } else {
                FileEntry::ClmOffBoundary {
                    base,
                    payload: ClmOffBoundaryEntry {
                        boundary,
                        mode_source: None,
                    },
                }
            };
            owner
                .clm_journal(
                    ClmJournalOp::Commit(ClmCommit {
                        expected,
                        row,
                        allow_child_usage_suffix: false,
                    }),
                    crate::clm::ProtectedActions::new(),
                )
                .unwrap();
        }
        owner.get_leaf_id().unwrap().to_owned()
    };
    let source = session_with_turns(dir.path(), 2);
    for row in source.retained_entries() {
        let FileEntry::Message { message, .. } = row else {
            continue;
        };
        let id = owner.append_message(message.clone()).unwrap();
        message_ids.push(id.clone());
        if let AgentMessage::Assistant(assistant) = message {
            let usage = if receipt_ids.is_empty() {
                serde_json::to_value(assistant.usage).unwrap()
            } else {
                serde_json::Value::Null
            };
            let pin = ClmUsagePin {
                mode: ClmMode::Off,
                branch_id: Some(id.clone()),
                epoch_id: off_entry_id.as_ref().map(|_| "receipt-projection".into()),
                revision: off_entry_id.as_ref().map(|_| 1),
                entry_id: off_entry_id.clone(),
            };
            let receipt = crate::clm::clm_usage_receipt(&pin, &usage, Some(&id));
            let receipt_id = owner
                .append_custom_entry(
                    receipt["customType"].as_str().unwrap(),
                    Some(receipt["data"].clone()),
                )
                .unwrap();
            receipt_ids.push(receipt_id);
            if scenario == "off" && receipt_ids.len() == 1 {
                off_entry_id = Some(add_boundary(&mut owner));
            }
        }
    }
    let unrelated = owner.append_custom_entry("clm_usage_other", None).unwrap();
    if scenario == "pre-epoch" {
        add_boundary(&mut owner);
        owner.branch(&unrelated);
    }
    owner.flush_now().unwrap();
    let path = owner.get_session_file().unwrap();
    let bytes = std::fs::read(path).unwrap();
    let audit = serde_json::to_value(owner.retained_entries()).unwrap();
    let keep_recent = source
        .retained_entries()
        .iter()
        .filter_map(message_from_entry)
        .skip(2)
        .map(|message| super::super::compaction::estimate_tokens(&message))
        .sum();
    let entries = super::prepare::model_compaction_entries(&owner).unwrap();
    let prepared = super::prepare::prepare_compaction(&entries, keep_recent).unwrap();
    assert!(
        !prepared.cut.is_split_turn,
        "receipts must not split an ordinary turn: {scenario}"
    );
    assert_eq!(
        entries[prepared.cut.first_kept_entry_index].id(),
        Some(message_ids[2].as_str())
    );
    assert!(entries
        .iter()
        .all(|row| !receipt_ids.iter().any(|id| row.id() == Some(id.as_str()))));
    assert!(
        entries
            .iter()
            .any(|row| row.id() == Some(unrelated.as_str())),
        "do not filter unrelated custom rows"
    );
    assert_eq!(
        serde_json::to_value(owner.retained_entries()).unwrap(),
        audit
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn clm_compaction_projection_ignores_receipts_before_first_epoch() {
    receipt_projection_preserves_turn_cut("never-on");
}

#[test]
fn clm_compaction_projection_ignores_receipts_after_off_with_receipt_suffix() {
    receipt_projection_preserves_turn_cut("off");
}

#[test]
fn clm_compaction_projection_ignores_receipts_on_pre_epoch_branch() {
    receipt_projection_preserves_turn_cut("pre-epoch");
}

mod auxiliary_model;
mod cut_math;
mod execute_compaction;
mod prepare_compaction;
mod second_compaction;
mod skip_guards;
mod split_turn;

async fn execute_compaction(
    session: &mut SessionManager,
    options: CompactOptions<'_>,
) -> anyhow::Result<CompactOutcome> {
    let started_at = std::time::Instant::now();
    let mut attempt = match prepare_attempt(session, &options) {
        Ok(attempt) => attempt,
        Err(PrepareError::Skipped(skip)) => {
            return Ok(CompactOutcome::Skipped(skip.user_message()))
        }
        Err(PrepareError::InvalidContext(error)) => return Err(error.into()),
    };
    let prepared = summarize_attempt(&attempt, &options).await?;
    assert!(
        commit_attempt(session, &mut attempt, &prepared, options.abort)?,
        "no concurrent writer in a unit test"
    );
    Ok(CompactOutcome::Ran(Box::new(CompactRun {
        result: prepared.result,
        entry: prepared.entry,
        duration_ms: started_at.elapsed().as_millis() as u64,
        ipython_state: None,
    })))
}

#[tokio::test]
async fn clm_post_off_compaction_summarizes_accepted_bodies_and_iterative_summaries() {
    use crate::clm::controller::{ClmRequestInputs, ClmToolOutcome};
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    use pa_types::clm::ClmModeSource;
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(
        dir.path().join("agent/settings.json"),
        r#"{"compaction":{"keepRecentTokens":1}}"#,
    )
    .unwrap();
    let provider = faux_registration();
    let mut model = provider.models[0].clone();
    model.context_window = 100_000;
    let mut owner = SessionManager::persisted(dir.path(), &dir.path().join("sessions"));
    owner
        .append_custom_message(
            "harness_digest",
            UserContent::Text("STALE_CLM_HARNESS_DIGEST".into()),
            false,
            None,
        )
        .unwrap();
    for row in session_with_turns(dir.path(), 8).retained_entries() {
        if let FileEntry::Message { message, .. } = row {
            owner.append_message(message.clone()).unwrap();
        }
    }
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
        working.replace("turn 0 message with some words", "ACCEPTED_CLM_REPLACEMENT"),
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
    runtime
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let response = pa_ai::faux::FauxResponseStep::Factory(Arc::new({
        let requests = requests.clone();
        move |context, _, _, _| {
            requests
                .lock()
                .unwrap()
                .push(serde_json::to_value(context).unwrap().to_string());
            Ok(pa_ai::faux::faux_assistant_text_message(
                "ITERATIVE_CLM_SUMMARY",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        }
    }));
    provider.set_responses(vec![response; 8]);
    engine
        .session
        .compact(None, &model, None, None)
        .await
        .unwrap();
    let first_count = {
        let requests = requests.lock().unwrap();
        assert!(requests
            .iter()
            .any(|request| request.contains("ACCEPTED_CLM_REPLACEMENT")));
        assert!(requests
            .iter()
            .all(|request| !request.contains("turn 0 message with some words")));
        assert!(requests
            .iter()
            .all(|request| !request.contains("STALE_CLM_HARNESS_DIGEST")));
        requests.len()
    };
    {
        let mut owner = engine.session.session_handle().lock().await;
        owner.append_message(serde_json::from_value(serde_json::json!({"role":"user","content":"later turn to retain","timestamp":2})).unwrap()).unwrap();
        owner
            .append_message(AgentMessage::Assistant(
                pa_ai::faux::faux_assistant_text_message(
                    "later retained reply",
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ),
            ))
            .unwrap();
    }
    engine
        .session
        .compact(None, &model, None, None)
        .await
        .unwrap();
    let requests = requests.lock().unwrap();
    assert!(requests[first_count..]
        .iter()
        .any(|request| request.contains("ITERATIVE_CLM_SUMMARY")));
    assert!(requests
        .iter()
        .all(|request| !request.contains("turn 0 message with some words")));
    provider.unregister();
}

#[tokio::test]
async fn clm_compaction_requires_a_durable_tail_even_when_the_accepted_snapshot_is_valid() {
    use crate::clm::journal::{ClmCommit, ClmJournalOp, ClmJournalReply};
    use pa_types::clm::{ClmBlock, ClmBoundary, ClmOffBoundaryEntry, ClmOnEntry, ClmSnapshot};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut owner = SessionManager::persisted(dir.path(), dir.path());
    let mut blocks = Vec::new();
    for row in session_with_turns(dir.path(), 6).retained_entries() {
        if let FileEntry::Message { message, .. } = row {
            owner.append_message(message.clone()).unwrap();
            blocks.push(ClmBlock {
                id: format!("synthetic-{}", blocks.len()),
                message: serde_json::to_value(message).unwrap(),
            });
        }
    }
    let boundary = ClmBoundary {
        epoch_id: "synthetic-epoch".into(),
        revision: 0,
        snapshot: ClmSnapshot::from_blocks(blocks),
        audit_watermark: owner.get_leaf_id().map(str::to_owned),
    };
    for revision in [0, 1] {
        let ClmJournalReply::View(expected) = owner
            .clm_journal(
                ClmJournalOp::ReadAfterEvents,
                std::collections::BTreeMap::default(),
            )
            .unwrap()
        else {
            panic!("read must return a view")
        };
        let base = pa_types::session::EntryBase {
            id: None,
            parent_id: None,
            timestamp: None,
            rest: serde_json::Map::default(),
        };
        let mut boundary = boundary.clone();
        boundary.revision = revision;
        let row = if revision == 0 {
            FileEntry::ClmOn {
                base,
                payload: ClmOnEntry {
                    boundary,
                    mode_source: None,
                },
            }
        } else {
            FileEntry::ClmOffBoundary {
                base,
                payload: ClmOffBoundaryEntry {
                    boundary,
                    mode_source: None,
                },
            }
        };
        owner
            .clm_journal(
                ClmJournalOp::Commit(ClmCommit {
                    expected,
                    row,
                    allow_child_usage_suffix: false,
                }),
                std::collections::BTreeMap::default(),
            )
            .unwrap();
    }
    assert_eq!(
        owner.active_context_checked().unwrap().messages.len(),
        12,
        "native owner admits this valid synthetic-ID snapshot"
    );
    let path = owner.get_session_file().unwrap().to_path_buf();
    let before = std::fs::read(&path).unwrap();
    let provider = faux_registration();
    let calls = Arc::new(AtomicUsize::new(0));
    let response = pa_ai::faux::FauxResponseStep::Factory(Arc::new({
        let calls = calls.clone();
        move |_, _, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(pa_ai::faux::faux_assistant_text_message(
                "accepted summary",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        }
    }));
    provider.set_responses(vec![response; 8]);
    let options = || CompactOptions {
        clm: None,
        model: provider.get_model(),
        api_key: None,
        custom_instructions: None,
        settings: super::compaction::CompactionSettings {
            keep_recent_tokens: 1,
            ..Default::default()
        },
        abort: None,
        harness_digest: None,
        auxiliary: None,
        semantic_edges: None,
        summary_delta: None,
    };
    let error = execute_compaction(&mut owner, options()).await.unwrap_err();
    assert!(
        error.to_string().contains("durable retained-tail"),
        "{error:#}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    owner
        .append_message(
            serde_json::from_value(
                serde_json::json!({"role":"user","content":"real retained tail","timestamp":2}),
            )
            .unwrap(),
        )
        .unwrap();
    owner
        .append_message(AgentMessage::Assistant(
            pa_ai::faux::faux_assistant_text_message(
                "real retained reply",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ),
        ))
        .unwrap();
    assert!(matches!(
        execute_compaction(&mut owner, options()).await.unwrap(),
        CompactOutcome::Ran(_)
    ));
    assert!(calls.load(Ordering::SeqCst) > 0);
    provider.unregister();
}
