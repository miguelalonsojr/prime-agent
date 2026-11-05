use super::*;
use serde_json::json;

fn row(value: serde_json::Value) -> FileEntry {
    serde_json::from_value(value).unwrap()
}

#[test]
fn clm_legacy_root_sentinel_preserves_ordinary_owner_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.jsonl");
    let text = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"cwd\":\"/tmp\",\"timestamp\":\"now\"}\n",
        "{\"type\":\"message\",\"id\":\"u\",\"parentId\":\"\",\"message\":{\"role\":\"user\",\"content\":\"legacy\",\"timestamp\":1}}\n"
    );
    std::fs::write(&path, text).unwrap();
    let mut owner = manager::SessionManager::open(dir.path(), dir.path(), &path);
    let accepted = view(&mut owner);
    assert_eq!(accepted.branch.len(), 1);
    assert_eq!(accepted.branch[0].parent_id(), Some(""));
    assert_eq!(owner.active_context_checked().unwrap().messages.len(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
}

#[test]
fn clm_legacy_root_compatibility_keeps_strict_ancestry_errors() {
    let user = |id: &str, parent: &str| {
        row(json!({
            "type":"message", "id":id, "parentId":parent,
            "message":{"role":"user","content":"audit","timestamp":1}
        }))
    };
    let mut with_boundary = vec![user("u", "")];
    with_boundary.push(row(json!({
        "type":"clm_on", "id":"on", "parentId":"u", "epochId":"e",
        "revision":0, "snapshot":{"blocks":[]}, "auditWatermark":"u"
    })));
    for (entries, leaf) in [
        (vec![user("u", "missing")], "u"),
        (vec![user("u", "u")], "u"),
        (vec![user("u", ""), user("u", "")], "u"),
        (with_boundary, "on"),
    ] {
        assert!(clm::selected_branch(&entries, Some(leaf)).is_err());
    }
}

#[test]
fn clm_accepted_boundary_excludes_deleted_audit() {
    let entries = vec![
        row(
            json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":"obsolete","timestamp":1}}),
        ),
        row(
            json!({"type":"clm_on","id":"on","parentId":"u","epochId":"epoch","revision":0,"auditWatermark":"u","snapshot":{"blocks":[]},"modeSource":"inherited-child"}),
        ),
    ];
    assert!(build_session_context(&entries, Some("on"))
        .messages
        .is_empty());
    assert_eq!(build_session_context(&entries, Some("u")).messages.len(), 1);
}

#[test]
fn clm_damage_is_not_repaired_into_audit_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let text = concat!("{\"type\":\"session\",\"id\":\"s\",\"version\":3,\"cwd\":\"/tmp\",\"timestamp\":\"now\"}\n",
        "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"obsolete\",\"timestamp\":1}}\n",
        "{\"type\":\"clm_on\",\"snapshot\":");
    std::fs::write(&path, text).unwrap();
    let _manager = manager::SessionManager::open(dir.path(), dir.path(), &path);
    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
}

