//! Probabilistic forecasting of power time series.
//!
//! The main implementation is Chronos-2 exported to ONNX (`onnx` module, see
//! `forecaster/export_onnx.py`). A seasonal-naive forecast acts as a fallback so the planner
//! keeps running if the model is missing or fails.

pub mod onnx;

use std::sync::Arc;

use jiff::Timestamp;
use tracing::warn;

/// One forecast call: a regularly sampled history. Missing values are `f64::NAN`.
#[derive(Debug, Clone)]
pub struct ForecastRequest {
    /// Start time of the first history step.
    pub start: Timestamp,
    pub step_minutes: i64,
    /// History of the series to forecast.
    pub target: Vec<f64>,
    pub horizon: usize,
}

/// Quantile forecast for `horizon` steps.
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    /// Ascending quantile levels.
    pub quantile_levels: Vec<f64>,
    /// `quantiles[level][step]`, non-decreasing over levels.
    pub quantiles: Vec<Vec<f64>>,
    /// Expected value per step.
    pub mean: Vec<f64>,
}

impl Forecast {
    /// A forecast without uncertainty.
    pub fn deterministic(values: Vec<f64>) -> Self {
        Self {
            quantile_levels: vec![0.5],
            quantiles: vec![values.clone()],
            mean: values,
        }
    }

    pub fn horizon(&self) -> usize {
        self.mean.len()
    }

    /// Quantile `level` at `step`, interpolated linearly between the available levels and clamped
    /// to the outermost ones.
    pub fn quantile_at(&self, level: f64, step: usize) -> f64 {
        let levels = &self.quantile_levels;
        let value = |i: usize| self.quantiles[i][step];
        if level <= levels[0] {
            return value(0);
        }
        if level >= levels[levels.len() - 1] {
            return value(levels.len() - 1);
        }
        let upper = levels.partition_point(|&l| l < level);
        let lower = upper - 1;
        let w = (level - levels[lower]) / (levels[upper] - levels[lower]);
        value(lower) * (1.0 - w) + value(upper) * w
    }

    /// Clamp all values to be at least `min`.
    pub fn clamp_min(mut self, min: f64) -> Self {
        for row in &mut self.quantiles {
            row.iter_mut().for_each(|v| *v = v.max(min));
        }
        self.mean.iter_mut().for_each(|v| *v = v.max(min));
        self
    }
}

pub trait Forecaster: Send + Sync {
    fn name(&self) -> &str;
    fn predict(&self, request: &ForecastRequest) -> anyhow::Result<Forecast>;
}

/// Default quantile levels used by the statistical forecasters.
const FALLBACK_LEVELS: [f64; 5] = [0.1, 0.25, 0.5, 0.75, 0.9];

/// Same time of day over the previous days (up to `max_days`). Fallback for 15-minute forecasts.
pub struct SeasonalNaive {
    pub max_days: usize,
}

impl Forecaster for SeasonalNaive {
    fn name(&self) -> &str {
        "seasonal-naive"
    }

    fn predict(&self, request: &ForecastRequest) -> anyhow::Result<Forecast> {
        anyhow::ensure!(
            request.step_minutes > 0 && 1440 % request.step_minutes == 0,
            "step must divide a day"
        );
        let season = (1440 / request.step_minutes) as usize;
        let history = &request.target;
        let n = history.len();
        let overall = finite_mean(history.iter().rev().take(season).copied()).unwrap_or(0.0);

        let mut quantiles = vec![Vec::with_capacity(request.horizon); FALLBACK_LEVELS.len()];
        let mut mean = Vec::with_capacity(request.horizon);
        for h in 0..request.horizon {
            let samples: Vec<f64> = (1..=self.max_days)
                .filter_map(|k| (n + h).checked_sub(k * season))
                .filter(|&i| i < n)
                .map(|i| history[i])
                .filter(|v| v.is_finite())
                .collect();
            let samples = if samples.is_empty() {
                vec![overall]
            } else {
                samples
            };
            mean.push(samples.iter().sum::<f64>() / samples.len() as f64);
            for (li, &level) in FALLBACK_LEVELS.iter().enumerate() {
                quantiles[li].push(empirical_quantile(&samples, level));
            }
        }
        Ok(Forecast {
            quantile_levels: FALLBACK_LEVELS.to_vec(),
            quantiles,
            mean,
        })
    }
}

