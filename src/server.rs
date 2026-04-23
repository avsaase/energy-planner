use std::sync::Arc;

use askama::Template;
use axum::{
    Json, Router, extract::State, http::StatusCode, response::Html, routing::get,
};
use jiff::Unit;
use serde::Serialize;
use tokio::sync::{Notify, RwLock};

use crate::{plot::generate_plot, types::Planning};

#[derive(Debug, Clone)]
pub struct AppState {
    pub current_plan: Arc<RwLock<Option<Planning>>>,
    pub start_plan: Arc<Notify>,
}

pub fn router(app_state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/latest-plan", get(latest_plan))
        .with_state(app_state)
}

#[derive(Debug, Clone, Serialize)]
struct PlanningSnapshot {
    has_plot: bool,
    plot_html: String,
    interval_count: usize,
    planned_at: String,
    first_interval_start: String,
    first_interval_end: String,
    first_interval_grid_w: f64,
    first_interval_battery_w: f64,
    first_interval_solar_w: f64,
    first_interval_consumption_w: f64,
}

#[derive(Template)]
#[template(path = "index.html")]
#[allow(dead_code)]
struct IndexTemplate {
    has_plot: bool,
    plot_html: String,
    interval_count: usize,
    planned_at: String,
    first_interval_start: String,
    first_interval_end: String,
    first_interval_grid_w: f64,
    first_interval_battery_w: f64,
    first_interval_solar_w: f64,
    first_interval_consumption_w: f64,
}

async fn root(State(app_state): State<AppState>) -> Result<Html<String>, StatusCode> {
    let planning = app_state.current_plan.read().await.clone();

    let template = if let Some(planning) = planning {
        let first_interval = planning.intervals.first();

        IndexTemplate {
            has_plot: true,
            plot_html: generate_plot(&planning),
            interval_count: planning.intervals.len(),
            planned_at: planning
                .planned_at
                .time()
                .round(Unit::Second)
                .unwrap()
                .to_string(),
            first_interval_start: first_interval
                .map(|interval| {
                    interval
                        .start
                        .time()
                        .round(Unit::Second)
                        .unwrap()
                        .to_string()
                })
                .unwrap_or_else(|| "-".to_string()),
            first_interval_end: first_interval
                .map(|interval| interval.end.time().round(Unit::Second).unwrap().to_string())
                .unwrap_or_else(|| "-".to_string()),
            first_interval_grid_w: first_interval
                .map(|interval| interval.grid_import_w - interval.grid_export_w)
                .unwrap_or(0.0),
            first_interval_battery_w: first_interval
                .map(|interval| interval.battery_discharge_w - interval.battery_charge_w)
                .unwrap_or(0.0),
            first_interval_solar_w: first_interval
                .map(|interval| interval.solar_production_w)
                .unwrap_or(0.0),
            first_interval_consumption_w: first_interval
                .map(|interval| interval.consumption_w)
                .unwrap_or(0.0),
        }
    } else {
        IndexTemplate {
            has_plot: false,
            plot_html: String::new(),
            interval_count: 0,
            planned_at: "-".to_string(),
            first_interval_start: "-".to_string(),
            first_interval_end: "-".to_string(),
            first_interval_grid_w: 0.0,
            first_interval_battery_w: 0.0,
            first_interval_solar_w: 0.0,
            first_interval_consumption_w: 0.0,
        }
    };

    template
        .render()
        .map(Html)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