use crate::clm::{
    journal::{ClmCommit, ClmJournalOp, ClmJournalReply},
    ProtectedActions,
};
fn on_row(watermark: Option<&str>) -> FileEntry {
    row(
        json!({"type":"clm_on","epochId":"e","revision":0,"snapshot":{"blocks":[]},"auditWatermark":watermark,"modeSource":"inherited-child"}),
    )
}
fn view(manager: &mut manager::SessionManager) -> crate::clm::journal::ClmJournalView {
    let ClmJournalReply::View(v) = manager
        .clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new())
        .unwrap()
    else {
        panic!("view");
    };
    v
}
#[test]
fn clm_commit_is_durable_before_first_assistant_and_reuses_exact_ids() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = manager::SessionManager::persisted(dir.path(), dir.path());
    let message =
        serde_json::from_value(json!({"role":"user","content":"same","timestamp":1})).unwrap();
    let a = owner.append_message(message).unwrap();
    let message =
        serde_json::from_value(json!({"role":"user","content":"same","timestamp":1})).unwrap();
    let b = owner.append_message(message).unwrap();
    assert_ne!(a, b);
    let expected = view(&mut owner);
    let ClmJournalReply::Committed {
        row: accepted,
        view: after,
    } = owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected: expected.clone(),
                row: on_row(Some(&b)),
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap()
    else {
        panic!("commit");
    };
    assert_eq!(accepted.parent_id(), Some(b.as_str()));
    assert_ne!(accepted.id(), Some("caller"));
    assert!(owner.active_context_checked().unwrap().messages.is_empty());
    assert_eq!(after.branch.len(), 3);
    let mut mirror = manager::SessionManager::in_memory(dir.path());
    for entry in &after.branch {
        mirror.adopt_authoritative_row(entry.clone()).unwrap();
    }
    mirror.adopt_authoritative_row(accepted).unwrap();
    assert_eq!(mirror.get_entries(), after.branch);
    let mut reopened =
        manager::SessionManager::open(dir.path(), dir.path(), owner.get_session_file().unwrap());
    assert!(reopened
        .active_context_checked()
        .unwrap()
        .messages
        .is_empty());
    assert_eq!(view(&mut reopened).branch, after.branch);
    assert!(owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: on_row(None),
                allow_child_usage_suffix: false
            }),
            ProtectedActions::new()
        )
        .is_err());
}
#[test]
fn clm_invalid_snapshot_and_unpersisted_owner_never_publish() {
    let dir = tempfile::tempdir().unwrap();
    let mut memory = manager::SessionManager::in_memory(dir.path());
    let expected = view(&mut memory);
    assert!(memory
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: on_row(None),
                allow_child_usage_suffix: false
            }),
            ProtectedActions::new()
        )
        .is_err());
    assert!(memory.get_entries().is_empty());
    let mut owner = manager::SessionManager::persisted(dir.path(), dir.path());
    let expected = view(&mut owner);
    let mut invalid = serde_json::to_value(on_row(None)).unwrap();
    invalid["snapshot"] = json!({});
    assert!(owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: row(invalid),
                allow_child_usage_suffix: false
            }),
            ProtectedActions::new()
        )
        .is_err());
    assert!(owner.get_entries().is_empty());
}
#[test]
fn clm_known_damage_fails_checked_context_but_keeps_audit() {
    for tail in [
        "{\"type\":\"clm_on\"}\n",
        "{\"type\":\"clm_revision\",\"snapshot\":",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let text=format!("{{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"cwd\":\"/tmp\",\"timestamp\":\"now\"}}\n{{\"type\":\"message\",\"id\":\"u\",\"message\":{{\"role\":\"user\",\"content\":\"audit\",\"timestamp\":1}}}}\n{tail}");
        std::fs::write(&path, &text).unwrap();
        let owner = manager::SessionManager::open(dir.path(), dir.path(), &path);
        assert!(owner.active_context_checked().is_err());
        assert!(owner.get_entries().iter().any(|e| e.id() == Some("u")));
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }
}

#[test]
fn clm_commit_preserves_raw_audit_and_next_append_uses_new_inode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.jsonl");
    let text=concat!("{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"cwd\":\"/tmp\",\"timestamp\":\"now\"}\n",
        "{ \"type\": \"message\", \"id\": \"u\", \"parentId\": null, \"message\": {\"role\":\"user\",\"content\":\"audit\",\"timestamp\":1,\"future\":{\"z\":2,\"a\":1}}, \"extra\":true }\n");
    std::fs::write(&path, text).unwrap();
    let mut owner = manager::SessionManager::open(dir.path(), dir.path(), &path);
    let expected = view(&mut owner);
    owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: on_row(Some("u")),
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap();
    assert!(std::fs::read(&path).unwrap().starts_with(text.as_bytes()));
    let id = owner
        .append_message(
            serde_json::from_value(json!({"role":"user","content":"later","timestamp":2})).unwrap(),
        )
        .unwrap();
    let reopened = manager::SessionManager::open(dir.path(), dir.path(), &path);
    assert_eq!(reopened.get_leaf_id(), Some(id.as_str()));
    assert_eq!(reopened.active_context_checked().unwrap().messages.len(), 1);
}

