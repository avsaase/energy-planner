use std::{path::PathBuf, sync::Arc};

use derive_more::Deref;
use itertools::Itertools;
use jiff::{RoundMode, ToSpan, Unit, Zoned, ZonedRound};
use tokio::sync::{Notify, RwLock};
use tracing::{debug, info, level_filters::LevelFilter};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    consumption_forecast::{PowerReading, forecast, train},
    epex_prediction_client::EpexPredictionClient,
    home_assistant::{
        addon::{AddonOptions, running_as_addon},
        client::HaClient,
    },
    types::{
        ConsumptionForecast, ElectricityPrice, ElectricityPriceParameters, ElectricityPrices,
        InputData, InputInterval, Planning, SolarForecast, SolarForecasts,
    },
};

pub mod consumption_forecast;
pub mod epex_prediction_client;
pub mod home_assistant;
pub mod optimizer;
pub mod plot;
pub mod server;
pub mod types;

pub const PLANNING_INTERVAL_MINUTES: i64 = 15;

#[derive(Clone, Deref)]
pub struct AppState {
    #[deref]
    pub state: Arc<RwLock<InnerState>>,
    pub start_plan: Arc<Notify>,
}

#[derive(Default)]
pub struct InnerState {
    pub current_plan: Option<Planning>,
    pub electricty_prices: Option<ElectricityPrices>,
    pub solar_forecast: Option<SolarForecasts>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(InnerState::default())),
            start_plan: Arc::new(Notify::new()),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn init_tracing() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().pretty())
        .with(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(LevelFilter::INFO.into()),
        )
        .init();
}

pub fn data_dir() -> PathBuf {
    if running_as_addon() {
        PathBuf::from("/data")
    } else {
        PathBuf::from("./data")
    }
}

pub fn planning_path() -> PathBuf {
    data_dir().join("planning.json")
}

pub fn interval_iter(start: Zoned, until: Zoned) -> impl Iterator<Item = (Zoned, Zoned)> {
    let end_of_first_interval = start
        .round(
            ZonedRound::new()
                .smallest(Unit::Minute)
                .increment(PLANNING_INTERVAL_MINUTES)
                .mode(RoundMode::Ceil),
        )
        .expect("Rounding up works");

    debug!(
        "Interval start: {}, end of first interval: {}, until: {}",
        start, end_of_first_interval, until
    );

    std::iter::successors(
        Some((start, end_of_first_interval)),
        move |(_, last_end)| {
            let start = last_end;
            let end = start + PLANNING_INTERVAL_MINUTES.minutes();
            if end > until {
                None
            } else {
                Some((start.clone(), end))
            }
        },
    )
}

#[tracing::instrument(skip(ha_client, epex_client, addon_options))]
pub async fn prepare_optimizer_input(
    now: Zoned,
    ha_client: &HaClient,
    epex_client: &EpexPredictionClient,
    addon_options: &AddonOptions,
) -> anyhow::Result<InputData> {
    let solar_forecasts = ha_client
        .get_solar_forecast(&addon_options.solar_forecast_entities)
        .await?;

    let electricty_prices = epex_client.fetch_electricity_prices().await?;

    let consumption_forecasts = build_consumption_forecast(
        ha_client,
        &addon_options.current_gross_consumption_power_entity,
        &now,
    )
    .await?;

    let forecast_end = &now + 3.days();

    let intervals = interval_iter(now, forecast_end)
        .filter_map(|(start, end)| {
            let consumption = lookup_consumption_forecast(&start, &end, &consumption_forecasts)?;

            let solar_forecast = lookup_solar_forecast(&start, &end, &solar_forecasts.forecasts)?;

            let (
                electricity_price_eur_per_kwh_take,
                electricity_price_eur_per_kwh_feed,
                electricity_price_is_forecast,
            ) = lookup_electricity_price(
                &start,
                &end,
                &electricty_prices.prices,
                addon_options.electricity_price_parameters,
            )?;

            Some(InputInterval {
                start,
                end,
                base_load_forecast_w: consumption,
                solar_forecast_w: solar_forecast,
                electricity_price_eur_per_kwh_take,
                electricity_price_eur_per_kwh_feed,
                electricity_price_is_forecast,
            })
        })
        .collect_vec();

    let battery_current_soc = 0.5; // TODO: get from HA

    let input_data = InputData {
        battery_parameters: addon_options.battery_parameters.clone(),
        intervals,
        electricity_price_parameters: addon_options.electricity_price_parameters,
        battery_current_soc_percent: battery_current_soc,
    };

    Ok(input_data)
}