/// Tries `primary` first and falls back on error.
pub struct WithFallback {
    pub primary: Option<Arc<dyn Forecaster>>,
    pub fallback: Arc<dyn Forecaster>,
}

impl Forecaster for WithFallback {
    fn name(&self) -> &str {
        self.primary
            .as_ref()
            .map_or(self.fallback.name(), |p| p.name())
    }

    fn predict(&self, request: &ForecastRequest) -> anyhow::Result<Forecast> {
        if let Some(primary) = &self.primary {
            match primary.predict(request) {
                Ok(forecast) if forecast.horizon() == request.horizon => return Ok(forecast),
                Ok(forecast) => warn!(
                    forecaster = primary.name(),
                    got = forecast.horizon(),
                    expected = request.horizon,
                    "Forecast has wrong horizon, using fallback"
                ),
                Err(e) => warn!(
                    forecaster = primary.name(),
                    error = %e,
                    "Forecast failed, using fallback"
                ),
            }
        }
        self.fallback.predict(request)
    }
}

fn finite_mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, count) = values
        .filter(|v| v.is_finite())
        .fold((0.0, 0usize), |(s, c), v| (s + v, c + 1));
    (count > 0).then(|| sum / count as f64)
}

pub(crate) fn empirical_quantile(samples: &[f64], level: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let pos = level * (sorted.len() - 1) as f64;
    let lower = pos.floor() as usize;
    let upper = pos.ceil() as usize;
    let w = pos - lower as f64;
    sorted[lower] * (1.0 - w) + sorted[upper] * w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(target: Vec<f64>, step_minutes: i64, horizon: usize) -> ForecastRequest {
        ForecastRequest {
            start: Timestamp::UNIX_EPOCH,
            step_minutes,
            target,
            horizon,
        }
    }

    fn assert_close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn quantile_at_interpolates_and_clamps() {
        let f = Forecast {
            quantile_levels: vec![0.1, 0.5, 0.9],
            quantiles: vec![vec![0.0], vec![10.0], vec![30.0]],
            mean: vec![12.0],
        };
        assert_eq!(f.quantile_at(0.05, 0), 0.0);
        assert_close(f.quantile_at(0.3, 0), 5.0);
        assert_close(f.quantile_at(0.7, 0), 20.0);
        assert_eq!(f.quantile_at(0.99, 0), 30.0);
    }

    #[test]
    fn seasonal_naive_uses_same_slot_of_previous_days() {
        // 4 steps per day (6 h), 2 days of history: day 1 = [1,2,3,4], day 2 = [3,4,5,6].
        let history = vec![1.0, 2.0, 3.0, 4.0, 3.0, 4.0, 5.0, 6.0];
        let f = SeasonalNaive { max_days: 7 }
            .predict(&request(history, 360, 2))
            .unwrap();
        assert_eq!(f.mean, vec![2.0, 3.0]);
        assert_close(f.quantile_at(0.1, 0), 1.2);
    }

    #[test]
    fn seasonal_naive_skips_missing_values() {
        let history = vec![1.0, 2.0, 3.0, 4.0, f64::NAN, 4.0, 5.0, 6.0];
        let f = SeasonalNaive { max_days: 7 }
            .predict(&request(history, 360, 1))
            .unwrap();
        assert_eq!(f.mean, vec![1.0]);
    }

    #[test]
    fn fallback_is_used_when_primary_fails() {
        struct Failing;
        impl Forecaster for Failing {
            fn name(&self) -> &str {
                "failing"
            }
            fn predict(&self, _: &ForecastRequest) -> anyhow::Result<Forecast> {
                anyhow::bail!("boom")
            }
        }
        let f = WithFallback {
            primary: Some(Arc::new(Failing)),
            fallback: Arc::new(SeasonalNaive { max_days: 7 }),
        }
        .predict(&request(
            vec![1.0, 2.0, 3.0, 4.0, 3.0, 4.0, 5.0, 6.0],
            360,
            2,
        ))
        .unwrap();
        assert_eq!(f.mean, vec![2.0, 3.0]);
    }
}
