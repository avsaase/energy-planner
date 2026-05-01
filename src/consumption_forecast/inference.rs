use anyhow::Context;
use jiff::{RoundMode, ToSpan, Unit, Zoned, ZonedRound};

use super::ForecastModel;
use super::PowerReading;
use super::utils::{NUM_FEATURES, build_index, feature_row};

pub const FORECAST_SLOTS: usize = 96; // 24 h × 4 slots/h

pub fn forecast(
    model: &ForecastModel,
    now: &Zoned,
    history: &[PowerReading],
) -> anyhow::Result<Vec<(Zoned, f64)>> {
    let index = build_index(history);

    let first_slot = now
        .round(
            ZonedRound::new()
                .smallest(Unit::Minute)
                .increment(15)
                .mode(RoundMode::Trunc),
        )
        .context("failed to compute first forecast slot")?;

    let mut last_15min_w = history
        .iter()
        .filter(|r| r.slot_start < first_slot)
        .max_by_key(|r| r.slot_start.timestamp())
        .map(|r| r.power_w)
        .unwrap_or(0.0);

    let mut results = Vec::with_capacity(FORECAST_SLOTS);

    for i in 0..FORECAST_SLOTS {
        let slot_t = first_slot.clone() + (15_i64 * i as i64).minutes();

        let row = feature_row(&slot_t, last_15min_w, &index);

        let preds = model
            .booster
            .predict(&row, NUM_FEATURES as i32, true)
            .context("LightGBM prediction failed")?;

        let predicted_w = preds.first().copied().unwrap_or(0.0).max(0.0);
        last_15min_w = predicted_w;

        results.push((slot_t, predicted_w));
    }

    Ok(results)
}
