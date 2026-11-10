//! Validate required wire structure once; unknown fields remain forward compatible.
use anyhow::{Context, Result};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

pub fn decode<T: DeserializeOwned>(value: &Value, context: &str) -> Result<T> {
    serde_json::from_value(value.clone()).with_context(|| format!("invalid {context}"))
}
fn identity<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    let id = String::deserialize(d)?;
    if id.trim().is_empty() {
        return Err(serde::de::Error::custom("empty identity"));
    }
    Ok(id)
}
#[derive(Deserialize)]
pub struct SessionList {
    pub sessions: Vec<Value>,
}
#[derive(Deserialize)]
pub struct BlockList {
    pub blocks: Vec<Value>,
}
#[derive(Deserialize)]
pub struct SessionView {
    pub windows: Vec<Window>,
}
#[derive(Deserialize)]
pub struct Window {
    #[serde(deserialize_with = "identity")]
    pub window_id: String,
    pub layers: Vec<Layer>,
}
#[derive(Deserialize)]
pub struct Layer {
    pub blocks: Vec<Value>,
}
#[derive(Deserialize)]
pub struct Block {
    #[serde(deserialize_with = "identity")]
    pub block_id: String,
    #[serde(default)]
    pub creator_name: Option<String>,
    #[serde(default)]
    pub flavor: Option<String>,
}
#[derive(Deserialize)]
pub struct Session {
    #[serde(deserialize_with = "identity")]
    pub session_id: String,
}
#[derive(Deserialize)]
pub struct Ticket {
    #[serde(deserialize_with = "identity")]
    pub ticket: String,
}

#[derive(Debug, Deserialize)]
pub struct ResolvedTarget {
    pub kind: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Resolution {
    Resolved {
        target: ResolvedTarget,
    },
    Ambiguous {
        candidates: Vec<ResolvedTarget>,
    },
    NotFound {
        #[serde(default)]
        reason: Option<String>,
    },
    #[serde(other)]
    Unknown,
}
#[derive(Deserialize)]
pub struct ResolveReply {
    pub results: Vec<Resolution>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(rename = "type")]
    kind: String,
    payload: T,
}
#[derive(Deserialize)]
struct IncomingStatus {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    seq: u64,
    payload: Envelope<SessionChange>,
}
#[derive(Deserialize)]
struct SessionChange {
    #[serde(deserialize_with = "identity")]
    session_id: String,
    event: Envelope<BlockStatus>,
}
#[derive(Deserialize)]
struct BlockStatus {
    #[serde(deserialize_with = "identity")]
    block_id: String,
    creator_name: String,
    #[serde(flatten)]
    update: StatusUpdate,
}
#[derive(Deserialize)]
#[serde(tag = "name", content = "payload")]
pub enum StatusUpdate {
    #[serde(rename = "program_status_changed")]
    Changed { id: String, record: Value },
    #[serde(rename = "program_status_removed")]
    Removed { ids: Vec<String> },
    #[serde(other)]
    Other,
}
/// Unrelated or malformed events cannot mutate the status snapshot.
pub fn program_status(value: &Value) -> Option<(u64, crate::domain::Target, StatusUpdate)> {
    let event: IncomingStatus = serde_json::from_value(value.clone()).ok()?;
    let session = event.payload;
    let change = session.payload;
    let block = change.event;
    if event.kind != "event"
        || session.kind != "session_event"
        || block.kind != "block_event"
        || block.payload.creator_name != "com.superlogical.terminal"
    {
        return None;
    }
    Some((
        event.seq,
        crate::domain::Target {
            session: change.session_id,
            block: block.payload.block_id,
        },
        block.payload.update,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn required_structure_is_distinct_from_an_empty_collection() {
        assert!(decode::<SessionList>(&json!({}), "session list").is_err());
        assert!(decode::<SessionList>(&json!({"sessions":[]}), "session list").is_ok());
        assert!(decode::<Block>(&json!({"block_id":""}), "block").is_err());
        assert!(decode::<SessionView>(&json!({"windows":[{"window_id":"w"}]}), "view").is_err());
        assert!(decode::<Block>(&json!({"block_id":"b", "future_field":true}), "block").is_ok());
    }
}
