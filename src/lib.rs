use std::path::PathBuf;

use anyhow::Context;
use itertools::Itertools;
use jiff::{RoundMode, ToSpan, Unit, Zoned, ZonedRound};
use tracing::debug;

use crate::{
    home_assistant::{
        addon::{AddonOptions, ConsumptionProfileEntry, running_as_addon},
        client::HaClient,
    },
    types::{
        ElectricityPrice, ElectricityPriceParameters, InputData, InputInterval, SolarForecast,
    },
};

pub mod home_assistant;
pub mod optimizer;
pub mod plot;
pub mod server;
pub mod types;

pub const PLANNING_INTERVAL_MINUTES: i64 = 15;

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

#[tracing::instrument(skip(ha_client, addon_options))]
pub async fn prepare_optimizer_input(
    now: Zoned,
    ha_client: &HaClient,
    addon_options: &AddonOptions,
) -> anyhow::Result<InputData> {
    let solar_forecasts = ha_client
        .get_solar_forecast(&addon_options.solar_forecast_entities)
        .await?;
    let solar_forecast_end = solar_forecasts
        .last()
        .map(|forecast| &forecast.end)
        .context("No solar forecasts available")?;
    debug!("Solar forecast end: {}", solar_forecast_end);

    let electricty_prices = ha_client
        .get_electricity_prices(&addon_options.electricity_price_entity)
        .await?;
    let electricity_price_end = electricty_prices
        .last()
        .map(|price| &price.end)
        .context("No electricity prices available")?;
    debug!("Electricity price end: {}", electricity_price_end);

    let data_end = solar_forecast_end.min(electricity_price_end);

    let intervals = interval_iter(now, data_end.clone())
        .filter_map(|(start, end)| {
            let consumption = lookup_consumption(&start, &end, &addon_options.consumption_profile)?;

            let solar_forecast = lookup_solar_forecast(&start, &end, &solar_forecasts)?;

            let (electricity_price_eur_per_kwh_take, electricity_price_eur_per_kwh_feed) =
                lookup_electricity_price(
                    &start,
                    &end,
                    &electricty_prices,
                    addon_options.electricity_price_parameters,
                )?;

            Some(InputInterval {
                start,
                end,
                base_load_forecast_w: consumption,
                solar_forecast_w: solar_forecast,
                electricity_price_eur_per_kwh_take,
                electricity_price_eur_per_kwh_feed,
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

fn lookup_consumption(
    start: &Zoned,
    end: &Zoned,
    consumption_profile: &[ConsumptionProfileEntry],
) -> Option<f64> {
    consumption_profile
        .iter()
        .find(|entry| {
            if entry.start <= entry.end {
                // Normal same-day range, e.g. 06:00 -> 17:00.
                // Only match when the interval itself does not cross local midnight.
                start.date() == end.date() && start.time() >= entry.start && end.time() <= entry.end
            } else {
                // Overnight range, e.g. 17:00 -> 00:00 or 22:00 -> 06:00.
                start.time() >= entry.start || end.time() <= entry.end
            }
        })
        .map(|entry| entry.load_w)
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
) -> Option<(f64, f64)> {
    let raw_electricity_price = electricity_prices
        .iter()
        .find(|price| &price.start <= start && &price.end >= end)
        .map(|price| price.price_per_kwh);

    raw_electricity_price.map(|price| {
        let effective_import_price = calculate_effective_import_price_per_kwh(price, parameters);
        let effective_export_price = calculate_effective_export_price_per_kwh(price, parameters);
        (effective_import_price, effective_export_price)
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
    (base_price - parameters.energy_tax_export_eur_per_kwh) * (1.0 - parameters.vat_export)
        - parameters.supplier_cost_export_eur_per_kwh
}
