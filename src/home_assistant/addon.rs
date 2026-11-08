use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;

use crate::{
    data_dir,
    types::{BatteryParameters, ElectricityPriceParameters, SolverParameters},
};

#[derive(Debug, Clone, Deserialize)]
pub struct AddonOptions {
    pub solar_forecast_entities: Vec<String>,
    pub battery_parameters: BatteryParameters,
    pub electricity_price_parameters: ElectricityPriceParameters,
    pub battery_soc_entity: String,
    pub solver_parameters: SolverParameters,
    pub sensors: SensorOptions,
    #[serde(default)]
    pub forecaster: ForecasterOptions,
    #[serde(default)]
    pub replanning: ReplanOptions,
    /// Days of complete replan records (inputs and plans) kept for debugging; 0 disables.
    #[serde(default = "default_run_log_days")]
    pub run_log_days: u32,
}

fn default_run_log_days() -> u32 {
    7
}

/// Live power sensors. Values are normalised to the sign conventions documented in `live.rs`
/// (grid: + import, battery: + discharge) using the `*_invert` flags.
#[derive(Debug, Clone, Deserialize)]
pub struct SensorOptions {
    pub grid_power_entity: String,
    #[serde(default)]
    pub grid_power_invert: bool,
    pub solar_power_entity: String,
    pub battery_power_entity: String,
    #[serde(default)]
    pub battery_power_invert: bool,
}

impl SensorOptions {
    /// The grid, PV and battery power sensors that together define house load. They need
    /// long-term statistics (`state_class: measurement`) for the long-range history.
    pub fn meter_entities(&self) -> [&str; 3] {
        [
            &self.grid_power_entity,
            &self.solar_power_entity,
            &self.battery_power_entity,
        ]
    }
}

/// The long-range forecast model.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ForecasterOptions {
    /// ONNX model from `forecaster/export_onnx.py`. Default: `$FORECAST_MODEL_PATH` (set in the
    /// add-on image, which has the model built in), else `./data/models/chronos2.onnx`.
    pub model_path: Option<PathBuf>,
}

impl ForecasterOptions {
    pub fn model_path(&self) -> PathBuf {
        self.model_path
            .clone()
            .or_else(|| std::env::var_os("FORECAST_MODEL_PATH").map(PathBuf::from))
            .unwrap_or_else(|| data_dir().join("models").join("chronos2.onnx"))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReplanOptions {
    /// Replan when net load `N` moves this far from its value at the last replan…
    pub net_load_change_threshold_w: f64,
    /// …for at least this long.
    pub net_load_change_sustain_seconds: u64,
    /// Never replan more often than this.
    pub min_interval_seconds: u64,
    /// Replan when the SOC deviates this much (fraction) from the planned path.
    pub soc_deviation: f64,
    /// Quantile used for the high net-load scenario (low uses `1 - q`).
    pub scenario_quantile: f64,
    /// Minimum distance of the high/low scenarios from the expected net load.
    pub scenario_min_spread_w: f64,
}

impl Default for ReplanOptions {
    fn default() -> Self {
        Self {
            net_load_change_threshold_w: 300.0,
            net_load_change_sustain_seconds: 15,
            min_interval_seconds: 20,
            soc_deviation: 0.03,
            scenario_quantile: 0.9,
            scenario_min_spread_w: 300.0,
        }
    }
}

impl AddonOptions {
    pub fn load() -> anyhow::Result<Self> {
        let options_path = std::env::var("OPTIONS_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir().join("options.json"));
        let options_file =
            std::fs::File::open(&options_path).context("Failed to load options file")?;
        let options =
            serde_json::from_reader(&options_file).context("Failed to read option file")?;
        Ok(options)
    }
}

pub fn running_as_addon() -> bool {
    std::env::var("SUPERVISOR_TOKEN").is_ok()
}
