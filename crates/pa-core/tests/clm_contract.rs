use pa_core::clm::{parse_context_document, render_context_document, snapshot_checksum};
use pa_types::clm::ClmSnapshot;
use serde_json::Value;
use std::collections::HashSet;

fn reference() -> Value {
    serde_json::from_str(include_str!("fixtures/clm/ts-reference.json")).unwrap()
}

#[test]
fn captured_documents_round_trip_without_changing_native_blocks() {
    let corpus = &reference()["corpus"];
    for key in [
        "utf8-escapes",
        "protected",
        "tools-intact",
        "delete-old",
        "edit-tool-result",
        "reorder-tool-group",
    ] {
        let case = &corpus[key];
        let base = ClmSnapshot::from_value(case.get("base").unwrap_or(&case["snapshot"]).clone());
        if case.get("snapshot").is_some() {
            let protected = if key == "protected" {
                HashSet::from(["protected/current".to_owned()])
            } else {
                HashSet::new()
            };
            assert_eq!(
                render_context_document(&base, &protected).unwrap(),
                case["document"].as_str().unwrap(),
                "{key}"
            );
        }
        let parsed = parse_context_document(case["document"].as_str().unwrap(), &base).unwrap();
        assert_eq!(parsed.as_value(), &case["parsed"], "{key}");
    }
}

#[test]
fn checksum_matches_javascript_object_order_and_numbers() {
    for case in reference()["checksumCases"].as_object().unwrap().values() {
        let snapshot = ClmSnapshot::from_value(case["snapshot"].clone());
        assert_eq!(
            snapshot_checksum(&snapshot).unwrap(),
            case["sha256"].as_str().unwrap()
        );
    }
}

fn snapshot(blocks: &[Value]) -> ClmSnapshot {
    ClmSnapshot::from_value(serde_json::json!({"blocks":blocks}))
}
fn row(value: Value) -> pa_types::session::FileEntry {
    serde_json::from_value(value).unwrap()
}

#[test]
fn foreign_images_and_fake_protected_references_are_rejected() {
    let sample = &reference()["corpus"]["tools-intact"];
    let base = ClmSnapshot::from_value(sample["snapshot"].clone());
    let document = sample["document"].as_str().unwrap();
    assert!(
        parse_context_document(&document.replace("alpha-result:1", "foreign:1"), &base).is_err()
    );
    assert!(parse_context_document(
        &document.replace("[[CTX_END]]", "[[CTX_PROTECTED id=alpha]]"),
        &base
    )
    .is_err());
    let protected = pa_core::clm::ProtectedActions::from([(
        "active".into(),
        vec![pa_types::clm::ClmBlock {
            id: "alpha".into(),
            message: sample["snapshot"]["blocks"][0]["message"].clone(),
        }],
    )]);
    let changed =
        snapshot(&[serde_json::json!({"id":"alpha","message":{"role":"user","content":"forged"}})]);
    assert!(pa_core::clm::validate_protection(&changed, &protected).is_err());
}

#[test]
fn reused_ids_in_separate_tool_groups_survive_but_duplicate_ids_within_a_group_fail() {
    let original = reference()["corpus"]["tools-intact"]["snapshot"].clone();
    let mut reused = original.clone();
    reused["blocks"][2]["message"]["content"][0]["id"] = Value::String("call-alpha".into());
    reused["blocks"][3]["message"]["toolCallId"] = Value::String("call-alpha".into());
    let reused = ClmSnapshot::from_value(reused);
    let text = render_context_document(&reused, &HashSet::new()).unwrap();
    assert_eq!(parse_context_document(&text, &reused).unwrap(), reused);
    let mut duplicate = original;
    let call = duplicate["blocks"][0]["message"]["content"][0].clone();
    duplicate["blocks"][0]["message"]["content"]
        .as_array_mut()
        .unwrap()
        .push(call);
    let duplicate = ClmSnapshot::from_value(duplicate);
    let text = render_context_document(&duplicate, &HashSet::new()).unwrap();
    assert!(parse_context_document(&text, &duplicate).is_err());
}

