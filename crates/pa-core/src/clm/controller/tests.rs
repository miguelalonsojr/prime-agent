use super::{ClmController, ClmRequestInputs, ClmToolOutcome};
use crate::clm::{journal::ClmJournalFn, ClmError, ProtectedActions};
use crate::session::manager::SessionManager;
use pa_types::clm::{ClmMode, ClmModeSource, ClmRequestBudget, ClmRevisionRef, ClmStatusSource};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct Fixture {
    dir: tempfile::TempDir,
    owner: Arc<Mutex<SessionManager>>,
    protected: Arc<Mutex<ProtectedActions>>,
    controller: ClmController,
    path: PathBuf,
    session: PathBuf,
}

fn inputs() -> ClmRequestInputs {
    ClmRequestInputs {
        system_prompt: "fixed".into(), tools: vec![],
        model: serde_json::from_value(json!({"id":"local","name":"Local","api":"openai-responses","provider":"openai","baseUrl":"http://unused.invalid","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":1000,"maxTokens":300})).unwrap(),
    }
}
fn callback(
    owner: &Arc<Mutex<SessionManager>>,
    protected: &Arc<Mutex<ProtectedActions>>,
) -> ClmJournalFn {
    let owner = owner.clone();
    let protected = protected.clone();
    Arc::new(move |op| {
        let owner = owner.clone();
        let protected = protected.clone();
        Box::pin(async move {
            let mappings = protected.lock().unwrap().clone();
            Ok(owner.lock().unwrap().clm_journal(op, mappings)?)
        })
    })
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::persisted(dir.path(), dir.path());
        for text in ["obsolete", "current request"] {
            manager
                .append_message(
                    serde_json::from_value(json!({"role":"user","content":text,"timestamp":1}))
                        .unwrap(),
                )
                .unwrap();
        }
        let session = manager.get_session_file().unwrap().to_path_buf();
        let owner = Arc::new(Mutex::new(manager));
        let protected = Arc::new(Mutex::new(ProtectedActions::new()));
        let controller = ClmController::new(
            callback(&owner, &protected),
            Some(session.clone()),
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        controller
            .set_mode(true, ClmModeSource::SessionOverride)
            .await
            .unwrap();
        let block = controller.project_request().await.unwrap().blocks[1].clone();
        protected
            .lock()
            .unwrap()
            .insert("request".into(), vec![block]);
        let path = dir.path().join("clm/context.txt");
        Self {
            dir,
            owner,
            protected,
            controller,
            path,
            session,
        }
    }
    fn change(&self, from: &str, to: &str) {
        let text = std::fs::read_to_string(&self.path).unwrap();
        assert!(text.contains(from));
        std::fs::write(&self.path, text.replace(from, to)).unwrap();
    }
    async fn revision(&self) -> u64 {
        self.controller
            .status(false)
            .await
            .unwrap()
            .revision
            .unwrap()
    }
    fn marker(&self) -> PathBuf {
        PathBuf::from(format!("{}.clm-writer", self.session.display()))
    }
}

#[tokio::test]
async fn acknowledged_edit_survives_reopen_without_replaying_removed_audit() {
    let f = Fixture::new().await;
    f.controller.prepare_tool("python").await.unwrap();
    assert!(!std::fs::read_to_string(&f.path)
        .unwrap()
        .contains("current request"));
    f.change("obsolete", "summary");
    f.controller
        .settle_tool("python", ClmToolOutcome::Completed, &inputs())
        .await
        .unwrap();
    assert_eq!(f.revision().await, 1);
    assert!(!f.marker().exists());
    let projection = f.controller.project_request().await.unwrap();
    assert_eq!(
        projection.blocks[0].message["content"],
        json!([{"type":"text","text":"summary"}])
    );
    assert_eq!(projection.blocks[1].message["content"], "current request");
    assert!(std::fs::read_to_string(&f.session)
        .unwrap()
        .contains("obsolete"));
    std::fs::write(&f.path, "unaccepted bytes").unwrap();
    let reopened = Arc::new(Mutex::new(SessionManager::open(
        f.dir.path(),
        f.dir.path(),
        &f.session,
    )));
    let controller = ClmController::new(
        callback(&reopened, &f.protected),
        Some(f.session.clone()),
        f.dir.path().to_path_buf(),
    )
    .await
    .unwrap();
    assert_eq!(
        controller.project_request().await.unwrap().blocks,
        projection.blocks
    );
}

#[tokio::test]
async fn unchanged_action_and_noop_mode_do_not_append() {
    let f = Fixture::new().await;
    let before = std::fs::read(&f.session).unwrap();
    f.controller
        .set_mode(true, ClmModeSource::InheritedChild)
        .await
        .unwrap();
    f.controller.prepare_tool("python").await.unwrap();
    f.controller
        .settle_tool("python", ClmToolOutcome::Completed, &inputs())
        .await
        .unwrap();
    assert_eq!(std::fs::read(&f.session).unwrap(), before);
    assert_eq!(f.revision().await, 0);
    assert!(!f.marker().exists());
    assert_eq!(
        f.controller.status(false).await.unwrap().mode_source,
        ClmStatusSource::SessionOverride
    );
}

#[tokio::test]
async fn rejected_edit_repairs_mirror_and_retains_accepted_revision() {
    let f = Fixture::new().await;
    f.controller.prepare_tool("python").await.unwrap();
    std::fs::write(&f.path, "malformed rejected bytes").unwrap();
    assert!(f
        .controller
        .settle_tool("python", ClmToolOutcome::Completed, &inputs())
        .await
        .is_err());
    assert_eq!(f.revision().await, 0);
    assert!(!f.marker().exists());
    assert_eq!(
        f.controller.project_request().await.unwrap().blocks[0].message["content"],
        "obsolete"
    );
    let rejected = std::fs::read_dir(f.path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().contains(".rejected-"))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(rejected).unwrap(),
        "malformed rejected bytes"
    );
    assert!(f.controller.status(false).await.unwrap().error.is_some());
}