thread_local! { static IO_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
pub(super) fn io_fault(stage: &str, path: &std::path::Path) -> std::io::Result<()> {
    if IO_FAULT.with(|f| f.get() == Some(stage)) {
        IO_FAULT.with(|f| f.set(None));
        if stage == "readback" {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)?
                .write_all(b"{\"type\":\"clm_revision\",\"snapshot\":")?;
            return Ok(());
        }
        return Err(std::io::Error::other(format!("injected {stage}")));
    }
    Ok(())
}
#[test]
fn clm_uncertain_commit_never_publishes_or_notifies_and_preflight_is_retryable() {
    for stage in [
        "directory-preflight",
        "write",
        "file-sync",
        "post-rename",
        "readback",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut owner = manager::SessionManager::persisted(dir.path(), dir.path());
        owner.flush_now().unwrap();
        let path = owner.get_session_file().unwrap().to_owned();
        let before = std::fs::read(&path).unwrap();
        let notices = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observer = notices.clone();
        owner.on_persist(Box::new(move |_| {
            observer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let expected = view(&mut owner);
        IO_FAULT.with(|f| f.set(Some(stage)));
        assert!(
            owner
                .clm_journal(
                    ClmJournalOp::Commit(ClmCommit {
                        expected,
                        row: on_row(None),
                        allow_child_usage_suffix: false
                    }),
                    ProtectedActions::new()
                )
                .is_err(),
            "{stage}"
        );
        IO_FAULT.with(|f| f.set(None));
        assert_eq!(
            notices.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{stage}"
        );
        assert!(owner.get_entries().is_empty());
        if stage == "directory-preflight" {
            assert!(owner.active_context_checked().is_ok());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        } else {
            assert!(owner.active_context_checked().is_err());
            assert!(owner
                .clm_journal(ClmJournalOp::ReadAfterEvents, ProtectedActions::new())
                .is_err());
        }
        if matches!(stage, "write" | "file-sync") {
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        let reopened = manager::SessionManager::open(dir.path(), dir.path(), &path);
        assert_eq!(
            reopened.active_context_checked().is_err(),
            stage == "readback"
        );
    }
}
fn revision(expected: &crate::clm::journal::ClmJournalView, watermark: Option<&str>) -> FileEntry {
    let snapshot = pa_types::clm::ClmSnapshot::from_value(json!({"blocks":[]}));
    let state = crate::clm::validate_clm_branch(&expected.branch).unwrap();
    row(
        json!({"type":"clm_revision","epochId":state.epoch_id,"revision":state.revision.unwrap()+1,"baseRevision":state.revision,"snapshot":snapshot,"auditWatermark":watermark,"observedActionId":"action","checksum":crate::clm::snapshot_checksum(&snapshot).unwrap()}),
    )
}
#[test]
fn clm_child_accounting_requires_captured_ancestry_and_other_suffixes_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = manager::SessionManager::persisted(dir.path(), dir.path());
    let expected = view(&mut owner);
    owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: on_row(None),
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap();
    let on_id = owner.get_leaf_id().unwrap().to_owned();
    let assistant=serde_json::from_value(json!({"role":"assistant","content":[],"api":"x","provider":"x","model":"x","usage":pa_types::ai::Usage::default(),"stopReason":"stop","timestamp":1})).unwrap();
    let target = owner.append_message(assistant).unwrap();
    let expected = view(&mut owner);
    for (id, total) in [("child-1", 3), ("child-2", 8)] {
        let usage = pa_types::ai::Usage {
            output: total,
            ..Default::default()
        };
        owner
            .append_child_usage_attribution(id, &target, usage, usage, None)
            .unwrap();
    }
    let candidate = revision(&expected, Some(&target));
    owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: candidate,
                allow_child_usage_suffix: true,
            }),
            ProtectedActions::new(),
        )
        .unwrap();
    let next = view(&mut owner);
    assert_eq!(
        crate::clm::validate_clm_branch(&next.branch)
            .unwrap()
            .revision,
        Some(1)
    );
    owner.set_leaf_id(Some(&on_id));
    let usage = pa_types::ai::Usage::default();
    owner
        .append_child_usage_attribution("sibling-child", &target, usage, usage, None)
        .unwrap();
    assert!(owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected: next.clone(),
                row: revision(&next, Some(&target)),
                allow_child_usage_suffix: true
            }),
            ProtectedActions::new()
        )
        .is_err());
    let expected = view(&mut owner);
    owner.append_custom_entry("ordinary", None).unwrap();
    assert!(owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                row: revision(&expected, None),
                expected,
                allow_child_usage_suffix: true
            }),
            ProtectedActions::new()
        )
        .is_err());
}