#[test]
fn source_budget_counts_utf16_and_captured_image_tool_cost() {
    use pa_core::clm::estimate_clm_request_tokens;
    use pa_types::ai::{Model, ModelCost, Tool};
    use pa_types::JsNumber;
    let mut model = Model {
        id: "x".into(),
        name: "x".into(),
        api: "openai-responses".into(),
        provider: "openai".into(),
        base_url: "https://example.invalid".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: ModelCost {
            input: JsNumber(0.0),
            output: JsNumber(0.0),
            cache_read: JsNumber(0.0),
            cache_write: JsNumber(0.0),
        },
        context_window: 1000,
        max_tokens: 300,
        max_tokens_explicit: false,
        featured: None,
        headers: None,
        compat: None,
    };
    let messages = [
        serde_json::json!({"role":"user","content":"hello","timestamp":1}),
        serde_json::json!({"role":"user","content":[{"type":"text","text":"image"},{"type":"image","mimeType":"image/png","data":"aW1hZ2U="}],"timestamp":4}),
    ];
    let tools = [Tool {
        name: "search".into(),
        description: "find synthetic records".into(),
        parameters: serde_json::json!({"type":"object","properties":{"query":{"type":"string"}}}),
    }];
    let actual =
        estimate_clm_request_tokens(&messages, "fixed instruction", &tools, &model).unwrap();
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        reference()["estimates"]["known"]
    );
    model.context_window = 0;
    let actual = estimate_clm_request_tokens(&messages[..1], "", &[], &model).unwrap();
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        reference()["estimates"]["unknown"]
    );
    let astral = estimate_clm_request_tokens(
        &[serde_json::json!({"role":"user","content":"🦊🦊🦊"})],
        "",
        &[],
        &model,
    )
    .unwrap();
    assert_eq!(astral.estimated_input_tokens, 2);
}

#[test]
fn validated_ancestry_watermark_checksum_and_off_suffix() {
    use pa_core::clm::{project_clm_branch, validate_child_usage_suffix, validate_clm_branch};
    let seed = snapshot(&[
        serde_json::json!({"id":"user-0","message":{"role":"user","content":"accepted","timestamp":1}}),
    ]);
    let header = row(
        serde_json::json!({"type":"message","id":"user-0","parentId":null,"timestamp":"2026-01-01","message":{"role":"user","content":"old","timestamp":1}}),
    );
    let on = row(
        serde_json::json!({"type":"clm_on","id":"on","parentId":"user-0","timestamp":"2026-01-01","epochId":"epoch","revision":0,"snapshot":seed.as_value(),"auditWatermark":"user-0"}),
    );
    let revision = row(
        serde_json::json!({"type":"clm_revision","id":"rev","parentId":"on","timestamp":"2026-01-01","epochId":"epoch","revision":1,"baseRevision":0,"checksum":pa_core::clm::snapshot_checksum(&seed).unwrap(),"observedActionId":"tool-1","snapshot":seed.as_value(),"auditWatermark":"user-0"}),
    );
    let off = row(
        serde_json::json!({"type":"clm_off_boundary","id":"off","parentId":"rev","timestamp":"2026-01-01","epochId":"epoch","revision":2,"snapshot":seed.as_value(),"auditWatermark":"user-0"}),
    );
    let suffix = row(
        serde_json::json!({"type":"message","id":"new","parentId":"off","timestamp":"2026-01-01","message":{"role":"user","content":"new","timestamp":2}}),
    );
    let path = vec![header, on, revision, off, suffix];
    assert_eq!(validate_clm_branch(&path).unwrap().revision, Some(2));
    let projection = project_clm_branch(&path).unwrap().unwrap();
    assert_eq!(
        projection
            .blocks
            .iter()
            .map(|b| b.id.as_str())
            .collect::<Vec<_>>(),
        vec!["user-0", "new"]
    );
    assert!(validate_child_usage_suffix(&path, Some("off")).is_err());
    let mut broken = path.clone();
    broken[2] = row(
        serde_json::json!({"type":"clm_revision","id":"rev","parentId":"on","timestamp":"2026-01-01","epochId":"epoch","revision":1,"baseRevision":0,"checksum":"wrong","observedActionId":"tool-1","snapshot":seed.as_value(),"auditWatermark":"user-0"}),
    );
    assert!(validate_clm_branch(&broken).is_err());
    broken[2] = path[2].clone();
    broken[3] = row(
        serde_json::json!({"type":"clm_off_boundary","id":"off","parentId":"rev","timestamp":"2026-01-01","epochId":"epoch","revision":2,"snapshot":seed.as_value(),"auditWatermark":"missing"}),
    );
    assert!(validate_clm_branch(&broken).is_err());
}

