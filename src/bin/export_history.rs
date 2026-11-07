//! Exports the history the forecasters see to CSV, for `forecaster/backtest.py`.
//!
//! - `data/history_15min.csv`: house load (grid + PV + battery) per 15 minutes (long-range
//!   forecast), from HA statistics.
//! - `data/history_1min.csv`: house load and PV per minute (short-range tier), from raw HA
//!   history (kept 10 days by default).
//!
//! Run with `HA_URL`/`HA_TOKEN` set and the add-on options in `data/options.json`.

use std::{fmt::Write as _, fs, path::Path};

use anyhow::Context;
use energy_planner::{
    data_dir,
    forecasts::{LONG_STEP, StatisticsCache, floor_to_slot},
    home_assistant::{addon::AddonOptions, client::HaClient},
    init_tracing,
    live::{LiveData, floor_to_minute},
    series::StepSeries,
};
use jiff::{SignedDuration, Timestamp};
use tracing::info;

const LONG_DAYS: i64 = 60;
const SHORT_DAYS: i64 = 10;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let options = AddonOptions::load()?;
    let ha = HaClient::new()?;
    let now = Timestamp::now();
    fs::create_dir_all(data_dir())?;

    // 15-minute history from statistics.
    let end = floor_to_slot(now);
    let start = end - SignedDuration::from_hours(24 * LONG_DAYS);
    let slots = (end.duration_since(start).as_secs() / LONG_STEP.as_secs()) as usize;
    let mut cache = StatisticsCache::default();
    cache
        .refresh(&ha, &options.sensors.meter_entities(), start, now)
        .await?;

    let columns = vec![(
        "house_load_w".to_string(),
        cache.house_load_slots(&options.sensors, start, slots),
    )];
    write_csv(
        &data_dir().join("history_15min.csv"),
        start,
        LONG_STEP,
        &columns,
    )?;

    // 1-minute history from raw states.
    let live = LiveData::connect(&ha, &options).await;
    let end = floor_to_minute(now);
    let start = end - SignedDuration::from_hours(24 * SHORT_DAYS);
    let minutes = (SHORT_DAYS * 24 * 60) as usize;
    let entities = live.entity_ids();
    let refs: Vec<&str> = entities.iter().map(String::as_str).collect();
    let mut ws = ha.connect_websocket().await?;
    let history = ws.get_history(&refs, start, now).await?;
    let series = |entity: &str, factor: f64| {
        let mut s = StepSeries::new(SignedDuration::from_hours(24 * 7));
        for &(at, v) in history.get(entity).into_iter().flatten() {
            s.push(at, v * factor);
        }
        s.bucket_means(start, SignedDuration::from_mins(1), minutes, end)
    };
    let grid = series(&live.grid.entity, live.grid.factor);
    let solar = series(&live.solar.entity, live.solar.factor);
    let battery = series(&live.battery.entity, live.battery.factor);
    let house_load: Vec<f64> = (0..minutes)
        .map(|i| grid[i] + solar[i] + battery[i])
        .collect();

    write_csv(
        &data_dir().join("history_1min.csv"),
        start,
        SignedDuration::from_mins(1),
        &[
            ("house_load_w".to_string(), house_load),
            ("solar_w".to_string(), solar),
        ],
    )?;
    Ok(())
}

fn write_csv(
    path: &Path,
    start: Timestamp,
    step: SignedDuration,
    columns: &[(String, Vec<f64>)],
) -> anyhow::Result<()> {
    let rows = columns.first().map_or(0, |(_, v)| v.len());
    let mut out = String::from("timestamp");
    for (name, _) in columns {
        write!(out, ",{name}")?;
    }
    out.push('\n');
    for i in 0..rows {
        write!(out, "{}", start + step * i as i32)?;
        for (_, values) in columns {
            match values[i] {
                v if v.is_finite() => write!(out, ",{v:.1}")?,
                _ => out.push(','),
            }
        }
        out.push('\n');
    }
    fs::write(path, out).with_context(|| format!("Failed to write {}", path.display()))?;
    info!(path = %path.display(), rows, "Wrote history");
    Ok(())
}