#[test]
fn clm_off_and_new_epoch_reopen_without_resurrecting_deleted_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = manager::SessionManager::persisted(dir.path(), dir.path());
    let user =
        serde_json::from_value(json!({"role":"user","content":"obsolete","timestamp":1})).unwrap();
    let watermark = owner.append_message(user).unwrap();
    let expected = view(&mut owner);
    owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: on_row(Some(&watermark)),
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap();
    let expected = view(&mut owner);
    let rev = revision(&expected, Some(&watermark));
    owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: rev,
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap();
    let expected = view(&mut owner);
    let off = row(
        json!({"type":"clm_off_boundary","epochId":"e","revision":2,"snapshot":{"blocks":[]},"auditWatermark":watermark,"modeSource":"session-override"}),
    );
    owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: off,
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap();
    let mut reopened =
        manager::SessionManager::open(dir.path(), dir.path(), owner.get_session_file().unwrap());
    assert!(reopened
        .active_context_checked()
        .unwrap()
        .messages
        .is_empty());
    let expected = view(&mut reopened);
    let mut enabled = serde_json::to_value(on_row(Some(&watermark))).unwrap();
    enabled["epochId"] = json!("e2");
    reopened
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: row(enabled),
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap();
    assert!(reopened
        .active_context_checked()
        .unwrap()
        .messages
        .is_empty());
    assert!(
        std::fs::read_to_string(reopened.get_session_file().unwrap())
            .unwrap()
            .contains("obsolete")
    );
}

#[test]
fn clm_checks_do_not_poison_ordinary_non_utf8_tail_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ordinary.jsonl");
    let mut bytes=b"{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"cwd\":\"/tmp\",\"timestamp\":\"now\"}\n".to_vec();
    bytes.push(0xff);
    std::fs::write(&path, bytes).unwrap();
    let owner = manager::SessionManager::open(dir.path(), dir.path(), &path);
    assert!(owner.active_context_checked().is_ok());
}
#[test]
fn clm_read_acknowledges_pre_assistant_rows_before_returning_ids() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = manager::SessionManager::persisted(dir.path(), dir.path());
    let id = owner
        .append_message(
            serde_json::from_value(json!({"role":"user","content":"delivery","timestamp":1}))
                .unwrap(),
        )
        .unwrap();
    let acknowledged = view(&mut owner);
    let path = owner.get_session_file().unwrap();
    assert!(path.exists());
    let reopened = manager::SessionManager::open(dir.path(), dir.path(), path);
    assert_eq!(reopened.get_leaf_id(), Some(id.as_str()));
    assert_eq!(acknowledged.leaf_id.as_deref(), Some(id.as_str()));
}
#[test]
fn clm_authoritative_child_row_adoption_folds_audit_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut mirror = manager::SessionManager::in_memory(dir.path());
    let assistant = row(
        json!({"type":"message","id":"a","parentId":null,"message":{"role":"assistant","content":[],"api":"x","provider":"x","model":"x","usage":pa_types::ai::Usage::default(),"stopReason":"stop","timestamp":1}}),
    );
    mirror.adopt_authoritative_row(assistant).unwrap();
    let aggregate = pa_types::ai::Usage {
        output: 8,
        ..Default::default()
    };
    let child = row(
        json!({"type":"child_usage_attributed","id":"child","parentId":"a","targetId":"a","childUsage":aggregate,"aggregateUsage":aggregate}),
    );
    mirror.adopt_authoritative_row(child).unwrap();
    let FileEntry::Message {
        message: pa_types::session::AgentMessage::Assistant(assistant),
        ..
    } = mirror.get_entry_by_id("a").unwrap()
    else {
        panic!("assistant");
    };
    assert_eq!(assistant.usage.output, 8);
}

