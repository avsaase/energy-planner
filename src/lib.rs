use std::{path::PathBuf, sync::Arc};

use derive_more::Deref;
use tokio::sync::{Notify, RwLock, watch};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    forecasts::LongRangeForecast,
    home_assistant::addon::running_as_addon,
    types::{ElectricityPrices, Planning, SolarForecasts},
};

pub mod battery_control;
pub mod epex_prediction_client;
pub mod forecaster;
pub mod forecasts;
pub mod home_assistant;
pub mod live;
pub mod nowcast;
pub mod optimizer;
pub mod planning;
pub mod plot;
pub mod refresh;
pub mod run_log;
pub mod series;
pub mod server;
pub mod types;

pub const PLANNING_INTERVAL_MINUTES: i64 = 15;

#[derive(Clone, Deref)]
pub struct AppState {
    #[deref]
    pub state: Arc<RwLock<InnerState>>,
    pub start_plan: Arc<Notify>,
    /// Bumped whenever `current_plan` changes; the web page follows it.
    pub plan_version: Arc<watch::Sender<u64>>,
}

#[derive(Default)]
pub struct InnerState {
    pub current_plan: Option<Planning>,
    pub electricty_prices: Option<ElectricityPrices>,
    pub solar_forecast: Option<SolarForecasts>,
    pub long_range: Option<LongRangeForecast>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(InnerState::default())),
            start_plan: Arc::new(Notify::new()),
            plan_version: Arc::new(watch::Sender::new(0)),
        }
    }

    /// Stores a new plan and tells the web page about it.
    pub async fn publish_plan(&self, planning: Planning) {
        self.write().await.current_plan = Some(planning);
        self.plan_version.send_modify(|version| *version += 1);
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn init_tracing() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(LevelFilter::INFO.into())
                // ONNX Runtime logs every graph optimization step at INFO.
                .add_directive("ort=warn".parse().expect("valid directive")),
        )
        .init();
}

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
