use super::*;
use std::{collections::HashMap, path::Path, sync::OnceLock};
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Stage {
    Read,
    Publish,
    MirrorFileSync,
    MirrorRename,
    MirrorDirectorySync,
    MarkerFileSync,
    MarkerDirectorySync,
    MarkerRemovalSync,
}
#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
}
struct Injection {
    gate: Option<Arc<Gate>>,
    failures: usize,
}
static INJECTIONS: OnceLock<Mutex<HashMap<(PathBuf, Stage), Injection>>> = OnceLock::new();
fn injections() -> &'static Mutex<HashMap<(PathBuf, Stage), Injection>> {
    INJECTIONS.get_or_init(Mutex::default)
}
fn pause_at(path: &Path, stage: Stage) -> Arc<Gate> {
    let gate = Arc::new(Gate::default());
    injections().lock().unwrap().insert(
        (path.to_path_buf(), stage),
        Injection {
            gate: Some(gate.clone()),
            failures: 0,
        },
    );
    gate
}
fn fail_at(path: &Path, stage: Stage, failures: usize) {
    injections().lock().unwrap().insert(
        (path.to_path_buf(), stage),
        Injection {
            gate: None,
            failures,
        },
    );
}
pub(crate) fn fault(path: &Path, stage: Stage) -> std::io::Result<()> {
    let mut entries = injections().lock().unwrap();
    if let Some(entry) = entries.get_mut(&(path.to_path_buf(), stage)) {
        if entry.failures > 0 {
            entry.failures -= 1;
            return Err(std::io::Error::other(format!("injected {stage:?}")));
        }
    }
    Ok(())
}
pub(crate) async fn pause(path: &Path, stage: Stage) {
    let gate = injections()
        .lock()
        .unwrap()
        .get_mut(&(path.to_path_buf(), stage))
        .and_then(|entry| entry.gate.take());
    if let Some(gate) = gate {
        gate.entered.notify_one();
        gate.release.notified().await;
    }
}
fn assistant(f: &Fixture) -> String {
    f.owner.lock().unwrap().append_message(serde_json::from_value(json!({"role":"assistant","content":[],"api":"x","provider":"x","model":"x","usage":pa_types::ai::Usage::default(),"stopReason":"stop","timestamp":1})).unwrap()).unwrap()
}
fn child(f: &Fixture, target: &str, name: &str) {
    let usage = pa_types::ai::Usage {
        output: 3,
        ..Default::default()
    };
    f.owner
        .lock()
        .unwrap()
        .append_child_usage_attribution(name, target, usage, usage, None)
        .unwrap();
}

#[tokio::test]
async fn cancelled_awaited_mirror_read_does_not_commit_or_repair() {
    let f = Fixture::new().await;
    f.controller.prepare_tool("python").await.unwrap();
    f.change("obsolete", "unaccepted late bytes");
    let gate = pause_at(&f.path, Stage::Read);
    let budget = inputs();
    let settlement = f
        .controller
        .settle_tool("python", ClmToolOutcome::Completed, &budget);
    tokio::pin!(settlement);
    tokio::select! { () = gate.entered.notified() => (), result = &mut settlement => panic!("mirror read did not suspend: {result:?}") }
    f.controller.cancel_tool("python");
    gate.release.notify_one();
    assert!(settlement.await.is_err());
    assert_eq!(f.revision().await, 0);
    assert!(std::fs::read_to_string(&f.path)
        .unwrap()
        .contains("unaccepted late bytes"));
    assert!(f.marker().exists());
    assert!(f.controller.project_request().await.is_err());
}

#[tokio::test]
async fn child_accounting_is_allowed_at_preparation_and_read_fences() {
    for stage in [Stage::Publish, Stage::Read] {
        let f = Fixture::new().await;
        let target = assistant(&f);
        if stage == Stage::Read {
            f.controller.prepare_tool("python").await.unwrap();
            f.change("obsolete", "summary");
        }
        let gate = pause_at(&f.path, stage);
        let budget = inputs();
        let operation = async {
            if stage == Stage::Publish {
                f.controller.prepare_tool("python").await
            } else {
                f.controller
                    .settle_tool("python", ClmToolOutcome::Completed, &budget)
                    .await
            }
        };
        tokio::pin!(operation);
        tokio::select! { () = gate.entered.notified() => (), result = &mut operation => panic!("fence did not suspend: {result:?}") }
        child(&f, &target, "a");
        child(&f, &target, "b");
        gate.release.notify_one();
        operation.await.unwrap();
        if stage == Stage::Publish {
            f.controller
                .settle_tool("python", ClmToolOutcome::Completed, &budget)
                .await
                .unwrap();
            assert_eq!(f.revision().await, 0);
        } else {
            assert_eq!(f.revision().await, 1);
        }
        assert!(!f.marker().exists());
        assert!(f
            .controller
            .status(false)
            .await
            .unwrap()
            .measured_usage
            .is_none());
    }
}