#[tokio::test]
async fn drift_refuses_edit_and_projection_but_view_never_imports_bytes() {
    let f = Fixture::new().await;
    std::fs::write(&f.path, "external bytes").unwrap();
    assert_eq!(
        f.controller.accepted_view().await.unwrap().messages[0]["content"],
        "obsolete"
    );
    assert_eq!(std::fs::read_to_string(&f.path).unwrap(), "external bytes");
    assert!(f.controller.prepare_tool("python").await.is_err());
    assert!(f.controller.project_request().await.is_err());
    assert_eq!(f.revision().await, 0);
}

#[tokio::test]
async fn cancelled_and_unknown_writers_remain_blocked_after_late_completion() {
    for outcome in [ClmToolOutcome::Aborted, ClmToolOutcome::Unknown] {
        let f = Fixture::new().await;
        f.controller.prepare_tool("python").await.unwrap();
        f.change("obsolete", "late bytes");
        assert!(f
            .controller
            .settle_tool("python", outcome, &inputs())
            .await
            .is_err());
        assert!(f
            .controller
            .settle_tool("python", ClmToolOutcome::Completed, &inputs())
            .await
            .is_err());
        let target = ClmRevisionRef {
            epoch_id: f.controller.status(false).await.unwrap().epoch_id.unwrap(),
            revision: 0,
        };
        assert!(f.controller.restore(target).await.is_err());
        assert!(f
            .controller
            .set_mode(false, ClmModeSource::SessionOverride)
            .await
            .is_err());
        assert!(f.controller.project_request().await.is_err());
        assert!(f.marker().exists());
        assert!(std::fs::read_to_string(&f.path)
            .unwrap()
            .contains("late bytes"));
        assert_eq!(f.revision().await, 0);
        assert!(ClmController::new(
            callback(&f.owner, &f.protected),
            Some(f.session.clone()),
            f.dir.path().to_path_buf()
        )
        .await
        .is_err());
    }
}

#[tokio::test]
async fn restore_off_and_new_epoch_use_accepted_context_and_current_watermark() {
    let f = Fixture::new().await;
    let epoch = f.controller.status(false).await.unwrap().epoch_id.unwrap();
    f.controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", "summary");
    f.controller
        .settle_tool("python", ClmToolOutcome::Completed, &inputs())
        .await
        .unwrap();
    f.controller
        .restore(ClmRevisionRef {
            epoch_id: epoch.clone(),
            revision: 0,
        })
        .await
        .unwrap();
    assert_eq!(f.revision().await, 2);
    assert_eq!(
        f.controller.accepted_view().await.unwrap().messages[0]["content"],
        "obsolete"
    );
    let off = f
        .controller
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    assert_eq!((off.mode, off.revision), (ClmMode::Off, Some(3)));
    f.controller.require_off("compact").unwrap();
    let on = f
        .controller
        .set_mode(true, ClmModeSource::InheritedChild)
        .await
        .unwrap();
    assert_ne!(on.epoch_id.as_ref(), Some(&epoch));
    assert_eq!(on.revision, Some(0));
    assert_eq!(on.mode_source, ClmStatusSource::InheritedChild);
    assert!(f.controller.require_off("compact").is_err());
    assert!(f
        .controller
        .restore(ClmRevisionRef {
            epoch_id: epoch,
            revision: 0
        })
        .await
        .is_err());
}

#[tokio::test]
async fn estimates_are_diagnostic_only_and_do_not_authorize_requests() {
    let f = Fixture::new().await;
    let bytes = std::fs::read(&f.session).unwrap();
    for (input, max, warning) in [
        (74, Some(100), false),
        (75, Some(100), true),
        (101, Some(100), true),
        (1000, None, false),
    ] {
        let estimate = ClmRequestBudget {
            estimated_input_tokens: input,
            max_input_tokens: max,
            output_reserve_tokens: None,
        };
        f.controller.record_request_estimate(estimate.clone());
        let status = f.controller.status(true).await.unwrap();
        assert_eq!(status.estimated_occupancy, Some(estimate));
        assert_eq!(
            status.warning.as_deref(),
            warning.then_some("CLM context is near its request budget; shorten stale history")
        );
        assert!(status.configured_default);
        assert!(status.measured_usage.is_none());
        assert_eq!(
            f.controller.project_request().await.unwrap().blocks.len(),
            2
        );
    }
    assert_eq!(std::fs::read(&f.session).unwrap(), bytes);
}

#[tokio::test]
async fn edit_budget_refusal_repairs_without_advancing_and_unknown_window_accepts() {
    let f = Fixture::new().await;
    f.controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", &"x".repeat(4000));
    assert!(matches!(
        f.controller
            .settle_tool("python", ClmToolOutcome::Completed, &inputs())
            .await,
        Err(ClmError::Budget(_))
    ));
    assert_eq!(f.revision().await, 0);
    f.controller.prepare_tool("python2").await.unwrap();
    f.change("obsolete", &"x".repeat(4000));
    let mut unknown = inputs();
    unknown.model.context_window = 0;
    f.controller
        .settle_tool("python2", ClmToolOutcome::Completed, &unknown)
        .await
        .unwrap();
    assert_eq!(f.revision().await, 1);
    assert_eq!(
        f.controller
            .status(false)
            .await
            .unwrap()
            .estimated_occupancy
            .unwrap()
            .max_input_tokens,
        None
    );
}

