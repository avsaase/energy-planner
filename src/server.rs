use std::{convert::Infallible, sync::Arc};

use askama::Template;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{Html, Sse, sse::Event},
    routing::get,
};
use futures::stream;
use jiff::Unit;
use tokio::sync::{Notify, RwLock};

use crate::{plot::generate_plot, types::Planning};

#[derive(Debug, Clone)]
pub struct AppState {
    pub current_plan: Arc<RwLock<Option<Planning>>>,
    pub start_plan: Arc<Notify>,
    pub plan_updated: Arc<Notify>,
}

pub fn router(app_state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/plan-overview-content", get(plan_overview_content))
        .route("/plot-content", get(plot_content))
        .route("/events/planning", get(planning_events))
        .with_state(app_state)
}

#[derive(Debug, Clone)]
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

impl PlanningSnapshot {
    fn from_planning(planning: Option<&Planning>) -> Self {
        if let Some(planning) = planning {
            let first_interval = planning.intervals.first();

            Self {
                has_plot: true,
                plot_html: generate_plot(planning),
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
                    .map(|interval| interval.battery_charge_w - interval.battery_discharge_w)
                    .unwrap_or(0.0),
                first_interval_solar_w: first_interval
                    .map(|interval| interval.solar_production_w)
                    .unwrap_or(0.0),
                first_interval_consumption_w: first_interval
                    .map(|interval| interval.consumption_w)
                    .unwrap_or(0.0),
            }
        } else {
            Self {
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
        }
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    plan_overview_content_html: String,
    plot_content_html: String,
}

#[derive(Template)]
#[template(path = "plan_overview_content.html")]
#[allow(dead_code)]
struct PlanOverviewContentTemplate {
    has_plot: bool,
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
#[template(path = "plot_content.html")]
#[allow(dead_code)]
struct PlotContentTemplate {
    has_plot: bool,
    plot_html: String,
}

impl From<&PlanningSnapshot> for PlanOverviewContentTemplate {
    fn from(snapshot: &PlanningSnapshot) -> Self {
        Self {
            has_plot: snapshot.has_plot,
            interval_count: snapshot.interval_count,
            planned_at: snapshot.planned_at.clone(),
            first_interval_start: snapshot.first_interval_start.clone(),
            first_interval_end: snapshot.first_interval_end.clone(),
            first_interval_grid_w: snapshot.first_interval_grid_w,
            first_interval_battery_w: snapshot.first_interval_battery_w,
            first_interval_solar_w: snapshot.first_interval_solar_w,
            first_interval_consumption_w: snapshot.first_interval_consumption_w,
        }
    }
}

impl From<&PlanningSnapshot> for PlotContentTemplate {
    fn from(snapshot: &PlanningSnapshot) -> Self {
        Self {
            has_plot: snapshot.has_plot,
            plot_html: snapshot.plot_html.clone(),
        }
    }
}

async fn root(State(app_state): State<AppState>) -> Result<Html<String>, StatusCode> {
    let snapshot = planning_snapshot(&app_state).await;
    let plan_overview_content_html = PlanOverviewContentTemplate::from(&snapshot)
        .render()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let plot_content_html = PlotContentTemplate::from(&snapshot)
        .render()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let template = IndexTemplate {
        plan_overview_content_html,
        plot_content_html,
    };

    template
        .render()
        .map(Html)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn plan_overview_content(
    State(app_state): State<AppState>,
) -> Result<Html<String>, StatusCode> {
    let snapshot = planning_snapshot(&app_state).await;
    PlanOverviewContentTemplate::from(&snapshot)
        .render()
        .map(Html)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn plot_content(State(app_state): State<AppState>) -> Result<Html<String>, StatusCode> {
    let snapshot = planning_snapshot(&app_state).await;
    PlotContentTemplate::from(&snapshot)
        .render()
        .map(Html)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn planning_snapshot(app_state: &AppState) -> PlanningSnapshot {
    let planning = app_state.current_plan.read().await.clone();
    PlanningSnapshot::from_planning(planning.as_ref())
}

async fn planning_events(
    State(app_state): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let event_stream = stream::unfold(app_state, |app_state| async move {
        app_state.plan_updated.notified().await;
        let event = Event::default().event("plan-update").data("updated");
        Some((Ok(event), app_state))
    });

    Sse::new(event_stream)
}
