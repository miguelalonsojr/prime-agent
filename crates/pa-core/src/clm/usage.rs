//! Source usage receipts are provider-only; audit totals may include child usage.
use super::{validate_clm_branch, ClmError};
use pa_types::{
    clm::{ClmMeasuredUsage, ClmMode, ClmUsagePin},
    session::{AgentMessage, FileEntry},
};
use serde_json::{json, Value};
use std::collections::HashSet;

/// Validate raw provider usage before typed defaults can hide absent fields.
#[must_use]
pub fn is_observed_clm_usage(usage: &Value) -> bool {
    let fields = [
        &usage["input"],
        &usage["output"],
        &usage["cacheRead"],
        &usage["cacheWrite"],
        &usage["totalTokens"],
        &usage["cost"]["input"],
        &usage["cost"]["output"],
        &usage["cost"]["cacheRead"],
        &usage["cost"]["cacheWrite"],
        &usage["cost"]["total"],
    ];
    fields
        .iter()
        .all(|v| v.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0))
        && fields.iter().any(|v| v.as_f64().is_some_and(|n| n > 0.0))
}

/// Construct source-shaped receipt fields for an owner to append as a custom row.
/// Missing or invalid raw usage produces an unknown receipt, never a zero bill.
#[must_use]
pub fn clm_usage_receipt(
    pin: &ClmUsagePin,
    usage: &Value,
    message_entry_id: Option<&str>,
) -> Value {
    let observed = is_observed_clm_usage(usage);
    let mut data = json!({"version":1,"pin":pin});
    if observed {
        data["usage"] = usage.clone();
    }
    if let Some(id) = message_entry_id {
        data["messageEntryId"] = json!(id);
    }
    json!({"customType":if observed {"clm_usage_v1"} else {"clm_usage_unknown_v1"},"data":data})
}

