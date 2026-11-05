use super::*;
use pa_core::clm::{
    journal::{ClmCommit, ClmJournalOp, ClmJournalReply},
    ProtectedActions,
};
use serde_json::json;
use std::io::Write;
fn on() -> pa_types::session::FileEntry {
    serde_json::from_value(json!({"type":"clm_on","epochId":"e","revision":0,"snapshot":{"blocks":[]},"auditWatermark":null})).unwrap()
}
fn view(store: &mut SessionFile) -> pa_core::clm::journal::ClmJournalView {
    let ClmJournalReply::View(view) = store
        .clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new())
        .unwrap()
    else {
        panic!("view");
    };
    view
}
#[test]
fn legacy_unknown_root_envelope_comparison_preserves_bytes_and_detects_changes() {
    let text = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"cwd\":\"/tmp\",\"timestamp\":\"now\"}\n",
        "{\"type\":\"session_info\",\"id\":\"meta\",\"parentId\":null,\"timestamp\":\"now\",\"name\":\"legacy\"}\n",
        "{\"type\":\"message\",\"id\":\"a\",\"timestamp\":\"now\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"audit\"}],\"timestamp\":1}}\n"
    );
    for change in ["body", "parent", "payload_null", "none"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.jsonl");
        std::fs::write(&path, text).unwrap();
        let mut store = SessionFile::open(&path).unwrap();
        match change {
            "body" => store.entries[1].fields["message"]["content"] = json!("changed"),
            "parent" => store.entries[1].parent_id = Some("meta".to_owned()),
            "payload_null" => store.entries[1].fields["message"]["parentId"] = Value::Null,
            _ => {}
        }
        let result = store.clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new());
        if change == "none" {
            let ClmJournalReply::View(view) = result.expect("equivalent absent/null root envelope")
            else {
                panic!("expected read acknowledgment");
            };
            assert_eq!(view.branch.last().unwrap().id(), Some("a"));
        } else {
            assert!(result.is_err(), "must reject {change}");
        }
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes(), "{change}");
    }
}

#[test]
fn forked_usage_receipts_survive_strict_owner_reading() {
    for observed in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jsonl");
        let mut store = SessionFile::create("/tmp", None, 0);
        store.set_path(source.clone());
        let user_id = store
            .persist_entry(
                "message",
                json!({
                    "message":{"role":"user","content":"seed","timestamp":1}
                }),
            )
            .unwrap();
        let usage = serde_json::to_value(pa_types::ai::Usage {
            input: 1,
            total_tokens: 1,
            ..Default::default()
        })
        .unwrap();
        let assistant_id = store
            .persist_entry(
                "message",
                json!({
                    "message":{"role":"assistant","content":[{"type":"text","text":"answer"}],
                        "api":"faux","provider":"faux","model":"faux-1","usage":usage,
                        "stopReason":"stop","timestamp":2}
                }),
            )
            .unwrap();
        let pin = pa_types::clm::ClmUsagePin {
            mode: pa_types::clm::ClmMode::Off,
            branch_id: Some(user_id),
            epoch_id: None,
            revision: None,
            entry_id: None,
        };
        let mut receipt = pa_core::clm::clm_usage_receipt(
            &pin,
            if observed { &usage } else { &Value::Null },
            Some(&assistant_id),
        );
        assert_eq!(
            receipt["customType"],
            if observed {
                "clm_usage_v1"
            } else {
                "clm_usage_unknown_v1"
            }
        );
        receipt["future"] = json!({"opaque":"kept"});
        receipt["data"]["future"] = json!([1, "kept"]);
        let receipt_id = store.persist_entry("custom", receipt).unwrap();
        store
            .persist_entry("session_state", json!({"state":{"status":"active"}}))
            .unwrap();
        let before = std::fs::read(&source).unwrap();
        let forked =
            pa_core::session::manager::SessionManager::fork_from(&source, dir.path(), dir.path())
                .unwrap();
        let reopened = SessionFile::open(forked.get_session_file().unwrap()).unwrap();
        assert_eq!(reopened.skipped_lines, 0, "observed={observed}");
        assert_eq!(reopened.messages().len(), 2);
        let original_receipt = store
            .entries()
            .iter()
            .find(|entry| entry.id == receipt_id)
            .unwrap();
        let copied_receipt = reopened
            .entries()
            .iter()
            .find(|entry| entry.id == receipt_id)
            .unwrap();
        assert_eq!(
            serde_json::to_value(copied_receipt).unwrap(),
            serde_json::to_value(original_receipt).unwrap()
        );
        assert_eq!(std::fs::read(&source).unwrap(), before);
    }
}

