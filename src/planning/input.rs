//! Building the optimizer input for one replan from cached and live data.

use anyhow::Context;
use itertools::Itertools;
use jiff::{SignedDuration, Timestamp, Zoned, tz::TimeZone};
use tracing::{info, warn};

use crate::{
    AppState, data_dir,
    home_assistant::{addon::AddonOptions, client::HaClient},
    live::{SHORT_HISTORY, SharedLive, floor_to_minute},
    nowcast::{
        HouseLoad, Nowcast, SolarCorrection, house_load_change_spread, interval_mean,
        net_load_spread, planning_intervals,
    },
    types::{
        ElectricityPrice, ElectricityPriceParameters, InputData, InputInterval, NetLoadScenarios,
    },
};

/// Live data whose newest P1 value is older than this is not used for the nowcast.
const LIVE_DATA_MAX_AGE: SignedDuration = SignedDuration::from_mins(2);

/// Inputs for one replan, plus what is needed to log it.
pub struct PreparedInput {
    pub input: InputData,
    pub nowcast: Option<Nowcast>,
}

/// Builds the optimizer input from the cached long-range, solar and price data and the live
/// measurements.
#[tracing::instrument(skip_all)]
pub async fn prepare_optimizer_input(
    now: Zoned,
    ha_client: &HaClient,
    addon_options: &AddonOptions,
    live: &SharedLive,
    app_state: &AppState,
) -> anyhow::Result<PreparedInput> {
    let (long_range, solar_forecasts, electricity_prices) = {
        let state = app_state.read().await;
        (
            state
                .long_range
                .clone()
                .context("No long-range forecast yet")?,
            state
                .solar_forecast
                .clone()
                .context("No solar forecast yet")?,
            state
                .electricty_prices
                .clone()
                .context("No electricity prices yet")?,
        )
    };
    let now_ts = now.timestamp();
    let tz = TimeZone::system();

    // Last 24 h per minute: the current value for the nowcast, the PV history for the solar
    // correction and the house load history for the scenario spread.
    let (history, live_house_load, fresh) = {
        let live = live.lock().unwrap();
        (
            live.minute_history(now_ts, SHORT_HISTORY.as_mins() as usize),
            live.house_load_w(now_ts),
            live.is_fresh(now_ts, LIVE_DATA_MAX_AGE),
        )
    };
    // Stale data (missed updates not filled in yet) would extend old values over the gap.
    let current = fresh
        .then(|| {
            history
                .house_load
                .last()
                .copied()
                .filter(|v| v.is_finite())
                .or(live_house_load)
        })
        .flatten();
    if current.is_none() {
        warn!(
            fresh,
            "No current house load measurement, using the long-range forecast only"
        );
    }
    let nowcast = current.map(|w| Nowcast {
        start: floor_to_minute(now_ts),
        current_house_load_w: w,
    });
    let house_load = HouseLoad {
        nowcast,
        long: &long_range,
    };

    let solar_slots: Vec<(Timestamp, Timestamp, f64)> = solar_forecasts
        .forecasts
        .iter()
        .map(|f| (f.start.timestamp(), f.end.timestamp(), f.forecast_w))
        .collect();
    let solar_at = |t: Timestamp| {
        solar_slots
            .iter()
            .find(|(start, end, _)| *start <= t && t < *end)
            .map(|(_, _, w)| *w)
    };
    let solar_correction = {
        let n = history.solar.len();
        let recent = n.saturating_sub(30);
        let forecast: Vec<f64> = (recent..n)
            .map(|i| {
                solar_at(history.start + SignedDuration::from_mins(i as i64)).unwrap_or(f64::NAN)
            })
            .collect();
        SolarCorrection::estimate(&history.solar[recent..], &forecast)
    };

    // Stop at the first interval with missing data so the SOC chain stays contiguous.
    let intervals = planning_intervals(now_ts, now_ts + SignedDuration::from_hours(72))
        .into_iter()
        .map_while(|(start, end)| {
            let house_load = interval_mean(start, end, |t| house_load.mean_at(t))?;
            let solar = interval_mean(start, end, |t| {
                let hours_ahead = t.duration_since(now_ts).as_secs_f64() / 3600.0;
                solar_at(t).map(|w| solar_correction.apply(w, hours_ahead))
            })?;
            let start = start.to_zoned(tz.clone());
            let end = end.to_zoned(tz.clone());
            let (take, feed, is_forecast) = lookup_electricity_price(
                &start,
                &end,
                &electricity_prices.prices,
                addon_options.electricity_price_parameters,
            )?;
            Some(InputInterval {
                start,
                end,
                house_load_forecast_w: house_load,
                solar_forecast_w: solar,
                electricity_price_eur_per_kwh_take: take,
                electricity_price_eur_per_kwh_feed: feed,
                electricity_price_is_forecast: is_forecast,
            })
        })
        .collect_vec();
    let first = intervals.first().context("No complete intervals to plan")?;

    let replan = &addon_options.replanning;
    let first_minutes = (&first.end - &first.start)
        .total(jiff::Unit::Minute)
        .map_or(5, |m| m.ceil() as usize);
    let (below_w, above_w) = net_load_spread(
        house_load_change_spread(&history.house_load, first_minutes, replan.scenario_quantile),
        replan.scenario_quantile,
        &solar_correction,
        replan.scenario_min_spread_w,
    );

    let live_soc = live.lock().unwrap().soc(now_ts);
    let battery_current_soc = match live_soc {
        Some(soc) => soc,
        None => {
            ha_client
                .get_battery_soc(&addon_options.battery_soc_entity)
                .await?
        }
    };

    info!(
        solar_ratio = solar_correction.ratio,
        below_w, above_w, "Prepared optimizer input"
    );

    Ok(PreparedInput {
        input: InputData {
            battery_parameters: addon_options.battery_parameters.clone(),
            intervals,
            electricity_price_parameters: addon_options.electricity_price_parameters,
            battery_current_soc_percent: battery_current_soc,
            solver_parameters: addon_options.solver_parameters.clone(),
            scenarios: NetLoadScenarios { below_w, above_w },
        },
        nowcast,
    })
}

