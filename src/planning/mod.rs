//! The planning loop: prepares the input, solves the plan, stores it and applies the battery
//! intent of the current interval.

pub mod input;

use std::{
    fs::File,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use jiff::Zoned;
use tokio::task::spawn_blocking;
use tracing::{error, info, warn};

use crate::{
    AppState,
    battery_control::BatteryController,
    home_assistant::{addon::AddonOptions, client::HaClient},
    live::SharedLive,
    optimizer, planning_path,
    types::Planning,
};
use input::{append_forecast_log, prepare_optimizer_input};

/// Replan this long after the plan's first interval ends, to be safely inside the next one.
const AFTER_INTERVAL_END: Duration = Duration::from_secs(1);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// The plan saved by the previous run, if any.
pub fn load_stored_plan() -> anyhow::Result<Planning> {
    let file = File::open(planning_path()).context("Failed to read planning file")?;
    let planning = serde_json::from_reader(file).context("Failed to parse planning file")?;
    Ok(planning)
}

/// Replans whenever triggered (live changes, refreshes) and at the end of the plan's first
/// interval, so the applied battery mode always belongs to the current interval (in practice at
/// every 5-minute boundary). Never more often than `min_interval_seconds`.
pub async fn planning_loop(
    ha_client: HaClient,
    options: Arc<AddonOptions>,
    live: SharedLive,
    app_state: AppState,
) {
    let min_interval = Duration::from_secs(options.replanning.min_interval_seconds);
    let mut last_start: Option<Instant> = None;
    let mut next_replan = tokio::time::Instant::now() + RETRY_AFTER_FAILURE;
    let mut controller = BatteryController::new(ha_client.clone());

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next_replan) => {
                info!("First interval of the plan ended (or retrying), replanning");
            }
            _ = app_state.start_plan.notified() => {}
        }
        if let Some(since) = last_start.map(|t| t.elapsed())
            && since < min_interval
        {
            tokio::time::sleep(min_interval - since).await;
        }
        last_start = Some(Instant::now());
        // Unless this replan succeeds and schedules the next one, retry after a while.
        next_replan = tokio::time::Instant::now() + RETRY_AFTER_FAILURE;

        let now = Zoned::now();
        let now_ts = now.timestamp();
        // Changes are measured from the net load this plan is based on.
        let (measured_house_load, measured_net_load) = {
            let mut live = live.lock().unwrap();
            let net_load = live.net_load_w(now_ts);
            live.change_detector.reset(net_load);
            (live.house_load_w(now_ts), net_load)
        };

        let start = Instant::now();
        let prepared =
            match prepare_optimizer_input(now.clone(), &ha_client, &options, &live, &app_state)
                .await
            {
                Ok(prepared) => prepared,
                Err(e) => {
                    error!(error = format!("{e:#}"), "Error preparing planning input");
                    continue;
                }
            };
        let prepare_ms = start.elapsed().as_millis();

        if let Err(e) = append_forecast_log(&prepared, measured_house_load, measured_net_load) {
            warn!(error = %e, "Failed to write forecast log");
        }

        let start = Instant::now();
        let input = prepared.input;
        let now_cloned = now.clone();
        let planning = match spawn_blocking(move || optimizer::solve(input, now_cloned)).await {
            Ok(Ok(planning)) => planning,
            Ok(Err(e)) => {
                error!(error = format!("{e:#}"), "Error in solver");
                continue;
            }
            Err(e) => {
                error!(error = %e, "Solver panicked");
                continue;
            }
        };
        info!(
            intervals = planning.intervals.len(),
            prepare_ms,
            solve_ms = start.elapsed().as_millis(),
            intent = ?planning.intervals.first().map(|i| &i.battery_intent),
            "Completed planning"
        );

        // Write the plan to disk for persistence
        let _ = File::create(planning_path())
            .context("Failed to create planning file")
            .and_then(|file| {
                serde_json::to_writer_pretty(file, &planning)
                    .context("Failed to write planning to file")
            })
            .inspect_err(|e| error!(error = %e, "Failed to write planning to disk"));

        let intent = planning
            .intervals
            .iter()
            .find(|i| i.start <= now && now < i.end)
            .map(|i| i.battery_intent.clone());
        if let Some(first) = planning.intervals.first() {
            let until_end: Duration = Zoned::now()
                .duration_until(&first.end)
                .try_into()
                .unwrap_or(Duration::ZERO);
            next_replan = tokio::time::Instant::now() + until_end + AFTER_INTERVAL_END;
        }
        app_state.publish_plan(planning).await;

        if let Some(intent) = intent {
            controller.apply(&intent).await;
        }
    }
}
