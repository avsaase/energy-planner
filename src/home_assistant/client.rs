use std::collections::HashMap;

use anyhow::Context;
use futures::future::try_join_all;
use itertools::Itertools;
use jiff::{Timestamp, ToSpan, Zoned, tz::TimeZone};
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::{
    PLANNING_INTERVAL_MINUTES,
    home_assistant::{addon::running_as_addon, types::EntityState},
    types::{ElectricityPrice, ElectricityPrices, SolarForecast, SolarForecasts},
};

#[derive(Debug, Clone)]
pub struct HaClient {
    pub(super) http_client: reqwest::Client,
    pub(super) base_url: Url,
    pub(super) token: SecretString,
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

    fn get_token() -> anyhow::Result<SecretString> {
        match std::env::var("SUPERVISOR_TOKEN") {
            Ok(token) => Ok(token.into()),
            Err(_) => std::env::var("HA_TOKEN")
                .map(|t| t.into())
                .context("Neither SUPERVISOR_TOKEN or HA_TOKEN is set"),
        }
    }

    fn get_base_url() -> anyhow::Result<Url> {
        if running_as_addon() {
            Ok(Url::parse("http://supervisor/core/").expect("URL is valid"))
        } else {
            std::env::var("HA_URL")
                .context("HA_URL environment variable is not set")
                .and_then(|url_str| Url::parse(&url_str).context("Failed to parse HA_URL"))
        }
    }

    #[tracing::instrument(skip(self))]
    async fn get_entity_state<A>(&self, entity_id: &str) -> anyhow::Result<EntityState<A>>
    where
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
            .json::<EntityState<A>>()
            .await?;

        Ok(state)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_solar_forecast(
        &self,
        entity_ids: &[String],
    ) -> anyhow::Result<SolarForecasts> {
        #[derive(Debug, Deserialize)]
        struct SolarForecastAttributes {
            #[serde(deserialize_with = "deserialize_map_as_vec")]
            pub watts: Vec<(Timestamp, f64)>,
        }

        let states = try_join_all(
            entity_ids
                .iter()
                .map(|entity_id| self.get_entity_state::<SolarForecastAttributes>(entity_id)),
        )
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

        Ok(SolarForecasts {
            updated_at: Zoned::now(),
            forecasts,
        })
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_electricity_prices(
        &self,
        entity_id: &str,
    ) -> anyhow::Result<ElectricityPrices> {
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
            .get_entity_state::<ElectrictyPriceAttributes>(entity_id)
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

        Ok(ElectricityPrices {
            updated_at: Zoned::now(),
            prices,
        })
    }
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
