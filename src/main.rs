use std::{
    fs::remove_file,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use energy_planner::{
    AppState,
    forecasts::load_forecaster,
    home_assistant::{addon::AddonOptions, client::HaClient},
    init_tracing,
    live::{LiveData, SharedLive, load_recent_history, run_live_updates},
    planning::{load_stored_plan, planning_loop},
    planning_path,
    refresh::refresh_loop,
    server::router,
};
use tokio::task::spawn_blocking;
use tracing::{error, info};

const LISTEN_ADDRESS: &str = "0.0.0.0:8099";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    info!("Starting energy planner");

    let options = Arc::new(AddonOptions::load()?);

    // Claim the web UI port first: if another planner is running, stop before touching anything.
    let listener = tokio::net::TcpListener::bind(LISTEN_ADDRESS)
        .await
        .with_context(|| {
            format!("Cannot listen on {LISTEN_ADDRESS}. Is another energy planner still running?")
        })?;
    info!("Loaded addon options: {:#?}", options);

    let ha_client = HaClient::new()?;
    let app_state = AppState::new();

    if let Ok(stored_planning) = load_stored_plan() {
        info!("Loaded existing plan from disk");
        app_state.publish_plan(stored_planning).await;
    } else {
        error!("Failed to read planning from disk, starting with empty plan");
        let _ = remove_file(planning_path());
    }

    info!("Loading forecast model");
    let forecaster = {
        let forecaster_options = options.forecaster.clone();
        spawn_blocking(move || load_forecaster(&forecaster_options)).await?
    };

    let live: SharedLive = Arc::new(Mutex::new(LiveData::connect(&ha_client, &options).await));
    if let Err(e) = load_recent_history(&ha_client, &live).await {
        error!(error = %e, "Failed to load live sensor history");
    }

    tokio::spawn(run_live_updates(
        ha_client.clone(),
        live.clone(),
        app_state.clone(),
        options.replanning.soc_deviation,
    ));
    tokio::spawn(refresh_loop(
        ha_client.clone(),
        options.clone(),
        forecaster,
        app_state.clone(),
    ));
    let plan_task_handle = tokio::spawn(planning_loop(ha_client, options, live, app_state.clone()));

    info!("Serving on http://localhost:8099");
    axum::serve(listener, router(app_state)).await?;

    plan_task_handle.await.expect("Task panicked");

    Ok(())
}
