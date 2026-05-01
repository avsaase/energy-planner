mod inference;
mod training;
mod utils;

pub use inference::{FORECAST_SLOTS, forecast};
pub use training::train;

use jiff::Zoned;
use lightgbm3::{Booster, ImportanceType};

/// A single 15-minute interval power reading.
pub struct PowerReading {
    pub slot_start: Zoned,
    pub power_w: f64,
}

/// Trained LightGBM consumption forecasting model.
pub struct ForecastModel {
    pub(crate) booster: Booster,
}

impl ForecastModel {
    /// Gain-based importance for each feature, in the fixed feature order:
    /// dow, hour, slot, is_weekend, cumulative_today, last_15min,
    /// last_hour, max_today, yesterday, last_week, avg_7d.
    pub fn feature_importance(&self) -> anyhow::Result<Vec<f64>> {
        Ok(self.booster.feature_importance(ImportanceType::Gain)?)
    }
}
