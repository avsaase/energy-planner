use std::collections::HashMap;

use jiff::{Timestamp, Zoned};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct WsResultMessage<T = Value> {
    pub id: u64,
    pub success: bool,
    pub result: Option<T>,
    pub error: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct WsEventMessage {
    pub id: u64,
    pub event: TriggerEvent,
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

#[derive(Debug, Deserialize)]
pub struct EntityStatistics(pub HashMap<String, Vec<StatisticsEntry>>);

#[derive(Debug, Deserialize)]
pub struct StatisticsEntry {
    #[serde(deserialize_with = "ms_timestamp::deserialize")]
    pub start: Zoned,
    #[serde(deserialize_with = "ms_timestamp::deserialize")]
    pub end: Zoned,
    pub max: f64,
    pub mean: f64,
    pub min: f64,
    pub last_reset: Option<String>,
}

mod ms_timestamp {
    use jiff::{Zoned, tz::TimeZone};
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Zoned, D::Error> {
        let ms = i64::deserialize(d)?;
        Ok(jiff::Timestamp::from_millisecond(ms)
            .map_err(serde::de::Error::custom)?
            .to_zoned(TimeZone::system()))
    }
}