#[tokio::test]
async fn nonaccounting_suffix_or_dropped_ancestry_rejects_at_each_fence() {
    for stage in [Stage::Publish, Stage::Read] {
        for drop_leaf in [false, true] {
            let f = Fixture::new().await;
            let before = f.owner.lock().unwrap().get_leaf_id().unwrap().to_owned();
            let target = assistant(&f);
            if stage == Stage::Read {
                f.controller.prepare_tool("python").await.unwrap();
                f.change("obsolete", "summary");
            }
            let gate = pause_at(&f.path, stage);
            let budget = inputs();
            let operation = async {
                if stage == Stage::Publish {
                    f.controller.prepare_tool("python").await
                } else {
                    f.controller
                        .settle_tool("python", ClmToolOutcome::Completed, &budget)
                        .await
                }
            };
            tokio::pin!(operation);
            tokio::select! { () = gate.entered.notified() => (), result = &mut operation => panic!("fence did not suspend: {result:?}") }
            if drop_leaf {
                f.owner.lock().unwrap().set_leaf_id(Some(&before));
            }
            child(&f, &target, "child");
            if !drop_leaf {
                f.owner
                    .lock()
                    .unwrap()
                    .append_custom_entry("unrelated", None)
                    .unwrap();
            }
            gate.release.notify_one();
            assert!(operation.await.is_err());
            assert_eq!(f.revision().await, 0);
            assert!(f.controller.project_request().await.is_err());
        }
    }
}

