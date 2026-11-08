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
    run_log::{RunLog, RunRecord},
    types::Planning,
};
use input::prepare_optimizer_input;

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
    let run_log = RunLog::new(options.run_log_days);

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

        let start = Instant::now();
        let input = prepared.input;
        let solve_input = input.clone();
        let now_cloned = now.clone();
        let result = spawn_blocking(move || optimizer::solve(solve_input, now_cloned))
            .await
            .map_err(|e| anyhow::anyhow!("Solver panicked: {e}"))
            .and_then(|result| result.context("Error in solver"));
        let solve_ms = start.elapsed().as_millis();

        let record = RunRecord {
            planned_at: &now,
            measured_house_load_w: measured_house_load,
            measured_net_load_w: measured_net_load,
            nowcast: prepared.nowcast,
            input: &input,
            planning: result.as_ref().ok(),
            error: result.as_ref().err().map(|e| format!("{e:#}")),
            prepare_ms,
            solve_ms,
        };
        if let Err(e) = run_log.append(&record) {
            warn!(error = %e, "Failed to write run log");
        }

        let planning = match result {
            Ok(planning) => planning,
            Err(e) => {
                error!(error = format!("{e:#}"), "Planning failed");
                continue;
            }
        };
        info!(
            intervals = planning.intervals.len(),
            prepare_ms,
            solve_ms,
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