#[tokio::test]
async fn cancellation_during_acknowledgement_never_republishes_a_cancelled_writer() {
    let f = Fixture::new().await;
    let accepted = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let original = callback(&f.owner, &f.protected);
    let journal: ClmJournalFn = {
        let accepted = accepted.clone();
        let release = release.clone();
        Arc::new(move |op| {
            let commit = matches!(op, crate::clm::journal::ClmJournalOp::Commit(_));
            let original = original.clone();
            let accepted = accepted.clone();
            let release = release.clone();
            Box::pin(async move {
                let reply = original(op).await?;
                if commit {
                    accepted.notify_one();
                    release.notified().await;
                }
                Ok(reply)
            })
        })
    };
    let controller =
        ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
            .await
            .unwrap();
    controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", "summary");
    let budget = inputs();
    let settlement = controller.settle_tool("python", ClmToolOutcome::Completed, &budget);
    tokio::pin!(settlement);
    tokio::select! { () = accepted.notified() => (), result = &mut settlement => panic!("did not reach acknowledgement barrier: {result:?}") }
    let durable_id = f.owner.lock().unwrap().get_leaf_id().unwrap().to_owned();
    println!("cancellation after durable owner success: row={durable_id}; reply still held");
    assert_eq!(controller.state.lock().unwrap().accepted.revision, Some(0));
    let reopened = SessionManager::open(f.dir.path(), f.dir.path(), &f.session);
    assert_eq!(reopened.get_leaf_id(), Some(durable_id.as_str()));
    controller.cancel_tool("python");
    std::fs::write(&f.path, "late physical bytes").unwrap();
    release.notify_one();
    assert!(settlement.await.is_err());
    let status = controller.status(false).await.unwrap();
    assert_eq!(status.revision, Some(1));
    assert_eq!(
        f.owner.lock().unwrap().get_leaf_id(),
        Some(durable_id.as_str())
    );
    assert_eq!(
        std::fs::read_to_string(&f.path).unwrap(),
        "late physical bytes"
    );
    assert!(f.marker().exists());
    assert!(controller.project_request().await.is_err());
    assert!(controller
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .is_err());
    assert!(controller
        .restore(ClmRevisionRef {
            epoch_id: status.epoch_id.unwrap(),
            revision: 0
        })
        .await
        .is_err());
}

#[tokio::test]
async fn missing_owned_marker_cannot_authorize_an_edit() {
    let f = Fixture::new().await;
    f.controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", "summary");
    std::fs::remove_file(f.marker()).unwrap();
    assert!(f
        .controller
        .settle_tool("python", ClmToolOutcome::Completed, &inputs())
        .await
        .is_err());
    assert_eq!(f.revision().await, 0);
}

#[path = "test_io.rs"]
pub(super) mod io;

#[tokio::test]
async fn owner_admission_refusal_repairs_without_poisoning_accepted_state() {
    let f = Fixture::new().await;
    let original = callback(&f.owner, &f.protected);
    let journal: ClmJournalFn = Arc::new(move |op| {
        let original = original.clone();
        Box::pin(async move {
            if matches!(op, crate::clm::journal::ClmJournalOp::Commit(_)) {
                return Err(ClmError::Budget("512 MiB session cap".into()).into());
            }
            original(op).await
        })
    });
    let controller =
        ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
            .await
            .unwrap();
    controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", "summary");
    assert!(matches!(
        controller
            .settle_tool("python", ClmToolOutcome::Completed, &inputs())
            .await,
        Err(ClmError::Budget(_))
    ));
    assert_eq!(controller.status(false).await.unwrap().revision, Some(0));
    assert!(!f.marker().exists());
    assert_eq!(
        controller.project_request().await.unwrap().blocks[0].message["content"],
        "obsolete"
    );
}

#[tokio::test]
async fn unknown_journal_result_never_repairs_or_publishes_acceptance() {
    let f = Fixture::new().await;
    let original = callback(&f.owner, &f.protected);
    let journal: ClmJournalFn = Arc::new(move |op| {
        let original = original.clone();
        Box::pin(async move {
            if matches!(op, crate::clm::journal::ClmJournalOp::Commit(_)) {
                return Err(anyhow::anyhow!("lost commit acknowledgement"));
            }
            original(op).await
        })
    });
    let controller =
        ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
            .await
            .unwrap();
    controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", "uncertain bytes");
    assert!(matches!(
        controller
            .settle_tool("python", ClmToolOutcome::Completed, &inputs())
            .await,
        Err(ClmError::Persistence(_))
    ));
    assert_eq!(controller.status(false).await.unwrap().revision, Some(0));
    assert!(f.marker().exists());
    assert!(std::fs::read_to_string(&f.path)
        .unwrap()
        .contains("uncertain bytes"));
    assert!(controller.project_request().await.is_err());
}

#[tokio::test]
async fn protected_references_and_action_identity_cannot_be_relabelled() {
    for kind in ["missing", "relabelled", "payload", "owner"] {
        let f = Fixture::new().await;
        f.controller.prepare_tool("python").await.unwrap();
        if kind == "owner" {
            assert!(f
                .controller
                .settle_tool("other", ClmToolOutcome::Completed, &inputs())
                .await
                .is_err());
            assert!(f.marker().exists());
            continue;
        }
        if kind == "payload" {
            f.protected.lock().unwrap().get_mut("request").unwrap()[0].message["content"] =
                json!("different");
        } else {
            let id = &f.protected.lock().unwrap()["request"][0].id;
            let marker = format!("[[CTX_PROTECTED id={id}]]");
            let replacement = if kind == "missing" {
                String::new()
            } else {
                format!("[[CTX_TURN id={id} role=user]]\ncurrent request\n[[CTX_END]]")
            };
            f.change(&marker, &replacement);
        }
        assert!(f
            .controller
            .settle_tool("python", ClmToolOutcome::Completed, &inputs())
            .await
            .is_err());
        assert_eq!(f.revision().await, 0);
        if kind == "payload" {
            assert!(f.marker().exists());
        } else {
            assert!(f.controller.project_request().await.is_ok());
        }
    }
}

