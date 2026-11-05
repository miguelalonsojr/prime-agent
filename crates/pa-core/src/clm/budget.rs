use super::{checksum::js_stringify, ClmError};
use pa_types::{
    ai::{Model, Tool},
    clm::ClmRequestBudget,
};
use serde_json::Value;

pub(super) const BRANCH_PREFIX: &str = "[branch-summary]\n\nThe following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub(super) const BRANCH_SUFFIX: &str = "\n</summary>";
pub(super) const COMPACTION_PREFIX: &str = "[compaction-summary]\n\nThe conversation history before this point was compacted into the following summary.\nThe retained messages below are authoritative; this summary may lag behind them.\n\n<summary>\n";
pub(super) const COMPACTION_SUFFIX: &str = "\n</summary>";
pub(super) const DIGEST_PREFIX: &str = "[harness-digest]\n\nThe persistent memories produced across this session so far:\n\n<harness_state>\n";
pub(super) const DIGEST_SUFFIX: &str = "\n</harness_state>";
fn len(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}
fn quarter(chars: u64) -> u64 {
    chars.div_ceil(4)
}
fn str_len(value: &Value) -> u64 {
    value.as_str().map_or(0, len)
}
fn content(value: &Value) -> u64 {
    if let Some(text) = value.as_str() {
        return quarter(len(text));
    }
    value.as_array().map_or(0, |array| {
        array
            .iter()
            .map(|block| match block["type"].as_str() {
                Some("text") => quarter(str_len(&block["text"])),
                Some("image") => quarter(str_len(&block["data"])).max(1200),
                _ => 0,
            })
            .sum()
    })
}
fn message(value: &Value) -> u64 {
    match value["role"].as_str() {
        Some("user" | "custom") => content(&value["content"]),
        Some("toolResult") => {
            quarter(str_len(&value["toolCallId"]) + str_len(&value["toolName"]))
                + content(&value["content"])
        }
        Some("assistant") => value["content"].as_array().map_or(0, |array| {
            array
                .iter()
                .map(|block| match block["type"].as_str() {
                    Some("text") => quarter(str_len(&block["text"])),
                    Some("thinking") => quarter(str_len(&block["thinking"])),
                    Some("toolCall") => quarter(
                        str_len(&block["id"])
                            + str_len(&block["name"])
                            + js_stringify(&block["arguments"]).map_or(0, |s| len(&s)),
                    ),
                    _ => 0,
                })
                .sum()
        }),
        Some("bashExecution") => quarter(str_len(&value["command"]) + str_len(&value["output"])),
        Some("branchSummary") => {
            quarter(len(BRANCH_PREFIX) + str_len(&value["summary"]) + len(BRANCH_SUFFIX))
        }
        Some("compactionSummary") => {
            let digest = value["harnessDigest"]
                .as_str()
                .filter(|d| !d.is_empty())
                .map_or(0, |d| len(DIGEST_PREFIX) + len(d) + len(DIGEST_SUFFIX) + 2);
            quarter(
                digest
                    + len(COMPACTION_PREFIX)
                    + str_len(&value["summary"])
                    + len(COMPACTION_SUFFIX),
            )
        }
        _ => 0,
    }
}

/// Returns a CLM safety error for invalid or unavailable input.
///
/// # Errors
/// Returns an error if the document, selected ancestry or budget violates CLM policy.
pub fn estimate_clm_request_tokens(
    messages: &[Value],
    system_prompt: &str,
    tools: &[Tool],
    model: &Model,
) -> Result<ClmRequestBudget, ClmError> {
    let mut chars = len(system_prompt);
    for tool in tools {
        chars += len(&tool.name) + len(&tool.description) + len(&js_stringify(&tool.parameters)?);
    }
    let estimated_input_tokens = quarter(chars) + messages.iter().map(message).sum::<u64>();
    let output_reserve_tokens =
        (model.context_window > 0).then(|| model.max_tokens.min(model.context_window / 4));
    let max_input_tokens = output_reserve_tokens.map(|reserve| model.context_window - reserve);
    Ok(ClmRequestBudget {
        estimated_input_tokens,
        output_reserve_tokens,
        max_input_tokens,
    })
}
