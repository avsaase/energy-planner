//! Chronos-2 exported to ONNX (`forecaster/export_onnx.py`), run with ONNX Runtime.
//!
//! The model is univariate with fixed shapes: input `context` (1, context_steps) with NaN for
//! missing values, output `quantiles` (1, levels, output_steps). Levels and shapes are read from
//! the model's metadata.

use std::{path::Path, sync::Mutex};

use anyhow::Context;
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use tracing::info;

use super::{Forecast, ForecastRequest, Forecaster};

/// The expected value is estimated as the average of the equally spaced levels 0.05..=0.95
/// (the tails 0.01 and 0.99 would over-weight extremes).
const MEAN_LEVELS: (f64, f64) = (0.05, 0.95);

pub struct OnnxForecaster {
    name: String,
    session: Mutex<Session>,
    quantile_levels: Vec<f64>,
    context_steps: usize,
    output_steps: usize,
}

fn ort_error(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

impl OnnxForecaster {
    /// Loads the model. Takes a few seconds; call from a blocking context.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let environment = ort::init()
            .with_name("energy-planner")
            .build()
            .map_err(ort_error)?;
        let session = Session::builder(&environment)
            .map_err(ort_error)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(ort_error)?
            .commit_from_file(path)
            .map_err(ort_error)
            .with_context(|| format!("Failed to load {}", path.display()))?;

        let metadata = session.metadata().map_err(ort_error)?;
        let custom = |key: &str| {
            metadata
                .custom(key)
                .with_context(|| format!("Model metadata has no {key:?}"))
        };
        let quantile_levels: Vec<f64> = serde_json::from_str(&custom("quantile_levels")?)
            .context("Invalid quantile_levels metadata")?;
        let context_steps: usize = custom("context_steps")?.parse()?;
        let output_steps: usize = custom("output_steps")?.parse()?;
        let source = custom("source_model").unwrap_or_else(|_| "unknown".into());
        drop(metadata);

        anyhow::ensure!(
            quantile_levels.windows(2).all(|w| w[0] < w[1]),
            "Quantile levels must be ascending"
        );
        info!(
            path = %path.display(),
            source,
            context_steps,
            output_steps,
            "Loaded ONNX forecaster"
        );
        Ok(Self {
            name: format!("onnx:{source}"),
            session: Mutex::new(session),
            quantile_levels,
            context_steps,
            output_steps,
        })
    }

    /// The model's fixed history length.
    pub fn context_steps(&self) -> usize {
        self.context_steps
    }

    /// Runs the model on exactly `context_steps` values; returns `quantiles[level][step]` for
    /// all `output_steps`.
    fn run(&self, context: Vec<f32>) -> anyhow::Result<Vec<Vec<f64>>> {
        let input =
            Tensor::from_array(([1usize, self.context_steps], context)).map_err(ort_error)?;
        let mut session = self.session.lock().unwrap();
        let outputs = session
            .run(ort::inputs!["context" => input])
            .map_err(ort_error)?;
        let (shape, data) = outputs["quantiles"]
            .try_extract_tensor::<f32>()
            .map_err(ort_error)?;
        let levels = self.quantile_levels.len();
        anyhow::ensure!(
            shape.len() == 3
                && shape[1] as usize == levels
                && shape[2] as usize == self.output_steps,
            "Unexpected output shape {shape:?}"
        );
        Ok((0..levels)
            .map(|l| {
                data[l * self.output_steps..(l + 1) * self.output_steps]
                    .iter()
                    .map(|&v| v as f64)
                    .collect()
            })
            .collect())
    }
}

impl Forecaster for OnnxForecaster {
    fn name(&self) -> &str {
        &self.name
    }

    fn predict(&self, request: &ForecastRequest) -> anyhow::Result<Forecast> {
        anyhow::ensure!(
            request.horizon <= self.output_steps,
            "Horizon {} exceeds the model's {} output steps",
            request.horizon,
            self.output_steps
        );
        // Exactly `context_steps` values: keep the most recent ones, pad missing history with NaN.
        let mut context = vec![f32::NAN; self.context_steps];
        let n = request.target.len().min(self.context_steps);
        for (slot, value) in context[self.context_steps - n..]
            .iter_mut()
            .zip(&request.target[request.target.len() - n..])
        {
            *slot = *value as f32;
        }
        anyhow::ensure!(
            context.iter().any(|v| v.is_finite()),
            "No history to forecast from"
        );

        let raw = self.run(context)?;
        Ok(to_forecast(&self.quantile_levels, &raw, request.horizon))
    }
}

/// Cuts the raw quantiles to `horizon`, removes quantile crossing and estimates the mean.
fn to_forecast(levels: &[f64], raw: &[Vec<f64>], horizon: usize) -> Forecast {
    let mut quantiles: Vec<Vec<f64>> = raw.iter().map(|q| q[..horizon].to_vec()).collect();
    for step in 0..horizon {
        let mut column: Vec<f64> = quantiles.iter().map(|q| q[step]).collect();
        column.sort_by(f64::total_cmp);
        for (q, v) in quantiles.iter_mut().zip(column) {
            q[step] = v;
        }
    }
    let mean_rows: Vec<&Vec<f64>> = levels
        .iter()
        .zip(&quantiles)
        .filter(|(l, _)| **l >= MEAN_LEVELS.0 - 1e-9 && **l <= MEAN_LEVELS.1 + 1e-9)
        .map(|(_, q)| q)
        .collect();
    let mean = (0..horizon)
        .map(|step| mean_rows.iter().map(|q| q[step]).sum::<f64>() / mean_rows.len() as f64)
        .collect();
    Forecast {
        quantile_levels: levels.to_vec(),
        quantiles,
        mean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_forecast_sorts_cuts_and_averages_inner_levels() {
        let levels = [0.01, 0.05, 0.5, 0.95, 0.99];
        let raw = vec![
            vec![0.0, 0.0, 9.0],
            vec![10.0, 30.0, 9.0], // crosses with the median at step 1
            vec![20.0, 20.0, 9.0],
            vec![30.0, 40.0, 9.0],
            vec![1000.0, 1000.0, 9.0],
        ];
        let f = to_forecast(&levels, &raw, 2);
        assert_eq!(f.quantiles[1], vec![10.0, 20.0]);
        assert_eq!(f.quantiles[2], vec![20.0, 30.0]);
        // Mean over 0.05, 0.5 and 0.95 only.
        assert_eq!(f.mean, vec![20.0, 30.0]);
    }

    /// Compares with the PyTorch output saved by `export_onnx.py`. Needs the exported model:
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn matches_pytorch_reference() {
        let dir = crate::data_dir().join("models");
        let model = OnnxForecaster::load(&dir.join("chronos2.onnx")).unwrap();

        #[derive(serde::Deserialize)]
        struct Reference {
            context: Vec<Option<f64>>,
            quantiles: Vec<Vec<f64>>,
        }
        let reference: Reference =
            serde_json::from_reader(std::fs::File::open(dir.join("chronos2.check.json")).unwrap())
                .unwrap();
        let context: Vec<f32> = reference
            .context
            .iter()
            .map(|v| v.map_or(f32::NAN, |v| v as f32))
            .collect();

        let raw = model.run(context).unwrap();
        let max_diff = raw
            .iter()
            .flatten()
            .zip(reference.quantiles.iter().flatten())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(max_diff < 0.5, "max deviation {max_diff} W");
    }
}