/// Appends one line per replan to `forecast_log.jsonl`: what was forecast for the first interval
/// and what was measured at that moment, for validating and tuning the forecasts.
pub fn append_forecast_log(
    prepared: &PreparedInput,
    measured_house_load_w: Option<f64>,
    measured_net_load_w: Option<f64>,
) -> anyhow::Result<()> {
    use std::io::Write;

    const MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;
    let path = data_dir().join("forecast_log.jsonl");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        std::fs::rename(&path, path.with_extension("jsonl.1"))?;
    }
    let first = prepared.input.intervals.first();
    let line = serde_json::json!({
        "at": Timestamp::now(),
        "measured_house_load_w": measured_house_load_w,
        "measured_net_load_w": measured_net_load_w,
        "nowcast_house_load_w": prepared.nowcast.map(|n| n.current_house_load_w),
        "first_interval": first.map(|i| serde_json::json!({
            "start": i.start.timestamp(),
            "end": i.end.timestamp(),
            "house_load_w": i.house_load_forecast_w,
            "solar_w": i.solar_forecast_w,
        })),
        "scenarios": prepared.input.scenarios,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Effective (import, export) price for `[start, end)` and whether any part is a forecast. An
/// interval spanning several price periods gets their time-weighted average; `None` if part of
/// it has no price.
fn lookup_electricity_price(
    start: &Zoned,
    end: &Zoned,
    electricity_prices: &[ElectricityPrice],
    parameters: ElectricityPriceParameters,
) -> Option<(f64, f64, bool)> {
    let (start, end) = (start.timestamp(), end.timestamp());
    let total = end.duration_since(start).as_secs_f64();
    let (mut weighted, mut covered, mut is_forecast) = (0.0, 0.0, false);
    for price in electricity_prices {
        let overlap_start = price.start.timestamp().max(start);
        let overlap_end = price.end.timestamp().min(end);
        if overlap_end > overlap_start {
            let secs = overlap_end.duration_since(overlap_start).as_secs_f64();
            weighted += price.price_per_kwh * secs;
            covered += secs;
            is_forecast |= price.is_forecast;
        }
    }
    if total <= 0.0 || (covered - total).abs() > 1e-3 {
        return None;
    }
    // The effective prices are affine in the base price, so averaging the base price is exact.
    let base_price = weighted / covered;
    Some((
        calculate_effective_import_price_per_kwh(base_price, parameters),
        calculate_effective_export_price_per_kwh(base_price, parameters),
        is_forecast,
    ))
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

#[cfg(test)]
mod tests {
    use jiff::{ToSpan, civil::date, tz::TimeZone};

    use super::*;

    fn at(h: i8, m: i8, s: i8) -> Zoned {
        date(2026, 11, 7)
            .at(h, m, s, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
    }

    fn prices() -> Vec<ElectricityPrice> {
        [(at(15, 30, 0), 0.10, false), (at(15, 45, 0), 0.40, true)]
            .into_iter()
            .map(|(start, price_per_kwh, is_forecast)| ElectricityPrice {
                end: &start + 15.minutes(),
                start,
                price_per_kwh,
                is_forecast,
            })
            .collect()
    }

    /// No taxes or costs: effective price = base price.
    fn plain() -> ElectricityPriceParameters {
        ElectricityPriceParameters {
            supplier_cost_import_eur_per_kwh: 0.0,
            supplier_cost_export_eur_per_kwh: 0.0,
            energy_tax_import_eur_per_kwh: 0.0,
            energy_tax_export_eur_per_kwh: 0.0,
            vat_import: 0.0,
            vat_export: 0.0,
        }
    }

    #[test]
    fn price_within_one_period() {
        let p = lookup_electricity_price(&at(15, 31, 0), &at(15, 36, 0), &prices(), plain());
        assert_eq!(p, Some((0.10, 0.10, false)));
    }

    #[test]
    fn price_spanning_two_periods_is_time_weighted() {
        // The first interval of a replan at 15:44:39 runs until 15:50: 21 s at 0.10, 300 s at 0.40.
        let (take, feed, is_forecast) =
            lookup_electricity_price(&at(15, 44, 39), &at(15, 50, 0), &prices(), plain()).unwrap();
        let expected = (0.10 * 21.0 + 0.40 * 300.0) / 321.0;
        assert!((take - expected).abs() < 1e-12 && (feed - expected).abs() < 1e-12);
        assert!(is_forecast);
    }

    #[test]
    fn price_missing_for_part_of_interval() {
        assert_eq!(
            lookup_electricity_price(&at(15, 55, 0), &at(16, 5, 0), &prices(), plain()),
            None
        );
    }
}
