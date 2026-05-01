use anyhow::Context;
use lightgbm3::{Booster, Dataset};

use super::ForecastModel;
use super::PowerReading;
use super::utils::{NUM_FEATURES, SlotKey, build_index, feature_row, lgbm_params, slot_of_day};

pub fn train(data: &[PowerReading]) -> anyhow::Result<ForecastModel> {
    let mut sorted: Vec<&PowerReading> = data.iter().collect();
    sorted.sort_by_key(|r| &r.slot_start);

    let index = build_index(data);

    let mut feature_mat: Vec<f32> = Vec::new();
    let mut labels: Vec<f32> = Vec::new();

    for (i, r) in sorted.iter().enumerate() {
        let yesterday: SlotKey = (
            r.slot_start
                .date()
                .checked_sub(jiff::Span::new().days(1))
                .expect("date sub"),
            slot_of_day(&r.slot_start),
        );
        if index.contains_key(&yesterday) {
            let last_15min_w = if i > 0 { sorted[i - 1].power_w } else { 0.0 };
            let row = feature_row(&r.slot_start, last_15min_w, &index);
            feature_mat.extend_from_slice(&row);
            labels.push(r.power_w as f32);
        }
    }

    anyhow::ensure!(!labels.is_empty(), "need at least 2 days of training data");

    let dataset = Dataset::from_slice(&feature_mat, &labels, NUM_FEATURES as i32, true)
        .context("failed to create LightGBM dataset")?;

    let booster =
        Booster::train(dataset, &lgbm_params()).context("failed to train LightGBM model")?;

    Ok(ForecastModel { booster })
}
