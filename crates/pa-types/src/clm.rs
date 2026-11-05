//! Native CLM wire vocabulary. The snapshot retains the entire ordered source JSON value.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClmBlock {
    pub id: String,
    pub message: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClmSnapshot(Value);
impl ClmSnapshot {
    #[must_use]
    pub fn from_value(value: Value) -> Self {
        Self(value)
    }
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.0
    }
    #[must_use]
    pub fn into_value(self) -> Value {
        self.0
    }
    #[must_use]
    pub fn from_blocks(blocks: Vec<ClmBlock>) -> Self {
        let mut object = serde_json::Map::new();
        object.insert(
            "blocks".into(),
            Value::Array(
                blocks
                    .into_iter()
                    .map(|block| {
                        let mut fields = serde_json::Map::new();
                        fields.insert("id".into(), Value::String(block.id));
                        fields.insert("message".into(), block.message);
                        Value::Object(fields)
                    })
                    .collect(),
            ),
        );
        Self(Value::Object(object))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClmMode {
    On,
    Off,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClmModeSource {
    ConfiguredDefault,
    InheritedChild,
    SessionOverride,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmBoundary {
    pub epoch_id: String,
    pub revision: u64,
    pub snapshot: ClmSnapshot,
    pub audit_watermark: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmOnEntry {
    #[serde(flatten)]
    pub boundary: ClmBoundary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_source: Option<ClmModeSource>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmRevisionEntry {
    #[serde(flatten)]
    pub boundary: ClmBoundary,
    pub base_revision: u64,
    pub checksum: String,
    pub observed_action_id: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmOffBoundaryEntry {
    #[serde(flatten)]
    pub boundary: ClmBoundary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_source: Option<ClmModeSource>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmBranchState {
    pub mode: ClmMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<ClmSnapshot>,
    pub audit_watermark: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmRevisionRef {
    pub epoch_id: String,
    pub revision: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmUsagePin {
    pub mode: ClmMode,
    pub branch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmRequestBudget {
    pub estimated_input_tokens: u64,
    pub output_reserve_tokens: Option<u64>,
    pub max_input_tokens: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmMeasuredUsage {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cost: f64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClmStatusSource {
    ConfiguredDefault,
    InheritedChild,
    SessionOverride,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmStatus {
    pub mode_source: ClmStatusSource,
    pub recent_revisions: Vec<ClmRevisionRef>,
    pub configured_default: bool,
    pub mode: ClmMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_occupancy: Option<ClmRequestBudget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_usage: Option<ClmMeasuredUsage>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClmAcceptedView {
    pub epoch_id: String,
    pub revision: u64,
    pub messages: Vec<Value>,
}
