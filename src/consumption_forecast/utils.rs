use std::collections::HashMap;

use jiff::{Span, Zoned, civil::Date};
use serde_json::json;

use super::PowerReading;

pub(super) const NUM_FEATURES: usize = 8;

/// Index key: local date + slot-of-day (0–95).
///
/// Using local time here is intentional: "same slot yesterday" means the same
/// clock hour, not the same UTC offset. A UTC-based index would land one slot
/// off across a DST transition.
pub(super) type SlotKey = (Date, usize);

pub(super) fn slot_of_day(t: &Zoned) -> usize {
    t.hour() as usize * 4 + t.minute() as usize / 15
}

pub(super) fn build_index(data: &[PowerReading]) -> HashMap<SlotKey, f64> {
    data.iter()
        .map(|r| {
            let key = (r.slot_start.date(), slot_of_day(&r.slot_start));
            (key, r.power_w)
        })
        .collect()
}

fn days_ago(date: Date, days: i32) -> Date {
    date.checked_sub(Span::new().days(days))
        .expect("date subtraction within reasonable range")
}

pub(super) fn feature_row(
    t: &Zoned,
    last_15min_w: f64,
    index: &HashMap<SlotKey, f64>,
) -> [f32; NUM_FEATURES] {
    let date = t.date();
    let slot = slot_of_day(t);
    let dow = t.weekday().to_monday_zero_offset() as f32; // 0 = Mon, 6 = Sun
    let is_weekend = if dow >= 5.0 { 1.0_f32 } else { 0.0 };

    let yesterday = index
        .get(&(days_ago(date, 1), slot))
        .copied()
        .unwrap_or(0.0);
    let last_week = index
        .get(&(days_ago(date, 7), slot))
        .copied()
        .unwrap_or(0.0);
    let avg_7d = {
        let vals: Vec<f64> = (1..=7)
            .filter_map(|d| index.get(&(days_ago(date, d), slot)).copied())
            .collect();
        if vals.is_empty() {
            0.0
        } else {
            vals.iter().sum::<f64>() / vals.len() as f64
        }
    };

    [
        dow,
        t.hour() as f32,
        slot as f32,
        is_weekend,
        last_15min_w as f32,
        yesterday as f32,
        last_week as f32,
        avg_7d as f32,
    ]
}

pub(super) fn lgbm_params() -> serde_json::Value {
    json!({
        "objective": "regression_l1",
        "metric": "mae",
        "num_iterations": 300,
        "learning_rate": 0.05,
        "num_leaves": 31,
        "min_child_samples": 3,
        "feature_fraction": 0.8,
        "bagging_fraction": 0.8,
        "bagging_freq": 5,
        "verbosity": -1,
    })
}