#[tokio::test]
async fn queued_preparations_admit_exactly_one_writer() {
    let f = Fixture::new().await;
    let (a, b) = tokio::join!(
        f.controller.prepare_tool("a"),
        f.controller.prepare_tool("b")
    );
    assert_eq!((a.is_ok(), b.is_ok()), (true, false));
    assert!(f.controller.project_request().await.is_err());
    f.controller
        .settle_tool("a", ClmToolOutcome::Completed, &inputs())
        .await
        .unwrap();
}

#[tokio::test]
async fn pending_native_group_stays_outside_revision_and_reopens_once() {
    let f = Fixture::new().await;
    let watermark = f
        .controller
        .project_request()
        .await
        .unwrap()
        .state
        .audit_watermark;
    let call = json!({"role":"assistant","content":[{"type":"toolCall","id":"call1","name":"python_repl","arguments":{}},{"type":"toolCall","id":"call2","name":"python_repl","arguments":{}}],"api":"x","provider":"x","model":"x","usage":pa_types::ai::Usage::default(),"stopReason":"toolUse","timestamp":2});
    let receipt = |id| json!({"role":"toolResult","toolCallId":id,"toolName":"python_repl","content":[],"isError":false,"timestamp":3});
    f.owner
        .lock()
        .unwrap()
        .append_message(serde_json::from_value(call).unwrap())
        .unwrap();
    f.owner
        .lock()
        .unwrap()
        .append_message(serde_json::from_value(receipt("call1")).unwrap())
        .unwrap();
    f.controller.prepare_tool("python").await.unwrap();
    assert!(!std::fs::read_to_string(&f.path).unwrap().contains("call1"));
    f.change("obsolete", "summary");
    f.controller
        .settle_tool("python", ClmToolOutcome::Completed, &inputs())
        .await
        .unwrap();
    let projection = f.controller.project_request().await.unwrap();
    assert_eq!(projection.state.audit_watermark, watermark);
    assert_eq!(projection.blocks.len(), 4);
    assert!(f
        .controller
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .is_err());
    let reopened = SessionManager::open(f.dir.path(), f.dir.path(), &f.session);
    assert_eq!(reopened.active_context_checked().unwrap().messages.len(), 4);
    f.owner
        .lock()
        .unwrap()
        .append_message(serde_json::from_value(receipt("call2")).unwrap())
        .unwrap();
    f.controller.prepare_tool("next").await.unwrap();
    assert!(std::fs::read_to_string(&f.path).unwrap().contains("call1"));
    f.controller
        .settle_tool("next", ClmToolOutcome::Completed, &inputs())
        .await
        .unwrap();
    assert_eq!(f.revision().await, 1);
    assert_eq!(
        f.controller.project_request().await.unwrap().blocks.len(),
        5
    );
}

#[tokio::test]
async fn oversized_mirror_edit_is_rejected_and_readonly_context_remains_inspectable() {
    let f = Fixture::new().await;
    f.controller.prepare_tool("python").await.unwrap();
    std::fs::write(&f.path, "x".repeat(crate::clm::MAX_EDITABLE_BYTES + 1)).unwrap();
    assert!(f
        .controller
        .settle_tool("python", ClmToolOutcome::Completed, &inputs())
        .await
        .is_err());
    assert_eq!(f.revision().await, 0);
    f.owner.lock().unwrap().append_message(serde_json::from_value(json!({"role":"user","content":"y".repeat(crate::clm::MAX_EDITABLE_BYTES),"timestamp":4})).unwrap()).unwrap();
    assert!(f.controller.project_request().await.is_err());
    assert!(f.controller.prepare_tool("large").await.is_err());
    std::fs::write(&f.path, "pending bytes").unwrap();
    let view = f.controller.accepted_view().await.unwrap();
    assert_eq!(view.messages.len(), 3);
    assert_eq!(view.revision, 0);
    assert_eq!(std::fs::read_to_string(&f.path).unwrap(), "pending bytes");
}

