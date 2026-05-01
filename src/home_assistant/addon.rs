use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;

use crate::{
    data_dir,
    types::{BatteryParameters, ElectricityPriceParameters},
};

#[derive(Debug, Clone, Deserialize)]
pub struct AddonOptions {
    pub solar_forecast_entities: Vec<String>,
    pub electricity_price_entity: String,
    pub battery_parameters: BatteryParameters,
    pub electricity_price_parameters: ElectricityPriceParameters,
    pub grid_limit_w: f64,
    pub current_gross_consumption_power_entity: String,
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