#[tokio::test]
async fn marker_sync_failures_remain_uncertain_and_block_restart() {
    for stage in [
        Stage::MarkerFileSync,
        Stage::MarkerDirectorySync,
        Stage::MarkerRemovalSync,
    ] {
        let f = Fixture::new().await;
        if stage == Stage::MarkerRemovalSync {
            f.controller.prepare_tool("python").await.unwrap();
        }
        fail_at(&f.path, stage, 1);
        let result = if stage == Stage::MarkerRemovalSync {
            f.controller
                .settle_tool("python", ClmToolOutcome::Completed, &inputs())
                .await
        } else {
            f.controller.prepare_tool("python").await
        };
        assert!(result.is_err());
        assert!(f.marker().exists());
        assert_eq!(f.revision().await, 0);
        assert!(f.controller.project_request().await.is_err());
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
async fn mirror_sync_and_rename_failures_do_not_fabricate_acceptance() {
    for stage in [
        Stage::MirrorFileSync,
        Stage::MirrorRename,
        Stage::MirrorDirectorySync,
    ] {
        let f = Fixture::new().await;
        fail_at(&f.path, stage, 1);
        assert!(f.controller.prepare_tool("python").await.is_err());
        assert_eq!(f.revision().await, 0);
        assert!(!f.marker().exists());
    }
}

#[tokio::test]
async fn acknowledged_revision_is_reported_when_mirror_repair_fails() {
    for failures in [1, 2] {
        let f = Fixture::new().await;
        f.controller.prepare_tool("python").await.unwrap();
        f.change("obsolete", "summary");
        fail_at(&f.path, Stage::MirrorRename, failures);
        let result = f
            .controller
            .settle_tool("python", ClmToolOutcome::Completed, &inputs())
            .await;
        assert_eq!(result.is_ok(), failures == 1);
        assert_eq!(f.revision().await, 1);
        let owner = SessionManager::open(f.dir.path(), f.dir.path(), &f.session);
        assert_eq!(owner.active_context_checked().unwrap().messages.len(), 2);
        if failures == 2 {
            assert!(f.marker().exists());
            assert!(f.controller.project_request().await.is_err());
            assert!(f
                .controller
                .status(false)
                .await
                .unwrap()
                .error
                .unwrap()
                .contains("mirror"));
        } else {
            assert!(!f.marker().exists());
            assert!(f.controller.project_request().await.is_ok());
        }
    }
}

#[tokio::test]
async fn cleanup_rejects_active_cancelled_unknown_markers_and_drift() {
    for state in ["active", "cancelled", "unknown-marker", "drift"] {
        let f = Fixture::new().await;
        match state {
            "active" | "cancelled" => {
                f.controller.prepare_tool("python").await.unwrap();
                if state == "cancelled" {
                    f.controller.cancel_tool("python");
                }
            }
            "unknown-marker" => std::fs::write(f.marker(), "unknown writer").unwrap(),
            "drift" => std::fs::write(&f.path, "unowned bytes").unwrap(),
            _ => unreachable!(),
        }
        let failed = append_failed_provider(&f).await;
        let audit = std::fs::read(&f.session).unwrap();
        let mirror = std::fs::read(&f.path).unwrap();
        assert!(
            f.controller
                .discard_failed_provider_entry(&failed)
                .await
                .is_err(),
            "{state}"
        );
        assert_eq!(f.revision().await, 0);
        assert_eq!(std::fs::read(&f.session).unwrap(), audit);
        assert_eq!(std::fs::read(&f.path).unwrap(), mirror);
        assert_eq!(f.marker().exists(), state != "drift");
    }
}

#[tokio::test]
async fn cleanup_strict_commit_rejects_racing_accounting_metadata_and_protection() {
    for change in ["accounting", "metadata", "protection"] {
        let f = Fixture::new().await;
        let target = assistant(&f);
        let original = callback(&f.owner, &f.protected);
        let owner = f.owner.clone();
        let protected = f.protected.clone();
        let journal: ClmJournalFn = Arc::new(move |op| {
            let original = original.clone();
            let owner = owner.clone();
            let protected = protected.clone();
            let target = target.clone();
            Box::pin(async move {
                if let crate::clm::journal::ClmJournalOp::Commit(_) = &op {
                    match change {
                        "accounting" => {
                            let usage = pa_types::ai::Usage {
                                output: 3,
                                ..Default::default()
                            };
                            owner
                                .lock()
                                .unwrap()
                                .append_child_usage_attribution(
                                    "racing-child",
                                    &target,
                                    usage,
                                    usage,
                                    None,
                                )
                                .unwrap();
                        }
                        "metadata" => {
                            owner
                                .lock()
                                .unwrap()
                                .append_custom_entry("racing-metadata", None)
                                .unwrap();
                        }
                        "protection" => {
                            protected.lock().unwrap().clear();
                        }
                        _ => unreachable!(),
                    }
                }
                original(op).await
            })
        });
        let controller =
            ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
                .await
                .unwrap();
        controller.project_request().await.unwrap();
        let failed = append_failed_provider(&f).await;
        let mirror = std::fs::read(&f.path).unwrap();
        assert!(
            controller
                .discard_failed_provider_entry(&failed)
                .await
                .is_err(),
            "{change}"
        );
        assert_eq!(controller.status(false).await.unwrap().revision, Some(0));
        assert_eq!(std::fs::read(&f.path).unwrap(), mirror);
        let audit = std::fs::read_to_string(&f.session).unwrap();
        assert!(!audit
            .lines()
            .any(
                |line| serde_json::from_str::<serde_json::Value>(line).unwrap()["type"]
                    == "clm_revision"
            ));
    }
}

#[tokio::test]
async fn cleanup_budget_refusal_keeps_accepted_state_and_can_retry() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::new().await;
    let original = callback(&f.owner, &f.protected);
    let reject = Arc::new(AtomicBool::new(true));
    let refuse = reject.clone();
    let journal: ClmJournalFn = Arc::new(move |op| {
        let original = original.clone();
        let refuse = refuse.clone();
        Box::pin(async move {
            if matches!(op, crate::clm::journal::ClmJournalOp::Commit(_))
                && refuse.load(Ordering::SeqCst)
            {
                return Err(ClmError::Budget(
                    "authoritative full-history admission refusal".into(),
                )
                .into());
            }
            original(op).await
        })
    });
    let controller =
        ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
            .await
            .unwrap();
    controller.project_request().await.unwrap();
    let failed = append_failed_provider(&f).await;
    let audit = std::fs::read(&f.session).unwrap();
    let mirror = std::fs::read(&f.path).unwrap();
    assert!(matches!(
        controller.discard_failed_provider_entry(&failed).await,
        Err(ClmError::Budget(_))
    ));
    assert_eq!(controller.status(false).await.unwrap().revision, Some(0));
    assert_eq!(std::fs::read(&f.session).unwrap(), audit);
    assert_eq!(std::fs::read(&f.path).unwrap(), mirror);
    reject.store(false, Ordering::SeqCst);
    controller
        .discard_failed_provider_entry(&failed)
        .await
        .unwrap();
    assert_eq!(controller.status(false).await.unwrap().revision, Some(1));
}

#[tokio::test]
async fn cleanup_mirror_failures_keep_the_acknowledged_revision_and_repair_only_it() {
    for stage in [
        Stage::MirrorFileSync,
        Stage::MirrorRename,
        Stage::MirrorDirectorySync,
    ] {
        for failures in [1, 2] {
            let f = Fixture::new().await;
            f.owner
                .lock()
                .unwrap()
                .append_message(
                    serde_json::from_value(
                        json!({"role":"user","content":"retained delivery","timestamp":2}),
                    )
                    .unwrap(),
                )
                .unwrap();
            let failed = append_failed_provider(&f).await;
            fail_at(&f.path, stage, failures);
            let result = f.controller.discard_failed_provider_entry(&failed).await;
            assert_eq!(result.is_ok(), failures == 1, "{stage:?}");
            assert_eq!(f.revision().await, 1);
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
            let projection = controller.project_request().await.unwrap();
            assert_eq!(projection.state.revision, Some(1));
            let text = serde_json::to_string(&projection.blocks).unwrap();
            assert!(text.contains("retained delivery") && !text.contains("partial"));
            if failures == 2 {
                assert!(f.controller.project_request().await.is_err());
                assert!(f
                    .controller
                    .status(false)
                    .await
                    .unwrap()
                    .error
                    .unwrap()
                    .contains("mirror"));
            } else {
                assert!(std::fs::read_to_string(&f.path)
                    .unwrap()
                    .contains("retained delivery"));
                assert!(f.controller.project_request().await.is_ok());
            }
        }
    }
}

#[tokio::test]
async fn cleanup_refusal_after_reopen_does_not_publish_unacknowledged_candidate() {
    let f = Fixture::new().await;
    f.owner
        .lock()
        .unwrap()
        .append_message(
            serde_json::from_value(
                json!({"role":"user","content":"uncovered delivery","timestamp":2}),
            )
            .unwrap(),
        )
        .unwrap();
    let failed = append_failed_provider(&f).await;
    let original = callback(&f.owner, &f.protected);
    let journal: ClmJournalFn = Arc::new(move |op| {
        let original = original.clone();
        Box::pin(async move {
            if matches!(op, crate::clm::journal::ClmJournalOp::Commit(_)) {
                return Err(ClmError::Budget("owner admission refusal".into()).into());
            }
            original(op).await
        })
    });
    let controller =
        ClmController::new(journal, Some(f.session.clone()), f.dir.path().to_path_buf())
            .await
            .unwrap();
    let before = std::fs::read(&f.path).unwrap();
    assert!(matches!(
        controller.discard_failed_provider_entry(&failed).await,
        Err(ClmError::Budget(_))
    ));
    assert_eq!(controller.status(false).await.unwrap().revision, Some(0));
    assert_eq!(
        std::fs::read(&f.path).unwrap(),
        before,
        "only acknowledged context may initialize a reopened mirror"
    );
}

#[tokio::test]
async fn cleanup_allows_accounting_already_in_the_captured_view() {
    let f = Fixture::new().await;
    let target = assistant(&f);
    child(&f, &target, "already-acknowledged-child");
    let failed = append_failed_provider(&f).await;
    f.owner
        .lock()
        .unwrap()
        .append_custom_entry("acknowledged-metadata", None)
        .unwrap();
    f.controller
        .discard_failed_provider_entry(&failed)
        .await
        .unwrap();
    let projection = f.controller.project_request().await.unwrap();
    assert_eq!(projection.state.revision, Some(1));
    assert!(projection.blocks.iter().any(|b| b.id == target));
    assert!(!projection.blocks.iter().any(|b| b.id == failed.id));
}

#[tokio::test]
async fn cleanup_rejects_oversized_candidate_without_covering_failure() {
    let f = Fixture::new().await;
    f.owner.lock().unwrap().append_message(serde_json::from_value(json!({"role":"user","content":"x".repeat(crate::clm::MAX_EDITABLE_BYTES),"timestamp":2})).unwrap()).unwrap();
    let failed = append_failed_provider(&f).await;
    let length = std::fs::metadata(&f.session).unwrap().len();
    assert!(matches!(
        f.controller.discard_failed_provider_entry(&failed).await,
        Err(ClmError::Document(_))
    ));
    assert_eq!(f.revision().await, 0);
    assert_eq!(std::fs::metadata(&f.session).unwrap().len(), length);
}
