use anyhow::Context;
use jiff::{Timestamp, ToSpan, Zoned, tz::TimeZone};
use serde::Deserialize;
use tracing::info;

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
        let time_zone = TimeZone::get("Europe/Amsterdam").expect("Valid time zone");
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

        // The known_until in the response is the start of the last known timestamp
        let known_until = (response.known_until + 15.minutes()).to_zoned(time_zone.clone());
        let prices: Vec<ElectricityPrice> = response
            .prices
            .into_iter()
            .map(|p| {
                let start = p.starts_at.to_zoned(time_zone.clone());
                ElectricityPrice {
                    is_forecast: start >= known_until,
                    end: start.clone() + 15.minutes(),
                    start,
                    price_per_kwh: p.total,
                }
            })
            .collect();

        info!(
            known_until = %known_until,
            forecast_until = %&prices.last().map(|p| &p.end).unwrap_or(&known_until),
            "Updated electricity prices"
        );

        Ok(ElectricityPrices {
            updated_at: Zoned::now(),
            prices,
        })
    }
}
