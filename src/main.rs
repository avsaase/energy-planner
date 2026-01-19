use std::{sync::Arc, time::Duration};

use energy_planner::{
    home_assistant::{addon::AddonOptions, client::HaClient},
    optimizer, planning_path, prepare_optimizer_input,
    server::{AppState, router},
};
use jiff::{RoundMode, Unit, Zoned, ZonedRound};
use tokio::sync::{Notify, RwLock};
use tracing::{debug, error, info, level_filters::LevelFilter};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().pretty())
        .with(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(LevelFilter::INFO.into()),
        )
        .init();

    info!("Starting energy planner");

    let addon_options = AddonOptions::load()?;
    info!("Loaded addon options: {:#?}", addon_options);

    let ha_client = HaClient::new()?;

    let app_state = AppState {
        current_plan: Arc::new(RwLock::new(None)),
        start_plan: Arc::new(Notify::new()),
    };

    // Read plan from file if it exists, so we have something to show in the UI immediately
    if let Ok(file) = std::fs::File::open(planning_path()) {
        let plan = serde_json::from_reader(file)?;
        info!("Loaded existing plan from disk");
        app_state.current_plan.write().await.replace(plan);
    }

    info!("Starting planning loop");
    let plan_task_handle =
        tokio::task::spawn(planning_loop(ha_client, addon_options, app_state.clone()));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8099").await?;
    info!("Serving on http://localhost:8099");
    axum::serve(listener, router(app_state)).await?;

    plan_task_handle.await.expect("Task panicked")?;

    Ok(())
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

        let input_data = prepare_optimizer_input(now.clone(), &ha_client, &addon_options)
            .await
            .inspect_err(|e| error!(error = %e, "Error preparing planning input"))?;

        info!(
            "Prepared optimizer input data from {} to {}",
            input_data
                .intervals
                .first()
                .map(|i| i.start.clone())
                .unwrap_or(now.clone()),
            input_data
                .intervals
                .last()
                .map(|i| i.end.clone())
                .unwrap_or(now.clone())
        );

        let planning_result = optimizer::solve(input_data, now)
            .inspect_err(|e| error!(error = %e, "Error in solver"))?;
        debug!("Planning result: {:?}", planning_result);

        // Write the plan to disk for persistence
        let file = std::fs::File::create(planning_path())?;
        serde_json::to_writer_pretty(file, &planning_result)?;

        // Update the in memory state
        let _ = app_state
            .current_plan
            .write()
            .await
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
