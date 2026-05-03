use std::collections::HashMap;

use jiff::{
    Span, Unit, Zoned,
    civil::{Date, Time, Weekday},
};
use serde_json::json;

use super::PowerReading;

pub(super) const NUM_FEATURES: usize = 8;

/// Index key: civil date + civil time.
pub(super) type SlotKey = (Date, Time);

pub(super) fn build_index(data: &[PowerReading]) -> HashMap<SlotKey, f64> {
    data.iter()
        .map(|r| ((r.slot_start.date(), r.slot_start.time()), r.power_w))
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
    let time = t.time();
    let dow = t.weekday().to_monday_zero_offset() as f32; // 0 = Mon, 6 = Sun
    let is_weekend = matches!(t.weekday(), Weekday::Saturday | Weekday::Sunday);

    let minutes_since_midnight = t
        .start_of_day()
        .expect("Overflow")
        .until(t)
        .expect("Overflow")
        .total(Unit::Minute)
        .expect("Span not larger than hours");

    let yesterday = index
        .get(&(days_ago(date, 1), time))
        .copied()
        .unwrap_or(0.0);
    let last_week = index
        .get(&(days_ago(date, 7), time))
        .copied()
        .unwrap_or(0.0);
    let avg_7d = {
        let vals: Vec<f64> = (1..=7)
            .filter_map(|d| index.get(&(days_ago(date, d), time)).copied())
            .collect();
        if vals.is_empty() {
            0.0
        } else {
            vals.iter().sum::<f64>() / vals.len() as f64
        }
    };

    [
        dow,
        is_weekend as u8 as f32,
        t.hour() as f32,
        minutes_since_midnight as f32,
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
