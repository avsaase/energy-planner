//! Refreshes the slowly changing planning inputs every 15 minutes: electricity prices, the
//! solar forecast and the long-range house load forecast.

use std::{sync::Arc, time::Duration, time::Instant};

use jiff::{RoundMode, ToSpan, Unit, Zoned, ZonedRound};
use tracing::{error, info};

use crate::{
    AppState,
    epex_prediction_client::EpexPredictionClient,
    forecaster::Forecaster,
    forecasts::{StatisticsCache, long_range_forecast},
    home_assistant::{addon::AddonOptions, client::HaClient},
};

/// Slow data refreshes this long after each 15-minute boundary, giving HA time to compile the
/// last statistics period.
const REFRESH_DELAY_SECS: i64 = 30;

/// Refreshes the slowly changing inputs every 15 minutes and then triggers a replan.
pub async fn refresh_loop(
    ha_client: HaClient,
    options: Arc<AddonOptions>,
    forecaster: Arc<dyn Forecaster>,
    app_state: AppState,
) {
    let epex_client = EpexPredictionClient::new();
    let mut statistics = StatisticsCache::default();

    loop {
        let prices_stale = app_state
            .read()
            .await
            .electricty_prices
            .as_ref()
            .is_none_or(|p| {
                jiff::Timestamp::now()
                    .duration_since(p.updated_at.timestamp())
                    .as_secs()
                    > 3600
            });
        if prices_stale {
            match epex_client.fetch_electricity_prices().await {
                Ok(prices) => app_state.write().await.electricty_prices = Some(prices),
                Err(e) => error!(error = %e, "Failed to fetch electricity prices"),
            }
        }

        match ha_client
            .get_solar_forecast(&options.solar_forecast_entities)
            .await
        {
            Ok(solar) => app_state.write().await.solar_forecast = Some(solar),
            Err(e) => error!(error = %e, "Failed to fetch solar forecast"),
        }

        let start = Instant::now();
        match long_range_forecast(
            &ha_client,
            &options,
            &forecaster,
            &mut statistics,
            jiff::Timestamp::now(),
        )
        .await
        {
            Ok(forecast) => {
                info!(
                    ms = start.elapsed().as_millis(),
                    "Updated long-range forecast"
                );
                app_state.write().await.long_range = Some(forecast);
            }
            Err(e) => error!(error = format!("{e:#}"), "Long-range forecast failed"),
        }

        app_state.start_plan.notify_one();
        sleep_till_next_refresh().await;
    }
}

async fn sleep_till_next_refresh() {
    let now = Zoned::now();
    let next = now
        .round(
            ZonedRound::new()
                .smallest(Unit::Minute)
                .increment(15)
                .mode(RoundMode::Ceil),
        )
        .expect("Rounding up to 15 minutes works")
        + REFRESH_DELAY_SECS.seconds();
    let mut duration: Duration = now
        .duration_until(&next)
        .try_into()
        .unwrap_or(Duration::ZERO);
    // Just after a boundary, ceil rounds to that same boundary: wait for the next one.
    if duration > Duration::from_secs(15 * 60) {
        duration -= Duration::from_secs(15 * 60);
    }
    duration += Duration::from_micros(1);

    info!(
        duration_secs = %duration.as_secs(),
        "Sleeping until next refresh"
    );
    tokio::time::sleep(duration).await;
}