#[test]
fn malformed_known_clm_fails_without_breaking_unknown_forward_compatibility() {
    assert!(serde_json::from_value::<pa_types::session::FileEntry>(
        serde_json::json!({"type":"clm_revision","id":"r","parentId":null,"timestamp":"now"})
    )
    .is_err());
    assert!(matches!(
        row(serde_json::json!({"type":"future_kind","id":"future"})),
        pa_types::session::FileEntry::Unknown { .. }
    ));
    let header = serde_json::json!({"type":"session","version":3,"id":"safe","timestamp":"2026-01-01","cwd":"/tmp"});
    let malformed = format!(
        "{}\n{}\n",
        header,
        serde_json::json!({"type":"clm_on","id":"x"})
    );
    assert!(pa_core::clm::validate_clm_import(malformed.as_bytes()).is_err());
}

#[test]
fn import_rejects_missing_clm_ancestor_and_preserves_normal_input() {
    let header = serde_json::json!({"type":"session","version":3,"id":"safe","timestamp":"2026-01-01","cwd":"/tmp"});
    let ordinary = format!(
        "{}\n{}\n",
        header,
        serde_json::json!({"type":"future_kind","id":"forward","parentId":null,"timestamp":"2026-01-01"})
    );
    assert!(pa_core::clm::validate_clm_import(ordinary.as_bytes())
        .unwrap()
        .is_none());
    let clm = format!(
        "{}\n{}\n",
        header,
        serde_json::json!({"type":"clm_on","id":"on","parentId":"missing","timestamp":"2026-01-01","epochId":"epoch","revision":0,"snapshot":{"blocks":[]},"auditWatermark":null})
    );
    assert!(pa_core::clm::validate_clm_import(clm.as_bytes()).is_err());
}

#[test]
fn checksum_includes_unknown_fields_and_numeric_index_key_enumeration() {
    let raw = r#"{"opaque":true,"blocks":[{"id":"x","message":{"role":"user","content":"x","timestamp":1},"unknown":{"11":"eleven","2":"two","z":"z"}}],"trailing":{"x":1}}"#;
    let value: Value = serde_json::from_str(raw).unwrap();
    let snapshot = ClmSnapshot::from_value(value);
    let equivalent = r#"{"opaque":true,"blocks":[{"id":"x","message":{"role":"user","content":"x","timestamp":1},"unknown":{"2":"two","11":"eleven","z":"z"}}],"trailing":{"x":1}}"#;
    let second = ClmSnapshot::from_value(serde_json::from_str(equivalent).unwrap());
    assert_eq!(
        snapshot_checksum(&snapshot).unwrap(),
        snapshot_checksum(&second).unwrap()
    );
    let reordered = r#"{"trailing":{"x":1},"blocks":[{"id":"x","message":{"role":"user","content":"x","timestamp":1},"unknown":{"2":"two","11":"eleven","z":"z"}}],"opaque":true}"#;
    let third = ClmSnapshot::from_value(serde_json::from_str(reordered).unwrap());
    assert_ne!(
        snapshot_checksum(&snapshot).unwrap(),
        snapshot_checksum(&third).unwrap()
    );
}

