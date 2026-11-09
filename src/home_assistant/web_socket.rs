use std::collections::HashMap;

use anyhow::{Context, bail};
use futures::{SinkExt, StreamExt};
use jiff::{SignedDuration, Timestamp};
use reqwest::Url;
use secrecy::ExposeSecret;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};

use crate::{
    home_assistant::{
        client::HaClient,
        types::{EntityState, EntityStatistics, WsEventMessage, WsResultMessage},
    },
    types::BatteryIntent,
};

/// Time window per history request (see [`HaWebSocket::get_history`]).
const HISTORY_CHUNK: SignedDuration = SignedDuration::from_hours(6);

pub struct HaWebSocket {
    pub(super) ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    pub(super) next_id: u64,
}

impl HaClient {
    pub async fn connect_websocket(&self) -> anyhow::Result<HaWebSocket> {
        let ws_url = self
            .base_url
            .to_string()
            .replace("http://", "ws://")
            .replace("https://", "wss://");

        let ws_url = Url::parse(&ws_url)?.join("api/websocket")?;

        let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url.to_string()).await?;

        // Receive auth required
        ws.next().await.context("Connection closed")??;

        // Send auth
        ws.send(Message::text(
            json!(
                {
                    "type": "auth",
                    "access_token": self.token.expose_secret()
                }
            )
            .to_string(),
        ))
        .await?;

        // Receive auth ok
        let message = ws.next().await.context("Connection closed")??;
        let Message::Text(message_text) = message else {
            bail!("Unexpected message type");
        };
        let message_json: Value = serde_json::from_str(&message_text).context("Invalid message")?;
        if message_json["type"] != "auth_ok" {
            bail!("Authentication failed");
        }

        debug!("Created authenticated websocket connection");

        Ok(HaWebSocket { ws, next_id: 1 })
    }
}

impl HaWebSocket {
    pub async fn subscribe_to_entities(&mut self, entity_ids: &[&str]) -> anyhow::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;

        self.ws
            .send(Message::text(
                json!(
                    {
                        "id": id,
                        "type": "subscribe_trigger",
                        "trigger": {
                            "platform": "state",
                            "entity_id": entity_ids,
                        }
                    }
                )
                .to_string(),
            ))
            .await?;

        // The immediate response to a command is always a result message.
        let message = self.ws.next().await.context("Connection closed")??;
        let Message::Text(message_text) = message else {
            bail!("Unexpected message");
        };
        let message: WsResultMessage =
            serde_json::from_str(&message_text).context("Unexpected message format")?;
        if !message.success {
            error!(error = ?message.error, "Failed to subscribe");
            bail!("Failed to subscribe");
        }

