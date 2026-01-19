use std::collections::HashMap;

use anyhow::{Context, bail};
use futures::{SinkExt, StreamExt, future::try_join_all};
use itertools::Itertools;
use jiff::{Timestamp, ToSpan, tz::TimeZone};
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;

use crate::{
    PLANNING_INTERVAL_MINUTES,
    home_assistant::addon::running_as_addon,
    types::{ElectricityPrice, SolarForecast},
};

#[derive(Debug, Clone)]
pub struct HaClient {
    http_client: reqwest::Client,
    base_url: Url,
    token: SecretString,
}

impl HaClient {
    pub fn new() -> anyhow::Result<Self> {
        let base_url = Self::get_base_url()?;
        let token = Self::get_token()?;
        Ok(Self::with_url_and_token(base_url, token))
    }

    pub fn with_url_and_token(base_url: Url, token: SecretString) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            base_url,
            token,
        }
    }

    pub fn get_token() -> anyhow::Result<SecretString> {
        match std::env::var("SUPERVISOR_TOKEN") {
            Ok(token) => Ok(token.into()),
            Err(_) => std::env::var("HA_TOKEN")
                .map(|t| t.into())
                .context("Neither SUPERVISOR_TOKEN or HA_TOKEN is set"),
        }
    }

    pub fn get_base_url() -> anyhow::Result<Url> {
        if running_as_addon() {
            Ok(Url::parse("http://supervisor/core").expect("URL is valid"))
        } else {
            std::env::var("HA_URL")
                .context("HA_URL environment variable is not set")
                .and_then(|url_str| Url::parse(&url_str).context("Failed to parse HA_URL"))
        }
    }

    #[tracing::instrument(skip(self))]
    async fn get_entity_state<S, A>(&self, entity_id: &str) -> anyhow::Result<EntityState<S, A>>
    where
        S: for<'de> Deserialize<'de>,
        A: for<'de> Deserialize<'de>,
    {
        let url = self.base_url.join(&format!("api/states/{}", entity_id))?;
        let state = self
            .http_client
            .get(url)
            .bearer_auth(self.token.expose_secret())
            .send()
            .await?
            .error_for_status()?
            .json::<EntityState<S, A>>()
            .await?;

        Ok(state)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_solar_forecast(
        &self,
        entity_ids: &[String],
    ) -> anyhow::Result<Vec<SolarForecast>> {
        #[derive(Debug, Deserialize)]
        struct SolarForecastAttributes {
            #[serde(deserialize_with = "deserialize_map_as_vec")]
            pub watts: Vec<(Timestamp, f64)>,
        }

        let states =
            try_join_all(entity_ids.iter().map(|entity_id| {
                self.get_entity_state::<String, SolarForecastAttributes>(entity_id)
            }))
            .await?;

        let forecasts = states
            .into_iter()
            .flat_map(|state| state.attributes.watts)
            .sorted_by_key(|(ts, _)| *ts)
            .dedup_by(|(a_ts, _), (b_ts, _)| a_ts == b_ts)
            .map(|(ts, watts)| {
                let start = ts.to_zoned(TimeZone::system());
                let end = &start + PLANNING_INTERVAL_MINUTES.minutes();
                SolarForecast {
                    start,
                    end,
                    forecast_w: watts,
                }
            })
            .collect();

        Ok(forecasts)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_electricity_prices(
        &self,
        entity_id: &str,
    ) -> anyhow::Result<Vec<ElectricityPrice>> {
        #[derive(Debug, Deserialize)]
        struct ElectrictyPriceAttributes {
            prices: Vec<ElectricityPriceEntry>,
        }

        #[derive(Debug, Deserialize)]
        struct ElectricityPriceEntry {
            from: Timestamp,
            till: Timestamp,
            price: f64,
        }

        let state = self
            .get_entity_state::<String, ElectrictyPriceAttributes>(entity_id)
            .await?;

        let prices = state
            .attributes
            .prices
            .into_iter()
            .map(|entry| ElectricityPrice {
                start: entry.from.to_zoned(TimeZone::system()),
                end: entry.till.to_zoned(TimeZone::system()),
                price_per_kwh: entry.price,
            })
            .sorted_by_key(|price| price.start.clone())
            .collect();

        Ok(prices)
    }

    #[tracing::instrument(skip(self))]
    #[expect(unused)]
    async fn get_long_term_statistics(
        &self,
        start: Timestamp,
        end: Timestamp,
        entity_id: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let ws_url = self
            .base_url
            .as_str()
            .replace("http://", "ws://")
            .replace("https://", "wss://");
        let ws_url = format!("{ws_url}/api/websocket");

        let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url).await?;

        // Step 1: Receive auth_required
        ws.next()
            .await
            .ok_or(anyhow::anyhow!("connection closed"))??;

        // Step 2: Authenticate
        ws.send(Message::text(
            serde_json::json!({
                "type": "auth",
                "access_token": self.token.expose_secret()
            })
            .to_string(),
        ))
        .await?;

        // Step 3: Receive auth_ok
        let auth_response = ws
            .next()
            .await
            .ok_or(anyhow::anyhow!("connection closed"))??;
        let auth_msg: serde_json::Value = serde_json::from_str(auth_response.to_text()?)?;
        if auth_msg["type"] != "auth_ok" {
            anyhow::bail!("authentication failed: {}", auth_msg);
        }

        // Step 4: Request statistics
        ws.send(Message::text(
            serde_json::json!({
                "id": 1,
                "type": "recorder/statistics_during_period",
                "start_time": start,
                "end_time": end,
                "statistic_ids": [entity_id],
                "period": "hour",
            })
            .to_string(),
        ))
        .await?;

        // Step 5: Receive result
        #[derive(Deserialize)]
        struct WsResponse {
            #[allow(unused)]
            id: u64,
            result: Option<serde_json::Value>,
            success: Option<bool>,
        }

        let response = ws
            .next()
            .await
            .ok_or(anyhow::anyhow!("connection closed"))??;
        let parsed: WsResponse = serde_json::from_str(response.to_text()?)?;

        if parsed.success != Some(true) {
            bail!("statistics request failed: {:?}", parsed.result);
        }

        ws.close(None).await?;

        parsed
            .result
            .ok_or(anyhow::anyhow!("no result in response"))
    }
}

#[derive(Debug, Deserialize)]
#[expect(unused)]
pub struct EntityState<S, A> {
    state: S,
    attributes: A,
}

fn deserialize_map_as_vec<'de, D>(deserializer: D) -> Result<Vec<(Timestamp, f64)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let map: HashMap<Timestamp, f64> = HashMap::deserialize(deserializer)?;
    let mut vec: Vec<(Timestamp, f64)> = map.into_iter().collect();
    vec.sort_by_key(|(timestamp, _)| *timestamp);
    Ok(vec)
}