#[test]
fn acknowledged_rows_reopen_with_exact_identity_and_no_audit_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut store = SessionFile::create("/tmp", None, 0);
    store.set_path(path.clone());
    let id = store
        .persist_entry(
            "message",
            json!({"message":{"role":"user","content":"obsolete","timestamp":1}}),
        )
        .unwrap();
    let expected = view(&mut store);
    let mut row = serde_json::to_value(on()).unwrap();
    row["auditWatermark"] = json!(id);
    let ClmJournalReply::Committed {
        row,
        view: accepted,
    } = store
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: serde_json::from_value(row).unwrap(),
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap()
    else {
        panic!("commit");
    };
    assert_eq!(row.id(), store.leaf_id());
    assert!(store.model_context_checked().unwrap().is_empty());
    assert_eq!(store.messages()[0]["content"], "obsolete");
    let mut reopened = SessionFile::open_windowed(&path).unwrap();
    assert!(reopened.model_context_checked().unwrap().is_empty());
    assert_eq!(view(&mut reopened).branch, accepted.branch);
    assert!(std::fs::read_to_string(&path).unwrap().contains("obsolete"));
}
#[test]
fn malformed_known_row_and_parse_damage_keep_audit_but_refuse_model_context() {
    for tail in [
        "{\"type\":\"clm_on\",\"id\":\"c\",\"parentId\":\"u\",\"timestamp\":\"now\"}\n",
        "{\"type\":\"clm_on\",\"snapshot\":",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut store = SessionFile::create("/tmp", None, 0);
        store.set_path(path.clone());
        store
            .persist_entry(
                "message",
                json!({"message":{"role":"user","content":"audit","timestamp":1}}),
            )
            .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(tail.as_bytes())
            .unwrap();
        let mut reopened = SessionFile::open_windowed(&path).unwrap();
        assert!(!reopened.entries().is_empty());
        assert!(reopened.model_context_checked().is_err());
        let _audit_messages = reopened.messages();
        assert!(reopened
            .clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new())
            .is_err());
    }
}

#[test]
fn failed_clm_write_keeps_leaf_and_blocks_checked_reads_and_later_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blocked");
    let mut store = SessionFile::create("/tmp", None, 0);
    store.set_path(path.clone());
    let expected = view(&mut store);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(store
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: on(),
                allow_child_usage_suffix: false
            }),
            ProtectedActions::new()
        )
        .is_err());
    assert_eq!(store.leaf_id(), None);
    assert!(store.entries().is_empty());
    assert!(store.model_context_checked().is_err());
    assert!(store
        .clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new())
        .is_err());
}

#[test]
fn journal_read_acknowledges_buffered_creation_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("buffered.jsonl");
    let mut store = SessionFile::create("/tmp", None, 0);
    store.set_path(path.clone());
    let id = store.append_message(&json!({"role":"user","content":"delivered","timestamp":1}));
    let acknowledged = view(&mut store);
    assert!(path.exists());
    assert_eq!(
        SessionFile::open(&path).unwrap().leaf_id(),
        Some(id.as_str())
    );
    assert_eq!(acknowledged.leaf_id.as_deref(), Some(id.as_str()));
}

#[test]
fn damaged_clm_keeps_audit_and_restored_model_settings_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("damaged-settings.jsonl");
    let mut store = SessionFile::create("/tmp", None, 0);
    store.set_path(path.clone());
    let model = store
        .persist_entry("model_change", json!({"provider":"p","modelId":"m"}))
        .unwrap();
    store.persist_entry("clm_on",json!({"epochId":"e","revision":0,"auditWatermark":null,"snapshot":{"blocks":"invalid"}})).unwrap();
    let reopened = SessionFile::open(&path).unwrap();
    assert!(reopened.model_context_checked().is_err());
    assert!(reopened.entry(&model).is_some());
    assert_eq!(
        reopened.restored_settings().model,
        Some(("p".into(), "m".into()))
    );
}

#[test]
fn authoritative_envelope_cannot_overwrite_the_proposed_raw_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SessionFile::create("/tmp", None, 0);
    store.set_path(dir.path().join("edited.jsonl"));
    let expected = view(&mut store);
    let mut proposed = on();
    let pa_types::session::FileEntry::ClmOn { payload, .. } = &mut proposed else {
        panic!("on");
    };
    payload.boundary.snapshot = pa_types::clm::ClmSnapshot::from_value(
        json!({"futureSnapshotField":{"version":2},"blocks":[{"id":"accepted","message":{"role":"user","content":"proposed context","timestamp":1}}]}),
    );
    let desired_snapshot = payload.boundary.snapshot.clone();
    store
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: proposed,
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap();
    assert_eq!(
        store.model_context_checked().unwrap(),
        vec![json!({"role":"user","content":"proposed context","timestamp":1})]
    );
    let accepted = store.branch_file_entries_checked().unwrap();
    let pa_types::session::FileEntry::ClmOn { payload, .. } = &accepted[0] else {
        panic!("on");
    };
    assert_eq!(payload.boundary.snapshot, desired_snapshot);
    assert_eq!(
        pa_core::clm::snapshot_checksum(&payload.boundary.snapshot).unwrap(),
        pa_core::clm::snapshot_checksum(&desired_snapshot).unwrap()
    );
    let accepted_snapshot = pa_core::clm::project_clm_branch(&accepted)
        .unwrap()
        .unwrap()
        .blocks;
    let reopened = SessionFile::open(&store.path).unwrap();
    assert_eq!(reopened.leaf_id(), store.leaf_id());
    assert_eq!(reopened.branch_file_entries_checked().unwrap(), accepted);
    let parsed =
        pa_core::session::clm::parse_session_entries_checked(&std::fs::read(&store.path).unwrap())
            .unwrap();
    assert_eq!(
        pa_core::clm::project_clm_branch(&parsed[1..])
            .unwrap()
            .unwrap()
            .blocks,
        accepted_snapshot
    );
}
