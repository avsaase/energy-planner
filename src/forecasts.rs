//! The long-range forecast of house load `L`: 15-minute steps over 3 days from ~3 weeks of HA
//! statistics, rerun on each 15-minute boundary. The model only sees house load; it picks up the
//! daily and weekly pattern from the history itself. The first half hour is corrected with the
//! live measurement in `nowcast`.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use anyhow::Context;
use jiff::{SignedDuration, Timestamp};
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use crate::{
    forecaster::{
        Forecast, ForecastRequest, Forecaster, SeasonalNaive, WithFallback, onnx::OnnxForecaster,
    },
    home_assistant::{
        addon::{AddonOptions, ForecasterOptions, SensorOptions},
        client::HaClient,
    },
    series::{aggregate_points, floor_to_minutes},
};

pub const LONG_STEP: SignedDuration = SignedDuration::from_mins(15);
const LONG_HISTORY_SLOTS: usize = 21 * 96;
const LONG_HORIZON_SLOTS: usize = 3 * 96 + 2;

/// Loads the forecast model, falling back to a seasonal-naive forecast if it is missing or
/// fails. Takes a few seconds; call from `spawn_blocking`.
pub fn load_forecaster(options: &ForecasterOptions) -> Arc<dyn Forecaster> {
    let path = options.model_path();
    let primary = match OnnxForecaster::load(&path) {
        Ok(model) => {
            if model.context_steps() != LONG_HISTORY_SLOTS {
                warn!(
                    model = model.context_steps(),
                    planner = LONG_HISTORY_SLOTS,
                    "Model history length differs from the planner's; history will be padded or cut"
                );
            }
            Some(Arc::new(model) as Arc<dyn Forecaster>)
        }
        Err(e) => {
            warn!(
                error = format!("{e:#}"),
                "Forecast model unavailable, using seasonal-naive forecasts"
            );
            None
        }
    };
    Arc::new(WithFallback {
        primary,
        fallback: Arc::new(SeasonalNaive { max_days: 7 }),
    })
}

#[derive(Debug, Clone)]
pub struct LongRangeForecast {
    /// Start of the first forecast slot.
    pub start: Timestamp,
    pub house_load: Forecast,
}

impl LongRangeForecast {
    fn slot(&self, t: Timestamp) -> Option<usize> {
        if t < self.start {
            return None;
        }
        let i = (t.duration_since(self.start).as_secs() / LONG_STEP.as_secs()) as usize;
        (i < self.house_load.horizon()).then_some(i)
    }

    pub fn house_load_at(&self, t: Timestamp) -> Option<f64> {
        self.slot(t).map(|i| self.house_load.mean[i])
    }
}

/// HA statistics kept in memory and extended incrementally.
///
/// HA purges 5-minute statistics after `purge_keep_days` (10 by default) but keeps hourly ones,
/// so slots without 5-minute data fall back to the hourly mean.
#[derive(Default)]
pub struct StatisticsCache {
    five_minute: HashMap<String, BTreeMap<Timestamp, f64>>,
    hourly: HashMap<String, BTreeMap<Timestamp, f64>>,
    fetched_until: Option<Timestamp>,
}

impl StatisticsCache {
    pub async fn refresh(
        &mut self,
        ha: &HaClient,
        entities: &[&str],
        from: Timestamp,
        now: Timestamp,
    ) -> anyhow::Result<()> {
        // Statistics are compiled a little after each period: re-fetch the last two hours.
        let start = self
            .fetched_until
            .map_or(from, |t| (t - SignedDuration::from_hours(2)).max(from));
        let mut ws = ha.connect_websocket().await?;
        for (period, cache) in [
            ("5minute", &mut self.five_minute),
            ("hour", &mut self.hourly),
        ] {
            let stats = ws.get_statistics(entities, start, now, period).await?;
            for (entity, entries) in stats.0 {
                let points = cache.entry(entity).or_default();
                for e in entries {
                    if let Some(mean) = e.mean {
                        points.insert(e.start.timestamp(), mean);
                    }
                }
            }
            for points in cache.values_mut() {
                *points = points.split_off(&from);
            }
        }
        self.fetched_until = Some(now);
        Ok(())
    }

    /// House load `L = P_grid + P_pv + P_bat` per 15-minute slot, from the meters' statistics
    /// (the mean of a sum is the sum of the means). `NaN` where a meter has no data.
    pub fn house_load_slots(
        &self,
        sensors: &SensorOptions,
        start: Timestamp,
        n: usize,
    ) -> Vec<f64> {
        let sign = |invert: bool| if invert { -1.0 } else { 1.0 };
        let grid = self.slots(&sensors.grid_power_entity, start, n);
        let solar = self.slots(&sensors.solar_power_entity, start, n);
        let battery = self.slots(&sensors.battery_power_entity, start, n);
        (0..n)
            .map(|i| {
                sign(sensors.grid_power_invert) * grid[i]
                    + solar[i]
                    + sign(sensors.battery_power_invert) * battery[i]
            })
            .collect()
    }

