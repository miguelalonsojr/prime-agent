use super::{ClmError, ProtectedActions};
use pa_types::clm::{ClmBlock, ClmSnapshot};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

fn encoded(id: &str) -> String {
    let mut out = String::new();
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            write!(out, "%{b:02X}").expect("String formatting is infallible");
        }
    }
    out
}
fn escape(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            if line.starts_with('\\') || line.starts_with("[[CTX_") {
                format!("\\{line}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn blocks(snapshot: &ClmSnapshot) -> Result<Vec<ClmBlock>, ClmError> {
    let values = snapshot
        .as_value()
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or_else(|| ClmError::document("snapshot blocks missing"))?;
    values
        .iter()
        .map(|value| {
            serde_json::from_value::<ClmBlock>(value.clone())
                .map_err(|e| ClmError::document(e.to_string()))
        })
        .collect()
}
fn role(block: &ClmBlock) -> Result<&str, ClmError> {
    block
        .message
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| ClmError::document("block role missing"))
}
fn body(block: &ClmBlock) -> Result<String, ClmError> {
    let msg = &block.message;
    let role = role(block)?;
    let content = match role {
        "bashExecution" if msg["excludeFromContext"] == true => {
            return Ok(escape(&super::checksum::js_stringify(msg)?))
        }
        "bashExecution" => {
            let output = msg["output"].as_str().unwrap_or("");
            let max_ticks = output
                .chars()
                .fold((0usize, 0usize), |(run, max), c| {
                    if c == '`' {
                        (run + 1, max.max(run + 1))
                    } else {
                        (0, max)
                    }
                })
                .1;
            let fence = "`".repeat(max_ticks.saturating_add(1).max(3));
            let mut rendered = if output.is_empty() {
                "(no output)".into()
            } else {
                format!("{fence}\n{output}\n{fence}")
            };
            if msg["cancelled"] == true {
                rendered.push_str("\n\n(command cancelled)");
            } else if msg["exitCode"].as_i64().is_some_and(|code| code != 0) {
                write!(rendered, "\n\nCommand exited with code {}", msg["exitCode"])
                    .expect("String formatting is infallible");
            }
            if msg["truncated"] == true {
                match msg["fullOutputPath"].as_str().filter(|s| !s.is_empty()) {
                    Some(path) => {
                        write!(rendered, "\n\n[Output truncated. Full output: {path}]")
                            .expect("String formatting is infallible");
                    }
                    None => rendered.push_str("\n\n[Output truncated.]"),
                }
            }
            return Ok(escape(&format!(
                "Ran `{}\x60\n{}",
                msg["command"].as_str().unwrap_or(""),
                rendered
            )));
        }
        "branchSummary" => {
            return Ok(escape(&format!(
                "{}{}{}",
                super::budget::BRANCH_PREFIX,
                msg["summary"].as_str().unwrap_or(""),
                super::budget::BRANCH_SUFFIX
            )))
        }
        "compactionSummary" => {
            let digest = msg["harnessDigest"]
                .as_str()
                .filter(|d| !d.is_empty())
                .map_or(String::new(), |d| {
                    format!(
                        "{}{}{}\n\n",
                        super::budget::DIGEST_PREFIX,
                        d,
                        super::budget::DIGEST_SUFFIX
                    )
                });
            return Ok(escape(&format!(
                "{}{}{}{}",
                digest,
                super::budget::COMPACTION_PREFIX,
                msg["summary"].as_str().unwrap_or(""),
                super::budget::COMPACTION_SUFFIX
            )));
        }
        _ => &msg["content"],
    };
    if let Some(text) = content.as_str() {
        return Ok(escape(text));
    }
    let Some(items) = content.as_array() else {
        return Ok(escape(&super::checksum::js_stringify(msg)?));
    };
    let mut parts = Vec::new();
    if role == "toolResult" {
        parts.push(escape(&format!(
            "Tool result {} {}",
            msg["toolCallId"].as_str().unwrap_or(""),
            msg["toolName"].as_str().unwrap_or("")
        )));
    }
    for (index, item) in items.iter().enumerate() {
        parts.push(match item["type"].as_str().unwrap_or("") {
            "text" => escape(item["text"].as_str().unwrap_or("")),
            "thinking" => escape(&format!(
                "Thinking: {}",
                item["thinking"].as_str().unwrap_or("")
            )),
            "image" => format!("[[CTX_IMAGE id={}:{}]]", encoded(&block.id), index),
            "toolCall" => escape(&format!(
                "Tool call {} {}: {}",
                item["id"].as_str().unwrap_or(""),
                item["name"].as_str().unwrap_or(""),
                super::checksum::js_stringify(&item["arguments"])?
            )),
            _ => return Err(ClmError::document("unsupported content type")),
        });
    }
    Ok(parts.join("\n"))
}

/// Returns a CLM safety error for invalid or unavailable input.
///
/// # Errors
/// Returns an error if the document, selected ancestry or budget violates CLM policy.
// Size is enforced on editable mirror writes by the controller. Accepted historical
// snapshots can still be rendered for read-only inspection above the edit limit.
pub fn render_context_document<S: std::hash::BuildHasher>(
    snapshot: &ClmSnapshot,
    protected_ids: &HashSet<String, S>,
) -> Result<String, ClmError> {
    let mut result = String::new();
    for block in blocks(snapshot)? {
        let id = encoded(&block.id);
        if protected_ids.contains(&block.id) {
            writeln!(result, "[[CTX_PROTECTED id={id}]]").expect("String formatting is infallible");
        } else {
            write!(
                result,
                "[[CTX_TURN id={id} role={}]]\n{}\n[[CTX_END]]\n",
                role(&block)?,
                body(&block)?
            )
            .expect("String formatting is infallible");
        }
    }
    Ok(result)
}

struct DocumentBlock {
    base: ClmBlock,
    raw: Value,
    body: String,
    role: String,
    protected: bool,
}
fn tool_groups(base: &[ClmBlock]) -> Result<Vec<Vec<String>>, ClmError> {
    let mut groups = Vec::new();
    let mut index = 0;
    while index < base.len() {
        let block = &base[index];
        if role(block)? == "toolResult" {
            return Err(ClmError::document("orphan tool result"));
        }
        if role(block)? != "assistant" {
            index += 1;
            continue;
        }
        let calls: Vec<_> = block.message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "toolCall")
            .collect();
        if calls.is_empty() {
            index += 1;
            continue;
        }
        let mut pending = HashMap::new();
        for call in &calls {
            let id = call["id"]
                .as_str()
                .ok_or_else(|| ClmError::document("tool call id missing"))?;
            if pending.insert(id, call["name"].as_str()).is_some() {
                return Err(ClmError::document("duplicate tool-call id in group"));
            }
        }
        let mut group = vec![block.id.clone()];
        for _ in calls {
            index += 1;
            let result = base
                .get(index)
                .ok_or_else(|| ClmError::document("incomplete tool group"))?;
            if role(result)? != "toolResult"
                || pending
                    .remove(result.message["toolCallId"].as_str().unwrap_or(""))
                    .flatten()
                    != result.message["toolName"].as_str()
            {
                return Err(ClmError::document("mismatched tool group"));
            }
            group.push(result.id.clone());
        }
        groups.push(group);
        index += 1;
    }
    Ok(groups)
}
fn parse_body(body: &str, base: &ClmBlock) -> Result<Vec<Value>, ClmError> {
    let mut content = Vec::new();
    let mut lines = Vec::new();
    let flush = |content: &mut Vec<Value>, lines: &mut Vec<String>| {
        if !lines.is_empty() {
            content.push(json!({"type":"text","text":lines.join("\n")}));
            lines.clear();
        }
    };
    for line in body.split('\n') {
        if line.starts_with("[[CTX_IMAGE") {
            let reference = line
                .strip_prefix("[[CTX_IMAGE id=")
                .and_then(|s| s.strip_suffix("]]"));
            let image = base.message["content"]
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .enumerate()
                        .find(|(i, item)| {
                            item["type"] == "image"
                                && reference == Some(format!("{}:{i}", encoded(&base.id)).as_str())
                        })
                        .map(|(_, item)| item.clone())
                })
                .ok_or_else(|| ClmError::document("unknown or malformed image reference"))?;
            flush(&mut content, &mut lines);
            content.push(image);
        } else if line.starts_with("[[CTX_PROTECTED") {
            return Err(ClmError::document("malformed protected reference"));
        } else {
            lines.push(
                line.strip_prefix('\\')
                    .filter(|s| s.starts_with('\\') || s.starts_with("[[CTX_"))
                    .unwrap_or(line)
                    .to_owned(),
            );
        }
    }
    flush(&mut content, &mut lines);
    Ok(content)
}
/// Returns a CLM safety error for invalid or unavailable input.
///
/// # Errors
/// Returns an error if the document, selected ancestry or budget violates CLM policy.
pub fn parse_context_document(text: &str, base: &ClmSnapshot) -> Result<ClmSnapshot, ClmError> {
    let original = blocks(base)?;
    let mut registry = HashMap::new();
    for (index, block) in original.iter().enumerate() {
        if block.id.is_empty() || registry.insert(encoded(&block.id), index).is_some() {
            return Err(ClmError::document("invalid or duplicate CLM block ID"));
        }
    }
    let original_raw = base.as_value()["blocks"]
        .as_array()
        .ok_or_else(|| ClmError::document("snapshot blocks missing"))?;
    let mut parsed = Vec::new();
    let mut seen = HashSet::new();
    let mut lines = text.split('\n');
    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        let protected = line
            .strip_prefix("[[CTX_PROTECTED id=")
            .and_then(|s| s.strip_suffix("]]"));
        let turn = line
            .strip_prefix("[[CTX_TURN id=")
            .and_then(|s| s.strip_suffix("]]"));
        let (id, role_label) = if let Some(id) = protected {
            (id, None)
        } else if let Some(header) = turn {
            let (id, role) = header
                .split_once(" role=")
                .ok_or_else(|| ClmError::document("malformed header"))?;
            (id, Some(role))
        } else {
            return Err(ClmError::document("malformed header"));
        };
        let index = *registry
            .get(id)
            .ok_or_else(|| ClmError::document("unknown block reference"))?;
        let block = &original[index];
        if !seen.insert(&block.id) {
            return Err(ClmError::document("duplicate block reference"));
        }
        let body_text = if protected.is_some() {
            body(block)?
        } else {
            let mut parts = Vec::new();
            loop {
                let line = lines
                    .next()
                    .ok_or_else(|| ClmError::document("unterminated block"))?;
                if line == "[[CTX_END]]" {
                    break;
                }
                parts.push(line);
            }
            parts.join("\n")
        };
        parsed.push(DocumentBlock {
            base: block.clone(),
            raw: original_raw[index].clone(),
            body: body_text,
            role: role_label.unwrap_or(role(block)?).into(),
            protected: protected.is_some(),
        });
    }
    let positions: HashMap<_, _> = parsed
        .iter()
        .enumerate()
        .map(|(i, b)| (b.base.id.as_str(), i))
        .collect();
    let mut changed = HashSet::new();
    for group in tool_groups(&original)? {
        let intact = positions.get(group[0].as_str()).is_some_and(|&start| {
            group.iter().enumerate().all(|(offset, id)| {
                parsed.get(start + offset).is_some_and(|b| {
                    b.base.id == *id
                        && b.role == role(&b.base).unwrap_or("")
                        && b.body == body(&b.base).unwrap_or_default()
                })
            })
        });
        if !intact {
            changed.extend(group);
        }
    }
    let mut result = Vec::new();
    for block in parsed {
        let content = parse_body(&block.body, &block.base)?;
        if block.role == role(&block.base)?
            && block.body == body(&block.base)?
            && !changed.contains(&block.base.id)
        {
            result.push(block.raw);
            continue;
        }
        if block.protected {
            return Err(ClmError::Protection(
                "edited tool group contains protected reference".into(),
            ));
        }
        let mut content = content;
        if block.role != role(&block.base)? {
            content.insert(0,json!({"type":"text","text":format!("[[CTX_TURN id={} role={}]]",encoded(&block.base.id),block.role)}));
        }
        if role(&block.base)? != "user" {
            content.insert(
                0,
                json!({"type":"text","text":format!("[Historical {}]",role(&block.base)?)}),
            );
        }
        result.push(json!({"id":block.base.id,"message":{"role":"user","content":content,"timestamp":block.base.message["timestamp"]}}));
    }
    let mut snapshot = serde_json::Map::new();
    snapshot.insert("blocks".into(), Value::Array(result));
    Ok(ClmSnapshot::from_value(Value::Object(snapshot)))
}

/// Returns a CLM safety error for invalid or unavailable input.
///
/// # Errors
/// Returns an error if the document, selected ancestry or budget violates CLM policy.
pub fn validate_protection(
    candidate: &ClmSnapshot,
    actions: &ProtectedActions,
) -> Result<(), ClmError> {
    let blocks = blocks(candidate)?;
    for action in actions.values() {
        for protected in action {
            if blocks
                .iter()
                .filter(|b| b.id == protected.id && b.message == protected.message)
                .count()
                != 1
            {
                return Err(ClmError::Protection(format!(
                    "protected block {} changed or removed",
                    protected.id
                )));
            }
        }
    }
    Ok(())
}
