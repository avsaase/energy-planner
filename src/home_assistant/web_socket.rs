use anyhow::{Context, bail};
use futures::{SinkExt, StreamExt};
use jiff::Zoned;
use reqwest::Url;
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};

use crate::home_assistant::{
    client::HaClient,
    types::{EntityState, EntityStatistics, WsEventMessage, WsResultMessage},
};

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

        info!("Created authenticated websocket connection");

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

    pub async fn next_state_change(&mut self) -> anyhow::Result<EntityState<Value>> {
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

                    return Ok(msg.event.variables.trigger.to_state);
                }
                Message::Close(_) => bail!("Connection closed"),
                _ => continue,
            }
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_statistics(
        &mut self,
        entity_id: &str,
        start: Zoned,
        end: Zoned,
        period: &str,
    ) -> anyhow::Result<EntityStatistics> {
        let id = self.next_id;
        self.next_id += 1;

        self.ws
            .send(Message::text(
                json!(
                    {
                        "id": id,
                        "type": "recorder/statistics_during_period",
                        "start_time": start.timestamp(),
                        "end_time": end.timestamp(),
                        "statistic_ids": [entity_id],
                        "period": period,
                    }
                )
                .to_string(),
            ))
            .await?;

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

                    let msg: WsResultMessage<EntityStatistics> = serde_json::from_value(raw)?;
                    if !msg.success {
                        bail!("Failed to get statistics: {:?}", msg.error);
                    }

                    let result = msg.result.context("Missing result field in response")?;
                    return Ok(result);
                }
                Message::Close(_) => bail!("Connection closed"),
                _ => continue,
            }
        }
    }
}