#[test]
fn checksum_javascript_fixed_notation_at_exponent_boundaries() {
    use sha2::{Digest, Sha256};
    let raw = r#"{"blocks":[{"id":"x","message":{"role":"user","content":"x","timestamp":1,"tiny":1e-6,"middle":1e20,"large":1e21}}]}"#;
    let expected = r#"{"blocks":[{"id":"x","message":{"role":"user","content":"x","timestamp":1,"tiny":0.000001,"middle":100000000000000000000,"large":1e+21}}]}"#;
    let snapshot = ClmSnapshot::from_value(serde_json::from_str(raw).unwrap());
    assert_eq!(
        snapshot_checksum(&snapshot).unwrap(),
        format!("{:x}", Sha256::digest(expected.as_bytes()))
    );
}

#[test]
fn import_rejects_unsupported_version_unknown_roles_and_broken_images() {
    let valid = |version: u64, message: Value| {
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"session","version":version,"id":"safe","timestamp":"2026-01-01","cwd":"/tmp"}),
            serde_json::json!({"type":"clm_on","id":"on","parentId":null,"timestamp":"2026-01-01","epochId":"epoch","revision":0,"snapshot":{"blocks":[{"id":"b","message":message}]},"auditWatermark":null})
        )
    };
    let regular = serde_json::json!({"role":"user","content":"hi","timestamp":1});
    assert!(
        pa_core::clm::validate_clm_import(valid(3, regular.clone()).as_bytes())
            .unwrap()
            .is_some()
    );
    assert!(pa_core::clm::validate_clm_import(valid(99, regular).as_bytes()).is_err());
    assert!(pa_core::clm::validate_clm_import(
        valid(3, serde_json::json!({"role":"alien","content":"hi"})).as_bytes()
    )
    .is_err());
    assert!(pa_core::clm::validate_clm_import(valid(3,serde_json::json!({"role":"user","content":[{"type":"image","mimeType":"image/png","data":"not base64!"}],"timestamp":1})).as_bytes()).is_err());
}