#[test]
fn clm_fork_refuses_damaged_source_without_copying_audit_as_context() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.jsonl");
    std::fs::write(&source,concat!("{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"cwd\":\"/tmp\",\"timestamp\":\"now\"}\n","{\"type\":\"message\",\"id\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"obsolete\",\"timestamp\":1}}\n","{\"type\":\"clm_on\",\"snapshot\":" )).unwrap();
    assert!(
        manager::SessionManager::fork_from(&source, dir.path(), &dir.path().join("forks")).is_err()
    );
}

#[cfg(unix)]
#[test]
fn clm_commit_preserves_symlink_owner_and_appends_to_canonical_file() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.jsonl");
    let link = dir.path().join("linked.jsonl");
    let mut owner = manager::SessionManager::persisted(dir.path(), dir.path());
    owner.flush_now().unwrap();
    std::fs::rename(owner.get_session_file().unwrap(), &real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut owner = manager::SessionManager::open(dir.path(), dir.path(), &link);
    let expected = view(&mut owner);
    owner
        .clm_journal(
            ClmJournalOp::Commit(ClmCommit {
                expected,
                row: on_row(None),
                allow_child_usage_suffix: false,
            }),
            ProtectedActions::new(),
        )
        .unwrap();
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::read_to_string(&real).unwrap().contains("clm_on"));
}