        Ok(message.id)
    }

    /// Next state change from a [`Self::subscribe_to_entities`] subscription as
    /// `(entity_id, new_state)`. The state is `None` when the entity was removed.
    pub async fn next_state_change(
        &mut self,
    ) -> anyhow::Result<(String, Option<EntityState<Value>>)> {
        loop {
            let message = self.ws.next().await.context("Connection closed")??;

            match message {
                Message::Ping(bytes) => {
                    self.ws.send(Message::Pong(bytes)).await?;
                    continue;
                }
                Message::Text(text) => {
                    // Check the type tag before committing to a full parse.
                    let raw: Value = serde_json::from_str(&text)?;
                    if raw["type"] != "event" {
                        continue;
                    }

                    let msg: WsEventMessage = serde_json::from_value(raw)?;

                    debug!(
                        entity = ?msg.event.variables.trigger.entity_id,
                        state  = ?msg.event.variables.trigger.to_state,
                        "Received new entity state",
                    );

                    let trigger = msg.event.variables.trigger;
                    return Ok((trigger.entity_id, trigger.to_state));
                }
                Message::Close(_) => bail!("Connection closed"),
                _ => continue,
            }
        }
    }

    /// Sends a command and waits for its result message.
    async fn command<T: DeserializeOwned>(&mut self, mut payload: Value) -> anyhow::Result<T> {
        let id = self.next_id;
        self.next_id += 1;
        payload["id"] = id.into();

        self.ws.send(Message::text(payload.to_string())).await?;

        loop {
            let message = self.ws.next().await.context("Connection closed")??;
            match message {
                Message::Ping(bytes) => {
                    self.ws.send(Message::Pong(bytes)).await?;
                    continue;
                }
                Message::Text(text) => {
                    let raw: Value = serde_json::from_str(&text)?;
                    if raw["type"] != "result" || raw["id"].as_u64() != Some(id) {
                        continue;
                    }
                    let msg: WsResultMessage<Value> = serde_json::from_value(raw)?;
                    if !msg.success {
                        bail!("Command {} failed: {:?}", payload["type"], msg.error);
                    }
                    return serde_json::from_value(msg.result.unwrap_or(Value::Null))
                        .with_context(|| format!("Unexpected result for {}", payload["type"]));
                }
                Message::Close(_) => bail!("Connection closed"),
                _ => continue,
            }
        }
    }

    #[tracing::instrument(skip(self, service_data))]
    pub async fn call_service(
        &mut self,
        domain: &str,
        service: &str,
        service_data: Value,
    ) -> anyhow::Result<()> {
        self.command::<Value>(json!({
            "type": "call_service",
            "domain": domain,
            "service": service,
            "service_data": service_data,
        }))
        .await?;
        Ok(())
    }

    /// Raw state changes of several entities between `start` and `end` (`NaN` for non-numeric
    /// states such as `unavailable`). Values are as reported, in each entity's own unit.
    ///
    /// Fetched per entity in [`HISTORY_CHUNK`] windows: a 1 Hz sensor produces ~3 MB of history
    /// per day, and a websocket message may not exceed 16 MiB.
    #[tracing::instrument(skip(self))]
    pub async fn get_history(
        &mut self,
        entity_ids: &[&str],
        start: Timestamp,
        end: Timestamp,
    ) -> anyhow::Result<HashMap<String, Vec<(Timestamp, f64)>>> {
        let mut history: HashMap<String, Vec<(Timestamp, f64)>> = HashMap::new();
        for &entity in entity_ids {
            let points = history.entry(entity.to_string()).or_default();
            let mut chunk_start = start;
            while chunk_start < end {
                let chunk_end = (chunk_start + HISTORY_CHUNK).min(end);
                let chunk = self
                    .get_history_chunk(entity, chunk_start, chunk_end)
                    .await
                    .with_context(|| format!("History of {entity} from {chunk_start}"))?;
                // Each chunk repeats the state at its start time; skip what we already have.
                let last = points.last().map(|(t, _)| *t);
                points.extend(
                    chunk
                        .into_iter()
                        .filter(|(t, _)| last.is_none_or(|l| *t > l)),
                );
                chunk_start = chunk_end;
            }
        }
        Ok(history)
    }

    async fn get_history_chunk(
        &mut self,
        entity_id: &str,
        start: Timestamp,
        end: Timestamp,
    ) -> anyhow::Result<Vec<(Timestamp, f64)>> {
        #[derive(Deserialize)]
        struct CompressedState {
            s: String,
            /// Last updated / last changed, epoch seconds. Which keys are present depends on the
            /// HA version and on whether the two differ.
            lu: Option<f64>,
            lc: Option<f64>,
        }
        let mut result: HashMap<String, Vec<CompressedState>> = self
            .command(json!({
                "type": "history/history_during_period",
                "start_time": start,
                "end_time": end,
                "entity_ids": [entity_id],
                "minimal_response": true,
                "no_attributes": true,
                "significant_changes_only": false,
            }))
            .await?;
        Ok(result
            .remove(entity_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|st| {
                let secs = st.lc.or(st.lu)?;
                let at = Timestamp::from_millisecond((secs * 1000.0) as i64).ok()?;
                Some((at, st.s.parse().unwrap_or(f64::NAN)))
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    pub async fn apply_battery_intent(&mut self, intent: &BatteryIntent) -> anyhow::Result<()> {
        info!(?intent, "Applying battery intent");
        match intent {
            BatteryIntent::Idle | BatteryIntent::Other => {
                self.call_service("script", "battery_set_idle", json!({}))
                    .await
            }
            BatteryIntent::Balance => {
                self.call_service("script", "battery_set_balance", json!({}))
                    .await
            }
            BatteryIntent::BalanceChargeOnly => {
                self.call_service("script", "battery_set_balance_charge_only", json!({}))
                    .await
            }
            BatteryIntent::BalanceDischargeOnly => {
                self.call_service("script", "battery_set_balance_discharge_only", json!({}))
                    .await
            }
            BatteryIntent::FixedCharge { power_w } => {
                self.call_service(
                    "script",
                    "battery_set_fixed_charge",
                    json!({ "charge_power": power_w.round() as i64 }),
                )
                .await
            }
            BatteryIntent::FixedDischarge { power_w } => {
                self.call_service(
                    "script",
                    "battery_set_fixed_discharge",
                    json!({ "discharge_power": power_w.round() as i64 }),
                )
                .await
            }
        }
    }

    /// Long-term statistics, with power converted to W.
    #[tracing::instrument(skip(self))]
    pub async fn get_statistics(
        &mut self,
        entity_ids: &[&str],
        start: Timestamp,
        end: Timestamp,
        period: &str,
    ) -> anyhow::Result<EntityStatistics> {
        self.command(json!({
            "type": "recorder/statistics_during_period",
            "start_time": start,
            "end_time": end,
            "statistic_ids": entity_ids,
            "period": period,
            "types": ["mean"],
            "units": { "power": "W" },
        }))
        .await
    }
}