    /// Mean per 15-minute slot from `start`; `NaN` where neither resolution has data.
    pub fn slots(&self, entity: &str, start: Timestamp, n: usize) -> Vec<f64> {
        let points = |cache: &HashMap<String, BTreeMap<Timestamp, f64>>| {
            cache
                .get(entity)
                .into_iter()
                .flatten()
                .map(|(t, v)| (*t, *v))
                .collect::<Vec<_>>()
        };
        let mut slots = aggregate_points(points(&self.five_minute), start, LONG_STEP, n);
        let hourly = self.hourly.get(entity);
        for (i, slot) in slots.iter_mut().enumerate() {
            if slot.is_nan() {
                let t = start + LONG_STEP * i as i32;
                let hour = floor_to_minutes(t, 60);
                if let Some(v) = hourly.and_then(|h| h.get(&hour)) {
                    *slot = *v;
                }
            }
        }
        slots
    }
}

pub fn floor_to_slot(t: Timestamp) -> Timestamp {
    floor_to_minutes(t, LONG_STEP.as_mins())
}

pub async fn long_range_forecast(
    ha: &HaClient,
    options: &AddonOptions,
    forecaster: &Arc<dyn Forecaster>,
    cache: &mut StatisticsCache,
    now: Timestamp,
) -> anyhow::Result<LongRangeForecast> {
    let start = floor_to_slot(now); // first forecast slot; history ends here
    let history_start = start - LONG_STEP * LONG_HISTORY_SLOTS as i32;

    cache
        .refresh(ha, &options.sensors.meter_entities(), history_start, now)
        .await
        .context("Failed to fetch statistics")?;

    let house_load = cache.house_load_slots(&options.sensors, history_start, LONG_HISTORY_SLOTS);

    let request = ForecastRequest {
        start: history_start,
        step_minutes: LONG_STEP.as_mins(),
        target: house_load,
        horizon: LONG_HORIZON_SLOTS,
    };
    let model = forecaster.clone();
    let house_load = spawn_blocking(move || model.predict(&request))
        .await??
        .clamp_min(0.0);
    info!(
        forecaster = forecaster.name(),
        slots = house_load.horizon(),
        "Long-range house load forecast ready"
    );
    Ok(LongRangeForecast { start, house_load })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_fall_back_to_hourly_where_five_minute_data_is_purged() {
        let t0 = Timestamp::from_second(0).unwrap();
        let mins = |m: i64| t0 + SignedDuration::from_mins(m);
        let mut cache = StatisticsCache::default();
        cache.hourly.insert(
            "sensor.load".into(),
            BTreeMap::from([(mins(0), 100.0), (mins(60), 200.0)]),
        );
        // 5-minute data only exists for the second hour's first slot.
        cache.five_minute.insert(
            "sensor.load".into(),
            BTreeMap::from([(mins(60), 300.0), (mins(65), 500.0)]),
        );
        let slots = cache.slots("sensor.load", t0, 9);
        assert_eq!(&slots[..6], &[100.0, 100.0, 100.0, 100.0, 400.0, 200.0]);
        assert!(slots[8].is_nan());
    }

    #[test]
    fn house_load_is_sum_of_meters_with_sign_normalisation() {
        let t0 = Timestamp::from_second(0).unwrap();
        let mut cache = StatisticsCache::default();
        for (entity, value) in [("grid", -500.0), ("pv", 2000.0), ("bat", 800.0)] {
            cache
                .five_minute
                .insert(entity.into(), BTreeMap::from([(t0, value)]));
        }
        let sensors = SensorOptions {
            grid_power_entity: "grid".into(),
            // Sensor reports export as positive.
            grid_power_invert: true,
            solar_power_entity: "pv".into(),
            battery_power_entity: "bat".into(),
            // Sensor reports charging as positive.
            battery_power_invert: true,
        };
        // Import 500 W, PV 2000 W, battery charging 800 W -> house uses 1700 W.
        assert_eq!(cache.house_load_slots(&sensors, t0, 1), vec![1700.0]);
    }

    #[test]
    fn long_range_lookup_by_slot() {
        let start = Timestamp::from_second(0).unwrap();
        let f = LongRangeForecast {
            start,
            house_load: Forecast::deterministic(vec![1.0, 2.0]),
        };
        assert_eq!(
            f.house_load_at(start + SignedDuration::from_mins(20)),
            Some(2.0)
        );
        assert_eq!(f.house_load_at(start + SignedDuration::from_mins(30)), None);
    }
}