#[test]
fn clm_normal_context_blocks_keep_duplicate_delivery_and_compaction_ids() {
    let entries = vec![
        row(
            json!({"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"same","timestamp":1}}),
        ),
        row(
            json!({"type":"message","id":"b","parentId":"a","message":{"role":"user","content":"same","timestamp":1}}),
        ),
    ];
    let blocks = clm::build_model_context_blocks_checked(&entries, Some("b")).unwrap();
    assert_eq!(
        blocks.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert_eq!(blocks[0].message, blocks[1].message);
    let mut compacted = entries;
    compacted.push(row(json!({"type":"compaction","id":"compact","parentId":"b","summary":"summary","firstKeptEntryId":"b","tokensBefore":5})));
    let blocks = clm::build_model_context_blocks_checked(&compacted, Some("compact")).unwrap();
    assert_eq!(
        blocks.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
        vec!["compact", "b"]
    );
    assert_eq!(blocks[0].message["role"], "compactionSummary");
    let messages = build_session_context(&compacted, Some("compact")).messages;
    assert_eq!(
        blocks.into_iter().map(|b| b.message).collect::<Vec<_>>(),
        messages
            .into_iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect::<Vec<_>>()
    );
}
#[test]
fn clm_normal_blocks_keep_only_the_newest_digest_and_kernel_notice_ids() {
    let mut entries = Vec::new();
    let mut parent = None;
    for (id, kind) in [
        ("d1", "harness_digest"),
        ("n1", "ipython_state"),
        ("d2", "harness_digest"),
        ("n2", "ipython_state"),
    ] {
        entries.push(row(json!({"type":"custom_message","id":id,"parentId":parent,"customType":kind,"content":"same","display":false})));
        parent = Some(id);
    }
    let blocks = clm::build_model_context_blocks_checked(&entries, Some("n2")).unwrap();
    assert_eq!(
        blocks.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
        vec!["d2", "n2"]
    );
}

#[test]
fn clm_torn_tag_detection_matches_type_field_not_unrelated_message_text() {
    assert!(clm::contains_clm_records(br#"{"type" : "clm_"#));
    assert!(!clm::contains_clm_records(
        br#"{"type":"message","message":{"role":"user","content":"mentions clm_on"#
    ));
    assert!(clm::parse_session_entries_checked(br#"{"type" : "clm_"#).is_err());
}

#[test]
fn clm_read_does_not_acknowledge_a_failed_append_after_initial_flush() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = manager::SessionManager::persisted(dir.path(), dir.path());
    let expected = view(&mut owner);
    owner
        .clm_journal(
            crate::clm::journal::ClmJournalOp::Commit(crate::clm::journal::ClmCommit {
                expected,
                row: on_row(None),
                allow_child_usage_suffix: false,
            }),
            crate::clm::ProtectedActions::new(),
        )
        .unwrap();
    let path = owner.get_session_file().unwrap().to_owned();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(owner
        .append_message(
            serde_json::from_value(json!({"role":"user","content":"failed append","timestamp":1}))
                .unwrap()
        )
        .is_err());
    assert!(owner
        .clm_journal(
            crate::clm::journal::ClmJournalOp::ReadAfterEvents,
            crate::clm::ProtectedActions::new()
        )
        .is_err());
}

#[test]
fn clm_open_preserves_parented_header_and_raw_audit_through_later_writes() {
    for depth in [None, Some(7)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("imported.jsonl");
        let mut header = json!({"type":"session","version":3,"id":"imported","cwd":"/tmp","timestamp":"now","parentSession":"generated-source.jsonl","futureHeader":{"keep":true}});
        if let Some(depth) = depth {
            header["rlmDepth"] = json!(depth);
        }
        let audit = json!({"type":"message","id":"u","parentId":null,"timestamp":"now","futureAudit":{"keep":true},"message":{"role":"user","content":"covered audit","timestamp":1}});
        let boundary = json!({"type":"clm_on","id":"on","parentId":"u","timestamp":"now","epochId":"e","revision":0,"auditWatermark":"u","snapshot":{"blocks":[]}});
        let raw = format!("  {header}  \n  {audit}  \n  {boundary}  \n");
        std::fs::write(&path, &raw).unwrap();
        let mut owner = manager::SessionManager::open(dir.path(), dir.path(), &path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        assert_eq!(owner.get_header().unwrap().rlm_depth, depth);
        assert_eq!(
            owner.get_header().unwrap().parent_session.as_deref(),
            Some("generated-source.jsonl")
        );
        let expected = view(&mut owner);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        let candidate = revision(&expected, Some("u"));
        owner
            .clm_journal(
                ClmJournalOp::Commit(ClmCommit {
                    expected,
                    row: candidate,
                    allow_child_usage_suffix: false,
                }),
                ProtectedActions::new(),
            )
            .unwrap();
        owner
            .append_message(
                serde_json::from_value(json!({"role":"user","content":"new suffix","timestamp":2}))
                    .unwrap(),
            )
            .unwrap();
        let _acknowledged = view(&mut owner);
        assert!(std::fs::read_to_string(&path).unwrap().starts_with(&raw));
        let reopened = manager::SessionManager::open(dir.path(), dir.path(), &path);
        assert_eq!(reopened.get_header().unwrap().rlm_depth, depth);
        assert_eq!(reopened.active_context_checked().unwrap().messages.len(), 1);
        assert!(std::fs::read_to_string(&path).unwrap().starts_with(&raw));
    }
    // The legacy ordinary-session metadata migration remains in force.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ordinary.jsonl");
    std::fs::write(&path,"{\"type\":\"session\",\"version\":3,\"id\":\"ordinary\",\"cwd\":\"/tmp\",\"timestamp\":\"now\",\"parentSession\":\"generated-source.jsonl\"}\n").unwrap();
    let ordinary = manager::SessionManager::open(dir.path(), dir.path(), &path);
    assert_eq!(ordinary.get_header().unwrap().rlm_depth, Some(0));
    assert_eq!(
        manager::read_session_header(&path).unwrap().rlm_depth,
        Some(0)
    );
}

#[test]
fn clm_authoritative_retries_survive_derived_usage_without_accepting_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let mut mirror = manager::SessionManager::in_memory(dir.path());
    let assistant = row(
        json!({"type":"message","id":"a","parentId":null,"message":{"role":"assistant","content":[],"api":"x","provider":"x","model":"x","usage":pa_types::ai::Usage::default(),"stopReason":"stop","timestamp":1}}),
    );
    let mut orphan = assistant.clone();
    let FileEntry::Message { base, .. } = &mut orphan else {
        panic!("assistant");
    };
    base.id = Some("orphan".into());
    base.parent_id = Some("missing".into());
    assert!(mirror.adopt_authoritative_row(orphan).is_err());
    assert!(mirror.get_entries().is_empty());
    let mut durable = manager::SessionManager::persisted(dir.path(), dir.path());
    assert!(durable.adopt_authoritative_row(assistant.clone()).is_err());
    assert!(durable.get_entries().is_empty());
    let aggregate = pa_types::ai::Usage {
        output: 8,
        ..Default::default()
    };
    let child = row(
        json!({"type":"child_usage_attributed","id":"child","parentId":"a","targetId":"a","childUsage":aggregate,"aggregateUsage":aggregate}),
    );
    mirror.adopt_authoritative_row(assistant.clone()).unwrap();
    mirror.adopt_authoritative_row(child.clone()).unwrap();
    let before = mirror.get_all_entries().to_vec();
    mirror.adopt_authoritative_row(assistant.clone()).unwrap();
    mirror.adopt_authoritative_row(child).unwrap();
    assert_eq!(mirror.get_all_entries(), before);
    assert_eq!(mirror.get_leaf_id(), Some("child"));
    for changed_usage in [false, true] {
        let mut conflict = assistant.clone();
        let FileEntry::Message {
            message: AgentMessage::Assistant(message),
            ..
        } = &mut conflict
        else {
            panic!("assistant");
        };
        if changed_usage {
            message.usage.output = 8;
        } else {
            message.content =
                serde_json::from_value(json!([{"type":"text","text":"different content"}]))
                    .unwrap();
        }
        assert!(mirror.adopt_authoritative_row(conflict).is_err());
    }
    assert_eq!(mirror.get_all_entries(), before);
    assert_eq!(mirror.get_leaf_id(), Some("child"));
    let mut replacement = assistant;
    let FileEntry::Message {
        message: AgentMessage::Assistant(message),
        ..
    } = &mut replacement
    else {
        panic!("assistant");
    };
    message.usage.output = 9;
    mirror.adopt_entries(vec![replacement.clone()]);
    mirror.adopt_authoritative_row(replacement).unwrap();
    assert_eq!(mirror.get_leaf_id(), Some("a"));
}

#[tokio::test]
async fn clm_runtime_rejects_file_sync_failure_without_publishing_or_dispatching_again() {
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    struct ResetFault;
    impl Drop for ResetFault {
        fn drop(&mut self) {
            IO_FAULT.with(|fault| fault.set(None));
        }
    }
    let _reset = ResetFault;
    IO_FAULT.with(|fault| assert!(fault.get().is_none()));
    let dir = tempfile::tempdir().unwrap();
    let mut owner = manager::SessionManager::persisted(dir.path(), &dir.path().join("sessions"));
    owner
        .append_message(
            serde_json::from_value(json!({"role":"user","content":"obsolete-fsync","timestamp":1}))
                .unwrap(),
        )
        .unwrap();
    let file = owner.get_session_file().unwrap().to_path_buf();
    let notices = Arc::new(AtomicUsize::new(0));
    owner.on_persist(Box::new({
        let notices = notices.clone();
        move |path| {
            let text = std::fs::read_to_string(path).unwrap();
            let row: serde_json::Value =
                serde_json::from_str(text.lines().last().unwrap()).unwrap();
            if row["type"] == "clm_revision" {
                notices.fetch_add(1, Ordering::SeqCst);
            }
        }
    }));
    let mirror = Arc::new(Mutex::new(None::<std::path::PathBuf>));
    let before = Arc::new(Mutex::new(Vec::new()));
    let tool = crate::session_engine::tool_bridge::bridge_tool(
        crate::tools::tool_definition::ToolDefinition {
            name: "fail_sync".into(),
            label: "fail_sync".into(),
            description: "exercise the owned sync failure".into(),
            prompt_snippet: String::new(),
            parameters: json!({"type":"object","properties":{}}),
            execution_mode: None,
            prepare_arguments: None,
            execute: Arc::new({
                let mirror = mirror.clone();
                let file = file.clone();
                let before = before.clone();
                let notices = notices.clone();
                move |_, _, _, _| {
                    let mirror = mirror.clone();
                    let file = file.clone();
                    let before = before.clone();
                    let notices = notices.clone();
                    Box::pin(async move {
                        let path = mirror.lock().unwrap().clone().unwrap();
                        let text = std::fs::read_to_string(&path)?;
                        assert!(text.contains("obsolete-fsync"));
                        std::fs::write(path, text.replace("obsolete-fsync", "changed-fsync"))?;
                        *before.lock().unwrap() = std::fs::read(file)?;
                        notices.store(0, Ordering::SeqCst);
                        IO_FAULT.with(|fault| fault.set(Some("file-sync")));
                        Ok(crate::tools::tool_definition::ToolExecutionResult::text(
                            "edited",
                        ))
                    })
                }
            }),
        },
    );
    let provider =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions::default());
    let model = provider.models[0].clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let response = pa_ai::faux::FauxResponseStep::Factory(Arc::new({
        let calls = calls.clone();
        move |_, _, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(pa_ai::faux::faux_assistant_message(
                vec![pa_ai::faux::faux_tool_call(
                    "fail_sync",
                    &json!({}),
                    Some("fail-sync"),
                )],
                pa_ai::faux::FauxAssistantMessageOptions {
                    stop_reason: Some(pa_types::ai::StopReason::ToolUse),
                    ..Default::default()
                },
            ))
        }
    }));
    provider.set_responses(vec![response.clone(), response]);
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        session_manager: Some(owner),
        tools: vec![tool],
        model: Some(serde_json::from_value(serde_json::to_value(&model).unwrap()).unwrap()),
        model_info: Some(model.clone()),
        stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
            None, model,
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let runtime = engine.session.clm.as_ref().unwrap();
    let status = runtime
        .set_mode(true, pa_types::clm::ClmModeSource::SessionOverride)
        .await
        .unwrap();
    *mirror.lock().unwrap() = Some(status.path.into());
    let failed = engine
        .prompt(
            "edit old context",
            crate::session_engine::PromptOptions::default(),
        )
        .await;
    assert!(
        failed.is_err(),
        "an unacknowledged revision must fail the action"
    );
    assert!(format!("{:#}", failed.unwrap_err()).contains("injected file-sync"));
    assert!(
        IO_FAULT.with(|fault| fault.get().is_none()),
        "the actual file-sync hook was consumed"
    );
    let after = std::fs::read(&file).unwrap();
    assert!(
        after.starts_with(&before.lock().unwrap()),
        "accepted journal prefix remains unchanged"
    );
    assert!(
        !String::from_utf8_lossy(&after).contains("changed-fsync"),
        "failed candidate was not published"
    );
    assert_eq!(
        notices.load(Ordering::SeqCst),
        0,
        "failed revision publication must not notify persistence listeners"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(runtime.view().await.is_err());
    assert!(engine
        .prompt(
            "must remain blocked",
            crate::session_engine::PromptOptions::default()
        )
        .await
        .is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let reopened = manager::SessionManager::open(dir.path(), dir.path(), &file);
    let accepted = reopened.active_context_checked().unwrap();
    let text = serde_json::to_string(&accepted.messages).unwrap();
    assert!(text.contains("obsolete-fsync"));
    assert!(!text.contains("changed-fsync"));
    provider.unregister();
}
