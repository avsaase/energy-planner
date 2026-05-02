use anyhow::Context;
use jiff::{Timestamp, ToSpan, Zoned, tz::TimeZone};
use serde::Deserialize;

use crate::types::{ElectricityPrice, ElectricityPrices};

const EPEX_PREDICTOR_URL: &str = "https://epexpredictor.batzill.com/prices?hours=-1&surcharge=0&taxPercent=0&region=NL&evaluation=false&unit=EUR_PER_KWH&hourly=false&timezone=Europe%2FAmsterdam";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpexPriceForecasts {
    prices: Vec<EpexPriceForecast>,
    known_until: Timestamp,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpexPriceForecast {
    starts_at: Timestamp,
    total: f64,
}

pub struct EpexPredictionClient {
    client: reqwest::Client,
}

impl EpexPredictionClient {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub async fn fetch_electricity_prices(&self) -> anyhow::Result<ElectricityPrices> {
        let response: EpexPriceForecasts = self
            .client
            .get(EPEX_PREDICTOR_URL)
            .send()
            .await
            .context("Error making request")?
            .error_for_status()
            .context("Error response")?
            .json()
            .await
            .context("Invalid resonse")?;

        let known_until = &response.known_until;
        let prices = response
            .prices
            .into_iter()
            .map(|p| {
                let start = p.starts_at.to_zoned(TimeZone::system());
                ElectricityPrice {
                    is_forecast: p.starts_at >= *known_until,
                    end: start.clone() + 15.minutes(),
                    start,
                    price_per_kwh: p.total,
                }
            })
            .collect();

        Ok(ElectricityPrices {
            updated_at: Zoned::now(),
            prices,
        })
    }
}
