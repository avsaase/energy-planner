use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct WsMessage {
    pub id: u64,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub event: Option<TriggerEvent>,
    // For result messages
    pub success: Option<bool>,
    pub error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct TriggerEvent {
    pub variables: TriggerVariables,
}

#[derive(Debug, Deserialize)]
pub struct TriggerVariables {
    pub trigger: TriggerData,
}

#[derive(Debug, Deserialize)]
pub struct TriggerData {
    pub entity_id: String,
    #[expect(unused)]
    pub from_state: Option<EntityState<Value>>,
    pub to_state: EntityState<Value>,
}

#[derive(Debug, Deserialize)]
pub struct EntityState<A> {
    pub state: String,
    pub attributes: A,
    pub last_changed: Timestamp,
    pub last_updated: Timestamp,
}