/// Fold measured provider usage for the current accepted revision of a complete branch.
/// # Errors
/// Invalid accepted state or incomplete ancestry remains a fatal CLM error.
pub fn get_clm_measured_usage(branch: &[FileEntry]) -> Result<Option<ClmMeasuredUsage>, ClmError> {
    let state = validate_clm_branch(branch)?;
    if state.mode != ClmMode::On {
        return Ok(None);
    }
    let Some(boundary) = branch.iter().rposition(|entry| {
        matches!(
            entry,
            FileEntry::ClmOn { .. }
                | FileEntry::ClmRevision { .. }
                | FileEntry::ClmOffBoundary { .. }
        )
    }) else {
        return Ok(None);
    };
    let accepted_id = branch[boundary].id();
    let selected: HashSet<_> = branch.iter().filter_map(FileEntry::id).collect();
    let mut attributed = HashSet::new();
    let mut total: Option<ClmMeasuredUsage> = None;
    for entry in branch {
        let FileEntry::Custom { payload, .. } = entry else {
            continue;
        };
        if !payload.custom_type.starts_with("clm_usage_") {
            continue;
        }
        let Some(data) = &payload.data else {
            return Ok(None);
        };
        let Some(pin) = data.get("pin").filter(|v| v.is_object()) else {
            return Ok(None);
        };
        if pin["epochId"].as_str() != state.epoch_id.as_deref()
            || pin["revision"].as_u64() != state.revision
            || pin["entryId"].as_str() != accepted_id
            || (pin.get("branchId") != Some(&Value::Null)
                && !pin["branchId"]
                    .as_str()
                    .is_some_and(|id| selected.contains(id)))
        {
            continue;
        }
        let usage = &data["usage"];
        if payload.custom_type != "clm_usage_v1"
            || data["version"] != 1
            || !is_observed_clm_usage(usage)
        {
            return Ok(None);
        }
        let sum = total.get_or_insert(ClmMeasuredUsage {
            input: 0.,
            output: 0.,
            cache_read: 0.,
            cache_write: 0.,
            cost: 0.,
        });
        sum.input += usage["input"].as_f64().unwrap_or_default();
        sum.output += usage["output"].as_f64().unwrap_or_default();
        sum.cache_read += usage["cacheRead"].as_f64().unwrap_or_default();
        sum.cache_write += usage["cacheWrite"].as_f64().unwrap_or_default();
        sum.cost += usage["cost"]["total"].as_f64().unwrap_or_default();
        if let Some(id) = data["messageEntryId"].as_str() {
            attributed.insert(id);
        }
    }
    if branch[boundary+1..].iter().any(|entry| matches!(entry,
        FileEntry::Message { message: AgentMessage::Assistant(a), .. }
        if a.stop_reason != pa_types::ai::StopReason::Aborted && !entry.id().is_some_and(|id| attributed.contains(id)))) { return Ok(None); }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use crate::clm::get_clm_measured_usage;
    use pa_types::{clm::ClmMeasuredUsage, session::FileEntry};
    use serde_json::{json, Value};
    fn row(v: Value) -> FileEntry {
        serde_json::from_value(v).unwrap()
    }
    fn on() -> FileEntry {
        row(
            json!({"type":"clm_on","id":"on","parentId":null,"epochId":"e","revision":0,"snapshot":{"blocks":[]},"auditWatermark":null}),
        )
    }
    fn receipt(kind: &str, usage: &Value) -> FileEntry {
        row(
            json!({"type":"custom","id":"r","parentId":"on","customType":kind,"data":{"version":1,"pin":{"mode":"on","epochId":"e","revision":0,"entryId":"on","branchId":"on"},"usage":usage}}),
        )
    }
    fn usage() -> Value {
        json!({"input":2,"output":3,"cacheRead":4,"cacheWrite":5,"totalTokens":14,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.25}})
    }
    #[test]
    fn observed_receipts_are_not_replaced_by_child_totals() {
        let child = row(
            json!({"type":"child_usage_attributed","id":"c","parentId":"r","targetId":"unused","origin":"spawn_task","childUsage":pa_types::ai::Usage {output:100,..Default::default()},"aggregateUsage":pa_types::ai::Usage {output:200,..Default::default()}}),
        );
        assert_eq!(
            get_clm_measured_usage(&[on(), receipt("clm_usage_v1", &usage()), child]).unwrap(),
            Some(ClmMeasuredUsage {
                input: 2.,
                output: 3.,
                cache_read: 4.,
                cache_write: 5.,
                cost: 0.25
            })
        );
    }
    #[test]
    fn missing_invalid_unknown_and_unsupported_usage_remain_unknown() {
        assert_eq!(get_clm_measured_usage(&[on()]).unwrap(), None);
        for kind in ["clm_usage_v1", "clm_usage_unknown_v1", "clm_usage_v2"] {
            for value in [json!({}), json!({"input":2}), Value::Null] {
                assert_eq!(
                    get_clm_measured_usage(&[on(), receipt(kind, &value)]).unwrap(),
                    None
                );
            }
        }
        let mut incomplete = usage();
        incomplete.as_object_mut().unwrap().remove("cacheRead");
        assert_eq!(
            get_clm_measured_usage(&[on(), receipt("clm_usage_v1", &incomplete)]).unwrap(),
            None
        );
    }
    #[test]
    fn old_revision_and_foreign_branch_receipts_do_not_count() {
        for (field, value) in [
            ("revision", json!(7)),
            ("entryId", json!("old")),
            ("branchId", json!("other")),
        ] {
            let mut r = serde_json::to_value(receipt("clm_usage_v1", &usage())).unwrap();
            r["data"]["pin"][field] = value;
            assert_eq!(get_clm_measured_usage(&[on(), row(r)]).unwrap(), None);
        }
    }
    #[test]
    fn unattributed_assistant_usage_is_unknown() {
        let a = row(
            json!({"type":"message","id":"a","parentId":"r","message":{"role":"assistant","content":[],"api":"x","provider":"x","model":"x","stopReason":"stop","timestamp":1,"usage":usage()}}),
        );
        assert_eq!(
            get_clm_measured_usage(&[on(), receipt("clm_usage_v1", &usage()), a]).unwrap(),
            None
        );
    }
    #[test]
    fn missing_dispatch_branch_is_not_a_null_root_pin() {
        let mut r = serde_json::to_value(receipt("clm_usage_v1", &usage())).unwrap();
        r["data"]["pin"].as_object_mut().unwrap().remove("branchId");
        assert_eq!(get_clm_measured_usage(&[on(), row(r)]).unwrap(), None);
    }
    #[test]
    fn receipt_producer_preserves_unknown_and_observed_raw_fields() {
        let pin = pa_types::clm::ClmUsagePin {
            mode: pa_types::clm::ClmMode::On,
            branch_id: Some("on".into()),
            epoch_id: Some("e".into()),
            revision: Some(0),
            entry_id: Some("on".into()),
        };
        let observed = super::clm_usage_receipt(&pin, &usage(), Some("assistant"));
        assert_eq!(observed["customType"], "clm_usage_v1");
        assert_eq!(observed["data"]["usage"], usage());
        assert_eq!(observed["data"]["messageEntryId"], "assistant");
        let unknown = super::clm_usage_receipt(&pin, &json!({"input":7}), None);
        assert_eq!(unknown["customType"], "clm_usage_unknown_v1");
        assert!(unknown["data"].get("usage").is_none());
    }
}