#[test]
fn document_renders_provider_visible_bash_and_summary_text() {
    let input = ClmSnapshot::from_value(serde_json::json!({"blocks":[
        {"id":"bash","message":{"role":"bashExecution","command":"echo hi","output":"hi","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1}},
        {"id":"branch","message":{"role":"branchSummary","summary":"short","fromId":"old","timestamp":2}},
        {"id":"compact","message":{"role":"compactionSummary","summary":"short","tokensBefore":300,"timestamp":3}}
    ]}));
    let doc = render_context_document(&input, &HashSet::new()).unwrap();
    assert!(doc.contains("Ran `echo hi`\n```\nhi\n```"));
    assert!(doc.contains("[branch-summary]\n\nThe following is a summary of a branch that this conversation came back from:\n\n<summary>\nshort\n</summary>"));
    assert!(doc.contains("[compaction-summary]\n\nThe conversation history before this point was compacted into the following summary.\nThe retained messages below are authoritative; this summary may lag behind them.\n\n<summary>\nshort\n</summary>"));
}

#[test]
fn off_projection_compacts_seed_and_appends_uncovered_suffix_once() {
    let seed = ClmSnapshot::from_value(serde_json::json!({"blocks":[
        {"id":"old","message":{"role":"user","content":"old","timestamp":1}},
        {"id":"keep","message":{"role":"user","content":"keep","timestamp":2}}
    ]}));
    let on = row(
        serde_json::json!({"type":"clm_on","id":"on","parentId":null,"timestamp":"2026-01-01","epochId":"epoch","revision":0,"snapshot":seed.as_value(),"auditWatermark":null}),
    );
    let off = row(
        serde_json::json!({"type":"clm_off_boundary","id":"off","parentId":"on","timestamp":"2026-01-01","epochId":"epoch","revision":1,"snapshot":seed.as_value(),"auditWatermark":null}),
    );
    let before = row(
        serde_json::json!({"type":"message","id":"before","parentId":"off","timestamp":"2026-01-01","message":{"role":"user","content":"before","timestamp":3}}),
    );
    let compact = row(
        serde_json::json!({"type":"compaction","id":"compact","parentId":"before","timestamp":"2026-01-01","summary":"summary","firstKeptEntryId":"keep","tokensBefore":12}),
    );
    let after = row(
        serde_json::json!({"type":"message","id":"after","parentId":"compact","timestamp":"2026-01-01","message":{"role":"user","content":"after","timestamp":4}}),
    );
    let actual = pa_core::clm::project_clm_branch(&[on, off, before, compact, after])
        .unwrap()
        .unwrap();
    assert_eq!(
        actual
            .blocks
            .iter()
            .map(|b| b.id.as_str())
            .collect::<Vec<_>>(),
        vec!["compact", "keep", "before", "after"]
    );
}

#[test]
fn budget_counts_digest_and_role_content_but_not_historical_bills() {
    use pa_core::clm::estimate_clm_request_tokens;
    use pa_types::{
        ai::{Model, ModelCost},
        JsNumber,
    };
    let model = Model {
        id: "x".into(),
        name: "x".into(),
        api: "openai-responses".into(),
        provider: "openai".into(),
        base_url: "https://example.invalid".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: ModelCost {
            input: JsNumber(0.0),
            output: JsNumber(0.0),
            cache_read: JsNumber(0.0),
            cache_write: JsNumber(0.0),
        },
        context_window: 1000,
        max_tokens: 300,
        max_tokens_explicit: false,
        featured: None,
        headers: None,
        compat: None,
    };
    let assistant = serde_json::json!({"role":"assistant","content":[{"type":"text","text":"reply"},{"type":"thinking","thinking":"abcd"},{"type":"toolCall","id":"tool-1","name":"read","arguments":{"path":"x"}}],"usage":{"input":999_999}});
    let mut unbilled = assistant.clone();
    unbilled.as_object_mut().unwrap().remove("usage");
    assert_eq!(
        estimate_clm_request_tokens(&[assistant], "", &[], &model)
            .unwrap()
            .estimated_input_tokens,
        estimate_clm_request_tokens(&[unbilled], "", &[], &model)
            .unwrap()
            .estimated_input_tokens
    );
    let compaction = serde_json::json!({"role":"compactionSummary","summary":"short","harnessDigest":"memory","timestamp":1});
    let plain = serde_json::json!({"role":"compactionSummary","summary":"short","timestamp":1});
    let with = estimate_clm_request_tokens(&[compaction], "", &[], &model)
        .unwrap()
        .estimated_input_tokens;
    let without = estimate_clm_request_tokens(&[plain], "", &[], &model)
        .unwrap()
        .estimated_input_tokens;
    assert!(
        with > without,
        "provider-visible digest must add token cost"
    );
    assert!(
        estimate_clm_request_tokens(
            &[serde_json::json!({"role":"bashExecution","command":"echo hi","output":"done"})],
            "",
            &[],
            &model
        )
        .unwrap()
        .estimated_input_tokens
            > 0
    );
}

#[test]
fn clm_file_entry_variants_keep_abi_and_tree_fields() {
    for kind in ["clm_on", "clm_revision", "clm_off_boundary"] {
        let mut value = serde_json::json!({"type":kind,"id":"row","parentId":"parent","timestamp":"2026-01-01","epochId":"e","revision":u8::from(kind!="clm_on"),"snapshot":{"blocks":[],"foreign":"kept"},"auditWatermark":null,"unknownRowField":true});
        if kind == "clm_revision" {
            value["baseRevision"] = serde_json::json!(0);
            value["checksum"] = serde_json::json!("abc");
            value["observedActionId"] = serde_json::json!("tool");
        } else {
            value["modeSource"] = serde_json::json!("session-override");
        }
        let entry = row(value.clone());
        assert_eq!(entry.id(), Some("row"));
        assert_eq!(entry.parent_id(), Some("parent"));
        assert_eq!(entry.timestamp(), "2026-01-01");
        assert_eq!(serde_json::to_value(entry).unwrap(), value);
    }
}

#[test]
fn oversized_accepted_snapshot_remains_readable() {
    let text = "x".repeat(pa_core::clm::MAX_EDITABLE_BYTES + 1);
    let base = ClmSnapshot::from_value(
        serde_json::json!({"blocks":[{"id":"old","message":{"role":"user","content":text,"timestamp":1}}]}),
    );
    let doc = render_context_document(&base, &HashSet::new()).unwrap();
    assert_eq!(parse_context_document(&doc, &base).unwrap(), base);
}

#[test]
fn import_rejects_usage_pins_to_absent_or_mismatched_accepted_boundaries() {
    let header = serde_json::json!({"type":"session","version":3,"id":"safe","timestamp":"2026-01-01","cwd":"/tmp"});
    let on = serde_json::json!({"type":"clm_on","id":"on","parentId":null,"timestamp":"2026-01-01","epochId":"e","revision":0,"snapshot":{"blocks":[]},"auditWatermark":null});
    let receipt = |branch: &str, epoch: &str| serde_json::json!({"type":"custom","id":"usage","parentId":"on","timestamp":"2026-01-01","customType":"clm_usage_unknown_v1","data":{"version":1,"pin":{"mode":"on","branchId":branch,"epochId":epoch,"revision":0,"entryId":"on"}}});
    let log = |row: Value| format!("{header}\n{on}\n{row}\n");
    assert!(
        pa_core::clm::validate_clm_import(log(receipt("on", "e")).as_bytes())
            .unwrap()
            .is_some()
    );
    assert!(pa_core::clm::validate_clm_import(log(receipt("missing", "e")).as_bytes()).is_err());
    assert!(pa_core::clm::validate_clm_import(log(receipt("on", "other")).as_bytes()).is_err());
}

fn off_digest_path() -> Vec<pa_types::session::FileEntry> {
    let snapshot = serde_json::json!({"blocks":[
        {"id":"frozen-digest","message":{"role":"custom","customType":"harness_digest","content":"frozen","display":false,"timestamp":1}},
        {"id":"kept","message":{"role":"user","content":"kept","timestamp":2}}
    ]});
    vec![
        row(
            serde_json::json!({"type":"clm_on","id":"on","parentId":null,"timestamp":"2026-01-01T00:00:00.000Z","epochId":"e","revision":0,"snapshot":snapshot,"auditWatermark":null}),
        ),
        row(
            serde_json::json!({"type":"clm_off_boundary","id":"off","parentId":"on","timestamp":"2026-01-01T00:00:00.000Z","epochId":"e","revision":1,"snapshot":snapshot,"auditWatermark":null}),
        ),
    ]
}
fn digest_row(id: &str, parent: &str, text: &str, iso: &str) -> pa_types::session::FileEntry {
    row(
        serde_json::json!({"type":"custom_message","id":id,"parentId":parent,"timestamp":iso,"customType":"harness_digest","content":text,"display":false}),
    )
}
fn projected_messages(
    path: &[pa_types::session::FileEntry],
) -> Vec<(String, pa_types::session::AgentMessage)> {
    pa_core::clm::project_clm_branch(path)
        .unwrap()
        .unwrap()
        .blocks
        .into_iter()
        .map(|block| {
            let message = serde_json::from_value(block.message)
                .expect("projected block must be an AgentMessage");
            (block.id, message)
        })
        .collect()
}
#[test]
fn off_projection_selects_only_newest_digest_across_frozen_and_suffix_forms() {
    use pa_types::session::AgentMessage;
    let mut path = off_digest_path();
    path.push(digest_row(
        "digest-1",
        "off",
        "first",
        "2026-01-01T00:00:01.250Z",
    ));
    path.push(row(serde_json::json!({"type":"message","id":"digest-2","parentId":"digest-1","timestamp":"2026-01-01T00:00:02.500Z","message":{"role":"custom","customType":"harness_digest","content":"newest","display":false,"timestamp":1_767_225_602_500u64}})));
    path.push(row(serde_json::json!({"type":"branch_summary","id":"branch","parentId":"digest-2","timestamp":"2026-01-01T00:00:03.750Z","fromId":"prior","summary":"returned"})));
    let projected = projected_messages(&path);
    assert_eq!(
        projected
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["kept", "digest-2", "branch"]
    );
    assert!(
        matches!(&projected[1].1,AgentMessage::Custom(custom) if custom.content.text()=="newest" && custom.timestamp==1_767_225_602_500)
    );
    assert!(
        matches!(&projected[2].1,AgentMessage::BranchSummary(summary) if summary.from_id=="prior" && summary.timestamp==1_767_225_603_750)
    );
}
#[test]
fn off_compaction_snapshot_supersedes_frozen_and_suffix_digests() {
    use pa_types::session::AgentMessage;
    let mut path = off_digest_path();
    path.push(digest_row(
        "early",
        "off",
        "early",
        "2026-01-01T00:00:01.250Z",
    ));
    path.push(row(serde_json::json!({"type":"compaction","id":"compact","parentId":"early","timestamp":"2026-01-01T00:00:02.500Z","summary":"short","firstKeptEntryId":"frozen-digest","tokensBefore":42,"customInstructions":"do not forget","harnessDigest":"compacted","harnessStateFingerprint":"fp"})));
    let projected = projected_messages(&path);
    assert_eq!(
        projected
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["compact", "kept"]
    );
    assert!(
        matches!(&projected[0].1,AgentMessage::CompactionSummary(summary) if summary.retained_message_count==Some(1) && summary.custom_instructions.as_deref()==Some("do not forget") && summary.harness_digest.as_deref()==Some("compacted") && summary.harness_state_fingerprint.as_deref()==Some("fp") && summary.timestamp==1_767_225_602_500)
    );
}
#[test]
fn newer_digest_supersedes_compaction_snapshot_and_fingerprint() {
    use pa_types::session::AgentMessage;
    let mut path = off_digest_path();
    path.push(row(serde_json::json!({"type":"compaction","id":"compact","parentId":"off","timestamp":"2026-01-01T00:00:02.500Z","summary":"short","firstKeptEntryId":"kept","tokensBefore":42,"harnessDigest":"stale","harnessStateFingerprint":"fp"})));
    path.push(digest_row(
        "newer",
        "compact",
        "live",
        "2026-01-01T00:00:03.750Z",
    ));
    let projected = projected_messages(&path);
    assert_eq!(
        projected
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["compact", "kept", "newer"]
    );
    assert!(
        matches!(&projected[0].1,AgentMessage::CompactionSummary(summary) if summary.retained_message_count==Some(1) && summary.harness_digest.is_none() && summary.harness_state_fingerprint.is_none() && summary.timestamp==1_767_225_602_500)
    );
    assert!(
        matches!(&projected[2].1,AgentMessage::Custom(custom) if custom.content.text()=="live" && custom.timestamp==1_767_225_603_750)
    );
}
#[test]
fn custom_suffix_keeps_numeric_milliseconds_and_payload() {
    use pa_types::session::AgentMessage;
    let mut path = off_digest_path();
    path.push(row(serde_json::json!({"type":"custom_message","id":"custom","parentId":"off","timestamp":"2026-01-01T00:00:01.250Z","customType":"synthetic","content":"hello","display":true,"details":{"source":"test"}})));
    let projected = projected_messages(&path);
    assert!(
        matches!(&projected[2].1,AgentMessage::Custom(custom) if custom.timestamp==1_767_225_601_250 && custom.custom_type=="synthetic" && custom.details==Some(serde_json::json!({"source":"test"})))
    );
}