async fn build_consumption_forecast(
    ha_client: &HaClient,
    sensor: &str,
    now: &Zoned,
) -> anyhow::Result<Vec<ConsumptionForecast>> {
    let mut websocket = ha_client.connect_websocket().await?;
    let history_start = now.clone() - 60.days();
    info!("Fetching consumption statistics for {sensor}");
    let statistics = websocket
        .get_statistics(sensor, history_start, now.clone(), "5minute")
        .await?;
    let entries = statistics.0.get(sensor).map(Vec::as_slice).unwrap_or(&[]);
    info!("Received {} 5-minute entries", entries.len());
    let readings: Vec<PowerReading> = entries
        .chunks(3)
        .filter(|chunk| chunk.len() == 3)
        .map(|chunk| PowerReading {
            slot_start: chunk[0].start.clone(),
            power_w: chunk.iter().map(|e| e.mean).sum::<f64>() / 3.0,
        })
        .collect();
    let model = train(&readings)?;
    info!("Consumption model trained on {} readings", readings.len());
    let slots = forecast(&model, now, &readings)?;
    info!("Consumption forecast produced {} slots", slots.len());
    Ok(slots
        .into_iter()
        .map(|(start, forecast_w)| {
            let end = start.clone() + PLANNING_INTERVAL_MINUTES.minutes();
            ConsumptionForecast {
                start,
                end,
                forecast_w,
            }
        })
        .collect())
}

fn lookup_consumption_forecast(
    start: &Zoned,
    end: &Zoned,
    forecasts: &[ConsumptionForecast],
) -> Option<f64> {
    forecasts
        .iter()
        .find(|f| &f.start <= start && &f.end >= end)
        .map(|f| f.forecast_w)
}

fn lookup_solar_forecast(
    start: &Zoned,
    end: &Zoned,
    solar_forecasts: &[SolarForecast],
) -> Option<f64> {
    solar_forecasts
        .iter()
        .find(|forecast| &forecast.start <= start && &forecast.end >= end)
        .map(|forecast| forecast.forecast_w)
}

fn lookup_electricity_price(
    start: &Zoned,
    end: &Zoned,
    electricity_prices: &[ElectricityPrice],
    parameters: ElectricityPriceParameters,
) -> Option<(f64, f64, bool)> {
    electricity_prices
        .iter()
        .find(|price| &price.start <= start && &price.end >= end)
        .map(|price| {
            let effective_import_price =
                calculate_effective_import_price_per_kwh(price.price_per_kwh, parameters);
            let effective_export_price =
                calculate_effective_export_price_per_kwh(price.price_per_kwh, parameters);
            (
                effective_import_price,
                effective_export_price,
                price.is_forecast,
            )
        })
}

fn calculate_effective_import_price_per_kwh(
    base_price: f64,
    parameters: ElectricityPriceParameters,
) -> f64 {
    (base_price + parameters.energy_tax_import_eur_per_kwh) * (1.0 + parameters.vat_import)
        + parameters.supplier_cost_import_eur_per_kwh
}

fn calculate_effective_export_price_per_kwh(
    base_price: f64,
    parameters: ElectricityPriceParameters,
) -> f64 {
    (base_price - parameters.energy_tax_export_eur_per_kwh) * (1.0 + parameters.vat_export)
        - parameters.supplier_cost_export_eur_per_kwh
}
