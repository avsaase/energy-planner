use std::{
    fs::{File, remove_file},
    time::{Duration, Instant},
};

use anyhow::Context;
use energy_planner::{
    AppState,
    home_assistant::{addon::AddonOptions, client::HaClient},
    init_tracing, optimizer, planning_path, prepare_optimizer_input,
    server::router,
    types::Planning,
};
use jiff::{RoundMode, Unit, Zoned, ZonedRound};
use tracing::{error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    info!("Starting energy planner");

    let addon_options = AddonOptions::load()?;
    info!("Loaded addon options: {:#?}", addon_options);

    let ha_client = HaClient::new()?;

    let app_state = AppState::new();

    if let Ok(stored_planning) = read_stored_planning_file().await {
        info!("Loaded existing plan from disk");
        app_state
            .write()
            .await
            .current_plan
            .replace(stored_planning);
    } else {
        error!("Failed to read planning from disk, starting with empty plan");
        let _ = remove_file(planning_path());
    }

    info!("Starting planning loop");
    let plan_task_handle =
        tokio::task::spawn(planning_loop(ha_client, addon_options, app_state.clone()));

    info!("Triggering new plan after restart");
    app_state.start_plan.notify_one();

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8099").await?;
    info!("Serving on http://localhost:8099");
    axum::serve(listener, router(app_state)).await?;

    plan_task_handle.await.expect("Task panicked")?;

    Ok(())
}

async fn read_stored_planning_file() -> anyhow::Result<Planning> {
    let file = File::open(planning_path()).context("Failed to read planning file")?;
    let planning = serde_json::from_reader(file).context("Failed to parse planning file")?;
    Ok(planning)
}

async fn planning_loop(
    ha_client: HaClient,
    addon_options: AddonOptions,
    app_state: AppState,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            _ = sleep_till_next_planning() => {
                    info!("Woke up for next planning iteration");
                },
            _ = app_state.start_plan.notified() => {
                info!("Received request to start planning immediately");
            },
        }

        let now = Zoned::now();

        let start = Instant::now();
        let Ok(input_data) = prepare_optimizer_input(now.clone(), &ha_client, &addon_options)
            .await
            .inspect_err(|e| error!(error = %e, "Error preparing planning input"))
        else {
            continue;
        };
        let elapsed = start.elapsed();

        info!(
            "Prepared optimizer input data from {} to {} in {} ms",
            input_data
                .intervals
                .first()
                .map(|i| i.start.clone())
                .unwrap_or_default(),
            input_data
                .intervals
                .last()
                .map(|i| i.end.clone())
                .unwrap_or_default(),
            elapsed.as_millis()
        );

        let start = Instant::now();
        let Ok(planning_result) = optimizer::solve(input_data, now.clone())
            .inspect_err(|e| error!(error = %e, "Error in solver"))
        else {
            continue;
        };

        info!(
            "Completed planning for {} intervals in {} ms",
            planning_result.intervals.len(),
            start.elapsed().as_millis()
        );

        // Write the plan to disk for persistence
        let _ = File::create(planning_path())
            .context("Failed to create planning file")
            .and_then(|file| {
                serde_json::to_writer_pretty(file, &planning_result)
                    .context("Failed to write planning to file")
            })
            .inspect_err(|e| error!(error = %e, "Failed to write planning to disk"));

        // Update the in memory state
        let _ = app_state
            .write()
            .await
            .current_plan
            .replace(planning_result);
    }
}

async fn sleep_till_next_planning() {
    let now = Zoned::now();
    let next = now
        .round(
            ZonedRound::new()
                .smallest(Unit::Minute)
                .increment(15)
                .mode(RoundMode::Ceil),
        )
        .expect("Rounding up to 15 minutes works");
    let mut duration: Duration = now
        .duration_until(&next)
        .try_into()
        .expect("Duration until next timestamp is non-negative");
    duration += Duration::from_micros(1); // Add 1 us to ensure we are past the next timestamp

    info!(
        next = %next,
        duration_secs = %duration.as_secs(),
        "Sleeping until next planning iteration"
    );
    tokio::time::sleep(duration).await;
}