#[tokio::test]
async fn restore_is_serialized_with_view_and_history_is_last_ten_of_current_epoch() {
    let f = Fixture::new().await;
    let epoch = f.controller.status(false).await.unwrap().epoch_id.unwrap();
    for _ in 0..11 {
        f.controller
            .restore(ClmRevisionRef {
                epoch_id: epoch.clone(),
                revision: 0,
            })
            .await
            .unwrap();
    }
    let (restored, viewed) = tokio::join!(
        f.controller.restore(ClmRevisionRef {
            epoch_id: epoch.clone(),
            revision: 0
        }),
        f.controller.accepted_view()
    );
    assert_eq!(restored.unwrap().revision, Some(12));
    assert_eq!(viewed.unwrap().revision, 12);
    let status = f.controller.status(false).await.unwrap();
    assert_eq!(
        status.recent_revisions,
        (3..=12)
            .rev()
            .map(|revision| ClmRevisionRef {
                epoch_id: epoch.clone(),
                revision
            })
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn accounting_suffix_is_checked_by_owner_at_commit() {
    for kind in ["child", "message", "dropped"] {
        let f = Fixture::new().await;
        let prior = f.owner.lock().unwrap().get_leaf_id().unwrap().to_owned();
        let target = f.owner.lock().unwrap().append_message(serde_json::from_value(json!({"role":"assistant","content":[],"api":"x","provider":"x","model":"x","usage":pa_types::ai::Usage::default(),"stopReason":"stop","timestamp":1})).unwrap()).unwrap();
        let original = callback(&f.owner, &f.protected);
        let owner = f.owner.clone();
        let journal: ClmJournalFn = Arc::new(move |op| {
            let original = original.clone();
            let owner = owner.clone();
            let target = target.clone();
            let prior = prior.clone();
            Box::pin(async move {
                if matches!(op, crate::clm::journal::ClmJournalOp::Commit(_)) {
                    let mut owner = owner.lock().unwrap();
                    if kind == "dropped" {
                        owner.set_leaf_id(Some(&prior));
                    }
                    let usage = pa_types::ai::Usage::default();
                    owner
                        .append_child_usage_attribution("at-commit", &target, usage, usage, None)
                        .unwrap();
                    if kind == "message" {
                        owner
                            .append_message(
                                serde_json::from_value(
                                    json!({"role":"user","content":"unrelated","timestamp":2}),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                original(op).await
            })
        });
        let controller =
            ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
                .await
                .unwrap();
        controller.prepare_tool("python").await.unwrap();
        f.change("obsolete", "summary");
        let result = controller
            .settle_tool("python", ClmToolOutcome::Completed, &inputs())
            .await;
        assert_eq!(result.is_ok(), kind == "child");
        assert_eq!(
            controller.status(false).await.unwrap().revision,
            Some(u64::from(kind == "child"))
        );
        assert_eq!(f.marker().exists(), kind != "child");
    }
}

#[tokio::test]
async fn submission_without_durable_success_is_not_acceptance() {
    let f = Fixture::new().await;
    let submitted = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let original = callback(&f.owner, &f.protected);
    let acknowledged_id = Arc::new(Mutex::new(None));
    let journal: ClmJournalFn = {
        let submitted = submitted.clone();
        let release = release.clone();
        let acknowledged_id = acknowledged_id.clone();
        Arc::new(move |op| {
            let original = original.clone();
            let submitted = submitted.clone();
            let release = release.clone();
            let acknowledged_id = acknowledged_id.clone();
            Box::pin(async move {
                if matches!(op, crate::clm::journal::ClmJournalOp::Commit(_)) {
                    submitted.notify_one();
                    release.notified().await;
                }
                let reply = original(op).await?;
                if let crate::clm::journal::ClmJournalReply::Committed { row, .. } = &reply {
                    *acknowledged_id.lock().unwrap() = row.id().map(str::to_owned);
                }
                Ok(reply)
            })
        })
    };
    let controller =
        ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
            .await
            .unwrap();
    controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", "summary");
    let before = std::fs::read(&f.session).unwrap();
    let budget = inputs();
    let settlement = controller.settle_tool("python", ClmToolOutcome::Completed, &budget);
    tokio::pin!(settlement);
    tokio::select! { () = submitted.notified() => (), result = &mut settlement => panic!("submission did not suspend: {result:?}") }
    assert!(acknowledged_id.lock().unwrap().is_none());
    assert_eq!(std::fs::read(&f.session).unwrap(), before);
    assert_eq!(controller.state.lock().unwrap().accepted.revision, Some(0));
    controller.cancel_tool("python");
    std::fs::write(&f.path, "late physical bytes").unwrap();
    release.notify_one();
    assert!(settlement.await.is_err());
    // The ABI cannot retract already submitted owner work. A later actual durable
    // acknowledgement is recorded, but never authorizes a cancelled writer.
    let id = acknowledged_id.lock().unwrap().clone().unwrap();
    println!("cancellation after submission before owner execution: later acknowledged row={id}");
    assert_eq!(f.owner.lock().unwrap().get_leaf_id(), Some(id.as_str()));
    assert_eq!(controller.status(false).await.unwrap().revision, Some(1));
    assert_eq!(
        std::fs::read_to_string(&f.path).unwrap(),
        "late physical bytes"
    );
    assert!(f.marker().exists());
    assert!(controller.project_request().await.is_err());
    assert!(controller
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .is_err());
    let status = controller.status(false).await.unwrap();
    assert!(controller
        .restore(ClmRevisionRef {
            epoch_id: status.epoch_id.unwrap(),
            revision: 0
        })
        .await
        .is_err());
}

#[tokio::test]
async fn accepted_raw_block_fields_survive_off_and_new_epoch() {
    let f = Fixture::new().await;
    let original = f.controller.project_request().await.unwrap();
    let mut snapshot = original.state.snapshot.unwrap().into_value();
    snapshot["blocks"][0]["future"] = json!({"z":1,"a":2});
    let raw = pa_types::clm::ClmSnapshot::from_value(snapshot.clone());
    {
        let mut owner = f.owner.lock().unwrap();
        let crate::clm::journal::ClmJournalReply::View(view) = owner
            .clm_journal(
                crate::clm::journal::ClmJournalOp::ReadAfterEvents,
                f.protected.lock().unwrap().clone(),
            )
            .unwrap()
        else {
            panic!("view")
        };
        let row = serde_json::from_value(json!({"type":"clm_revision","epochId":original.state.epoch_id,"revision":1,"baseRevision":0,"snapshot":raw,"auditWatermark":original.state.audit_watermark,"observedActionId":"seed","checksum":crate::clm::snapshot_checksum(&raw).unwrap()})).unwrap();
        owner
            .clm_journal(
                crate::clm::journal::ClmJournalOp::Commit(crate::clm::journal::ClmCommit {
                    expected: view,
                    row,
                    allow_child_usage_suffix: false,
                }),
                f.protected.lock().unwrap().clone(),
            )
            .unwrap();
    }
    let controller = ClmController::new(
        callback(&f.owner, &f.protected),
        Some(f.session.clone()),
        f.dir.path().to_path_buf(),
    )
    .await
    .unwrap();
    controller
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    controller
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    let after = controller
        .project_request()
        .await
        .unwrap()
        .state
        .snapshot
        .unwrap();
    assert_eq!(after.as_value()["blocks"][0], snapshot["blocks"][0]);
}

#[tokio::test]
async fn null_watermark_cannot_cover_a_pending_group_and_malformed_groups_refuse() {
    for kind in ["null", "orphan", "reordered", "interrupted"] {
        let f = Fixture::new().await;
        if kind == "null" {
            let snapshot = f
                .controller
                .project_request()
                .await
                .unwrap()
                .state
                .snapshot
                .unwrap();
            f.controller
                .set_mode(false, ClmModeSource::SessionOverride)
                .await
                .unwrap();
            let mut owner = f.owner.lock().unwrap();
            let crate::clm::journal::ClmJournalReply::View(expected) = owner
                .clm_journal(
                    crate::clm::journal::ClmJournalOp::ReadAfterEvents,
                    f.protected.lock().unwrap().clone(),
                )
                .unwrap()
            else {
                panic!("view")
            };
            let row = serde_json::from_value(json!({"type":"clm_on","epochId":"null-epoch","revision":0,"snapshot":snapshot,"auditWatermark":null})).unwrap();
            owner
                .clm_journal(
                    crate::clm::journal::ClmJournalOp::Commit(crate::clm::journal::ClmCommit {
                        expected,
                        row,
                        allow_child_usage_suffix: false,
                    }),
                    f.protected.lock().unwrap().clone(),
                )
                .unwrap();
        }
        let controller = ClmController::new(
            callback(&f.owner, &f.protected),
            Some(f.session.clone()),
            f.dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        if kind != "orphan" {
            f.owner.lock().unwrap().append_message(serde_json::from_value(json!({"role":"assistant","content":[{"type":"toolCall","id":"one","name":"python_repl","arguments":{}},{"type":"toolCall","id":"two","name":"python_repl","arguments":{}}],"api":"x","provider":"x","model":"x","usage":pa_types::ai::Usage::default(),"stopReason":"toolUse","timestamp":2})).unwrap()).unwrap();
        }
        if kind != "null" {
            f.owner.lock().unwrap().append_message(serde_json::from_value(json!({"role":"toolResult","toolCallId":if kind=="reordered" {"two"} else {"one"},"toolName":"python_repl","content":[],"isError":false,"timestamp":3})).unwrap()).unwrap();
        }
        if kind == "interrupted" {
            f.owner
                .lock()
                .unwrap()
                .append_message(
                    serde_json::from_value(
                        json!({"role":"user","content":"unrelated","timestamp":4}),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        if kind == "null" {
            controller.prepare_tool("python").await.unwrap();
            f.change("obsolete", "summary");
            assert!(controller
                .settle_tool("python", ClmToolOutcome::Completed, &inputs())
                .await
                .unwrap_err()
                .to_string()
                .contains("watermark"));
            assert_eq!(controller.project_request().await.unwrap().blocks.len(), 3);
        } else {
            assert!(controller.prepare_tool("python").await.is_err());
        }
        assert_eq!(controller.status(false).await.unwrap().revision, Some(0));
    }
}

#[tokio::test]
async fn status_uses_provider_receipts_and_resets_after_revision() {
    let f = Fixture::new().await;
    let status = f.controller.status(false).await.unwrap();
    let leaf = f.owner.lock().unwrap().get_leaf_id().unwrap().to_owned();
    let usage = json!({"input":4,"output":5,"cacheRead":6,"cacheWrite":7,"totalTokens":22,"cost":{"input":0.1,"output":0.2,"cacheRead":0.3,"cacheWrite":0.4,"total":1.0}});
    let receipt = crate::clm::clm_usage_receipt(
        &pa_types::clm::ClmUsagePin {
            mode: ClmMode::On,
            branch_id: Some(leaf.clone()),
            entry_id: Some(leaf),
            epoch_id: status.epoch_id,
            revision: Some(0),
        },
        &usage,
        None,
    );
    f.owner
        .lock()
        .unwrap()
        .append_custom_entry(
            receipt["customType"].as_str().unwrap(),
            Some(receipt["data"].clone()),
        )
        .unwrap();
    let measured = f
        .controller
        .status(false)
        .await
        .unwrap()
        .measured_usage
        .unwrap();
    assert_eq!(
        measured,
        pa_types::clm::ClmMeasuredUsage {
            input: 4.0,
            output: 5.0,
            cache_read: 6.0,
            cache_write: 7.0,
            cost: 1.0
        }
    );
    f.controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", "summary");
    f.controller
        .settle_tool("python", ClmToolOutcome::Completed, &inputs())
        .await
        .unwrap();
    assert!(f
        .controller
        .status(false)
        .await
        .unwrap()
        .measured_usage
        .is_none());
}

#[tokio::test]
async fn observed_external_mode_change_cannot_leave_synchronous_off_guard_open() {
    let f = Fixture::new().await;
    f.controller
        .set_mode(false, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    let other = ClmController::new(
        callback(&f.owner, &f.protected),
        Some(f.session.clone()),
        f.dir.path().join("other"),
    )
    .await
    .unwrap();
    other
        .set_mode(true, ClmModeSource::SessionOverride)
        .await
        .unwrap();
    assert_eq!(f.controller.status(false).await.unwrap().mode, ClmMode::On);
    assert!(f.controller.require_off("compact").is_err());
}

async fn append_failed_provider(f: &Fixture) -> pa_types::clm::ClmBlock {
    let message = serde_json::from_value(json!({"role":"assistant","content":[{"type":"toolCall","id":"partial","name":"inspect","arguments":{}}],"api":"x","provider":"x","model":"x","usage":pa_types::ai::Usage::default(),"stopReason":"error","timestamp":1})).unwrap();
    let id = f.owner.lock().unwrap().append_message(message).unwrap();
    let crate::clm::journal::ClmJournalReply::View(view) =
        callback(&f.owner, &f.protected)(crate::clm::journal::ClmJournalOp::ReadAfterEvents)
            .await
            .unwrap()
    else {
        panic!("missing acknowledged view")
    };
    let block = crate::clm::project_clm_branch(&view.branch)
        .unwrap()
        .unwrap()
        .blocks
        .pop()
        .unwrap();
    assert_eq!(block.id, id);
    block
}

#[tokio::test]
async fn cleanup_exact_failed_tail_retains_edits_deliveries_and_raw_audit_on_reopen() {
    let f = Fixture::new().await;
    f.controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", "edited note");
    f.controller
        .settle_tool("python", ClmToolOutcome::Completed, &inputs())
        .await
        .unwrap();
    f.owner
        .lock()
        .unwrap()
        .append_message(
            serde_json::from_value(
                json!({"role":"user","content":"valid uncovered delivery","timestamp":2}),
            )
            .unwrap(),
        )
        .unwrap();
    let failed = append_failed_provider(&f).await;
    f.owner
        .lock()
        .unwrap()
        .append_custom_entry("metadata-after-failure", None)
        .unwrap();
    let audit_before = std::fs::read_to_string(&f.session).unwrap();
    let raw_failed = audit_before
        .lines()
        .find(|line| line.contains(&failed.id))
        .unwrap()
        .to_owned();
    f.controller
        .discard_failed_provider_entry(&failed)
        .await
        .unwrap();
    let accepted = f.controller.accepted_view().await.unwrap();
    assert_eq!(accepted.revision, 2);
    let text = serde_json::to_string(&accepted.messages).unwrap();
    assert!(
        text.contains("edited note")
            && text.contains("current request")
            && text.contains("valid uncovered delivery")
    );
    assert!(!text.contains("partial"));
    let audit = std::fs::read_to_string(&f.session).unwrap();
    assert!(audit.starts_with(&audit_before));
    assert_eq!(
        audit
            .lines()
            .find(|line| line.contains(&failed.id))
            .unwrap(),
        raw_failed
    );
    let row: serde_json::Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
    assert_eq!(row["auditWatermark"], failed.id);
    assert_eq!(
        row["observedActionId"],
        format!("failed-provider:{}", failed.id)
    );
    assert_eq!(row["baseRevision"], 1);
    assert_eq!(row["epochId"], accepted.epoch_id);
    assert_eq!(row["id"].as_str(), f.owner.lock().unwrap().get_leaf_id());
    println!(
        "cleanup accepted revision row={} failed audit row={}",
        row["id"], failed.id
    );
    let reopened = Arc::new(Mutex::new(SessionManager::open(
        f.dir.path(),
        f.dir.path(),
        &f.session,
    )));
    let controller = ClmController::new(
        callback(&reopened, &f.protected),
        Some(f.session.clone()),
        f.dir.path().join("reopened"),
    )
    .await
    .unwrap();
    assert_eq!(
        controller.accepted_view().await.unwrap().messages,
        accepted.messages
    );
    assert!(
        !serde_json::to_string(&controller.project_request().await.unwrap().blocks)
            .unwrap()
            .contains("partial")
    );
}

#[tokio::test]
async fn cleanup_unchanged_context_still_covers_failed_audit_and_blocks_without_cleanup() {
    let f = Fixture::new().await;
    let before = f.controller.accepted_view().await.unwrap();
    let failed = append_failed_provider(&f).await;
    assert!(f.controller.project_request().await.is_err());
    assert!(f.controller.prepare_tool("python").await.is_err());
    let reopened = Arc::new(Mutex::new(SessionManager::open(
        f.dir.path(),
        f.dir.path(),
        &f.session,
    )));
    let controller = ClmController::new(
        callback(&reopened, &f.protected),
        Some(f.session.clone()),
        f.dir.path().join("reopened"),
    )
    .await
    .unwrap();
    assert!(controller.project_request().await.is_err());
    assert!(controller.prepare_tool("python").await.is_err());
    f.controller
        .discard_failed_provider_entry(&failed)
        .await
        .unwrap();
    let after = f.controller.accepted_view().await.unwrap();
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.revision, before.revision + 1);
    assert_eq!(
        f.controller
            .project_request()
            .await
            .unwrap()
            .state
            .audit_watermark,
        Some(failed.id)
    );
}

#[tokio::test]
async fn cleanup_rejects_unowned_mismatched_accepted_or_nonfinal_targets() {
    for invalid in [
        "id",
        "message",
        "accepted",
        "empty-protection",
        "bad-protection",
        "not-last",
        "another-error",
        "pending",
    ] {
        let f = Fixture::new().await;
        let accepted = f.controller.project_request().await.unwrap().blocks[0].clone();
        if invalid == "another-error" {
            append_failed_provider(&f).await;
        }
        if invalid == "pending" {
            f.owner.lock().unwrap().append_message(serde_json::from_value(json!({"role":"assistant","content":[{"type":"toolCall","id":"pending","name":"inspect","arguments":{}}],"api":"x","provider":"x","model":"x","usage":pa_types::ai::Usage::default(),"stopReason":"toolUse","timestamp":1})).unwrap()).unwrap();
        }
        let mut failed = append_failed_provider(&f).await;
        match invalid {
            "id" => failed.id = "not-the-durable-id".into(),
            "message" => failed.message["timestamp"] = json!(99),
            "accepted" => failed = accepted,
            "empty-protection" => f.protected.lock().unwrap().clear(),
            "bad-protection" => {
                f.protected.lock().unwrap().get_mut("request").unwrap()[0].message["content"] =
                    json!("not the owned delivery");
            }
            "not-last" => {
                f.owner
                    .lock()
                    .unwrap()
                    .append_message(
                        serde_json::from_value(
                            json!({"role":"user","content":"later valid delivery","timestamp":2}),
                        )
                        .unwrap(),
                    )
                    .unwrap();
            }
            _ => (),
        }
        let audit = std::fs::read(&f.session).unwrap();
        let mirror = std::fs::read(&f.path).unwrap();
        assert!(
            f.controller
                .discard_failed_provider_entry(&failed)
                .await
                .is_err(),
            "{invalid}"
        );
        assert_eq!(f.revision().await, 0, "{invalid}");
        assert_eq!(std::fs::read(&f.session).unwrap(), audit, "{invalid}");
        assert_eq!(std::fs::read(&f.path).unwrap(), mirror, "{invalid}");
    }
}

#[tokio::test]
async fn child_admission_allows_idle_modes_without_importing_or_publishing() {
    for enabled in [true, false] {
        let f = Fixture::new().await;
        f.controller
            .set_mode(enabled, ClmModeSource::SessionOverride)
            .await
            .unwrap();
        let audit = std::fs::read(&f.session).unwrap();
        std::fs::write(&f.path, "unaccepted working bytes").unwrap();
        f.controller
            .assert_child_admission_available(None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&f.session).unwrap(), audit);
        assert_eq!(
            std::fs::read_to_string(&f.path).unwrap(),
            "unaccepted working bytes"
        );
        assert!(!f.marker().exists());
    }
}

#[tokio::test]
async fn child_admission_requires_the_owned_writer_but_allows_ordinary_audit_suffixes() {
    let f = Fixture::new().await;
    assert!(f
        .controller
        .assert_child_admission_available(Some("stale"))
        .await
        .is_err());
    f.controller.prepare_tool("python").await.unwrap();
    assert!(f
        .controller
        .assert_child_admission_available(None)
        .await
        .is_err());
    assert!(f
        .controller
        .assert_child_admission_available(Some("other"))
        .await
        .is_err());
    f.owner
        .lock()
        .unwrap()
        .append_tool_started("python", "ipython", json!({}))
        .unwrap();
    let audit = std::fs::read(&f.session).unwrap();
    let mirror = std::fs::read(&f.path).unwrap();
    f.controller
        .assert_child_admission_available(Some("python"))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&f.session).unwrap(), audit);
    assert_eq!(std::fs::read(&f.path).unwrap(), mirror);
    assert!(f.marker().is_file());
    f.controller.cancel_tool("python");
    assert!(f
        .controller
        .assert_child_admission_available(Some("python"))
        .await
        .is_err());
    assert!(f
        .controller
        .assert_child_admission_available(None)
        .await
        .is_err());
}

#[tokio::test]
async fn child_admission_rechecks_cancellation_after_the_owner_acknowledges() {
    let f = Fixture::new().await;
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let original = callback(&f.owner, &f.protected);
    let journal: ClmJournalFn = Arc::new({
        let armed = armed.clone();
        let reached = reached.clone();
        let release = release.clone();
        move |op| {
            let original = original.clone();
            let armed = armed.clone();
            let reached = reached.clone();
            let release = release.clone();
            Box::pin(async move {
                let reply = original(op).await?;
                if armed.load(std::sync::atomic::Ordering::SeqCst) {
                    reached.notify_one();
                    release.notified().await;
                }
                Ok(reply)
            })
        }
    });
    let controller =
        ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
            .await
            .unwrap();
    controller.prepare_tool("python").await.unwrap();
    let audit = std::fs::read(&f.session).unwrap();
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let admission = controller.assert_child_admission_available(Some("python"));
    tokio::pin!(admission);
    tokio::select! { ()=reached.notified()=>(), result=&mut admission=>panic!("admission skipped the owner barrier: {result:?}") }
    controller.cancel_tool("python");
    release.notify_one();
    assert!(admission.await.is_err());
    assert!(f.marker().is_file());
    assert_eq!(std::fs::read(&f.session).unwrap(), audit);
}

#[tokio::test]
async fn child_admission_rejects_missing_or_nonregular_owned_markers() {
    for directory in [false, true] {
        let f = Fixture::new().await;
        f.controller.prepare_tool("python").await.unwrap();
        std::fs::remove_file(f.marker()).unwrap();
        if directory {
            std::fs::create_dir(f.marker()).unwrap();
        }
        let audit = std::fs::read(&f.session).unwrap();
        assert!(f
            .controller
            .assert_child_admission_available(Some("python"))
            .await
            .is_err());
        assert!(f
            .controller
            .assert_child_admission_available(None)
            .await
            .is_err());
        assert_eq!(std::fs::read(&f.session).unwrap(), audit);
        assert_eq!(f.marker().is_dir(), directory);
    }
}

#[tokio::test]
async fn child_admission_rejects_changed_identity_accepted_state_or_uncertain_owner() {
    for fault in ["session", "generation", "accepted", "uncertain"] {
        let f = Fixture::new().await;
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let original = callback(&f.owner, &f.protected);
        let journal: ClmJournalFn = Arc::new({
            let armed = armed.clone();
            move |op| {
                let original = original.clone();
                let armed = armed.clone();
                Box::pin(async move {
                    let mut reply = original(op).await?;
                    if armed.load(std::sync::atomic::Ordering::SeqCst) {
                        if fault == "uncertain" {
                            anyhow::bail!("owner persistence uncertain");
                        }
                        let crate::clm::journal::ClmJournalReply::View(view) = &mut reply else {
                            panic!("admission must only read")
                        };
                        match fault {
                            "session" => view.session_id.push_str("-changed"),
                            "generation" => view.generation.push_str("-changed"),
                            "accepted" => {
                                let pa_types::session::FileEntry::ClmOn { payload, .. } =
                                    view.branch.last_mut().unwrap()
                                else {
                                    panic!("fixture has an on boundary")
                                };
                                payload.boundary.epoch_id.push_str("-changed");
                            }
                            _ => unreachable!(),
                        }
                    }
                    Ok(reply)
                })
            }
        });
        let controller =
            ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
                .await
                .unwrap();
        controller.prepare_tool("python").await.unwrap();
        armed.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            controller
                .assert_child_admission_available(Some("python"))
                .await
                .is_err(),
            "{fault}"
        );
        assert!(f.marker().is_file());
    }
}
