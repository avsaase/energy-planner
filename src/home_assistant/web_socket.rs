use anyhow::{Context, bail};
use futures::{SinkExt, StreamExt};
use reqwest::Url;
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};

use crate::home_assistant::{
    client::HaClient,
    types::{EntityState, WsMessage},
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

        let ws_url = Url::parse(&ws_url)?.join("/api/websocket")?;

        println!("URL: {ws_url}");

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

        // Send to msubscribe_trigger message
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

        // Wait for the response
        let message = self.ws.next().await.context("Connection closed")??;
        let Message::Text(message_text) = message else {
            bail!("Unexpected message");
        };
        let message: WsMessage =
            serde_json::from_str(&message_text).context("Unexpected message format")?;
        if message.success.is_none_or(|success| !success) {
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
                    let ws_msg: WsMessage = serde_json::from_str(&text)?;

                    if ws_msg.msg_type != "event" {
                        continue;
                    }

                    let Some(event) = ws_msg.event else {
                        continue;
                    };

                    debug!(
                        entity = ?event.variables.trigger.entity_id,
                        state =  ?event.variables.trigger.to_state,
                        "Received new entity state",

                    );

                    return Ok(event.variables.trigger.to_state);
                }
                Message::Close(_) => bail!("Connection closed"),
                _ => continue,
            }
        }
    }
}
