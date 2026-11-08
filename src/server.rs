//! The web UI. The page loads the plan as JSON (`plan`) and follows updates over Server-Sent
//! Events (`events`), falling back to polling `version` when events don't get through.
//!
//! Complete replan records can be downloaded per day from `runs` (see `run_log`).
//!
//! All routes are used with relative URLs from the page, so they work under Home Assistant's
//! ingress path prefix. SSE through ingress needs HA Core with
//! home-assistant/core#160704 (no compression of `text/event-stream`, 2026.3/2026.4).

use std::{convert::Infallible, time::Duration};

use askama::Template;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderName, StatusCode, header},
    response::{
        Html, IntoResponse,
        sse::{Event, Sse},
    },
    routing::get,
};
use futures::Stream;
use jiff::{Unit, Zoned};
use serde::Serialize;
use tokio::sync::watch;

use crate::{AppState, plot::generate_plots, run_log, types::Planning};

/// Visible heartbeat on the event stream. The page falls back to polling if it receives neither
/// a plan update nor a ping for a while (a buffering proxy delivers neither).
const PING_INTERVAL: Duration = Duration::from_secs(15);

pub fn router(app_state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/plan", get(plan))
        .route("/version", get(version))
        .route("/events", get(events))
        .route("/runs", get(runs))
        .route("/runs/{name}", get(run_file))
        .with_state(app_state)
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {}

#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewTemplate {
    planned_at: String,
    first_interval_start: String,
    first_interval_end: String,
    first_interval_grid_w: f64,
    first_interval_battery_w: f64,
    first_interval_solar_w: f64,
    first_interval_consumption_w: f64,
    first_interval_mode: String,
    first_interval_scenarios: String,
}

#[derive(Serialize)]
struct PlanResponse {
    version: u64,
    interval_count: usize,
    overview_html: String,
    plots: Vec<PlotResponse>,
}

#[derive(Serialize)]
struct PlotResponse {
    id: &'static str,
    /// Plotly figure: `{data, layout, config}`.
    figure: serde_json::Value,
}

fn internal_error(e: impl std::fmt::Display) -> StatusCode {
    tracing::error!(error = %e, "Failed to render web UI");
    StatusCode::INTERNAL_SERVER_ERROR
}

async fn root() -> Result<Html<String>, StatusCode> {
    IndexTemplate {}.render().map(Html).map_err(internal_error)
}

async fn plan(State(app_state): State<AppState>) -> Result<Json<PlanResponse>, StatusCode> {
    // Read the version first: if a new plan arrives in between, the page gets the newer plan
    // with the older version and simply fetches once more on the next event.
    let version = *app_state.plan_version.borrow();
    let Some(planning) = app_state.read().await.current_plan.clone() else {
        return Ok(Json(PlanResponse {
            version,
            interval_count: 0,
            overview_html: String::new(),
            plots: Vec::new(),
        }));
    };

    let plots = generate_plots(&planning)
        .into_iter()
        .map(|(id, plot)| {
            Ok(PlotResponse {
                id,
                figure: serde_json::to_value(&plot)?,
            })
        })
        .collect::<Result<_, serde_json::Error>>()
        .map_err(internal_error)?;

    Ok(Json(PlanResponse {
        version,
        interval_count: planning.intervals.len(),
        overview_html: overview(&planning).render().map_err(internal_error)?,
        plots,
    }))
}

fn overview(planning: &Planning) -> OverviewTemplate {
    let time = |t: &Zoned| t.time().round(Unit::Second).unwrap().to_string();
    let first = planning.intervals.first();
    OverviewTemplate {
        planned_at: time(&planning.planned_at),
        first_interval_start: first.map_or_else(|| "-".into(), |i| time(&i.start)),
        first_interval_end: first.map_or_else(|| "-".into(), |i| time(&i.end)),
        first_interval_grid_w: first.map_or(0.0, |i| i.grid_import_w - i.grid_export_w),
        first_interval_battery_w: first.map_or(0.0, |i| i.battery_discharge_w - i.battery_charge_w),
        first_interval_solar_w: first.map_or(0.0, |i| i.solar_production_w),
        first_interval_consumption_w: first.map_or(0.0, |i| i.consumption_w),
        first_interval_mode: first
            .map_or_else(|| "-".into(), |i| format!("{:?}", i.battery_intent)),
        first_interval_scenarios: first.and_then(|i| i.scenario).map_or_else(
            || "-".into(),
            |s| {
                format!(
                    "net load {:.0} / {:.0} / {:.0} W → battery {:.0} / {:.0} / {:.0} W",
                    s.net_load_low_w,
                    s.net_load_mid_w,
                    s.net_load_high_w,
                    s.battery_low_w,
                    s.battery_mid_w,
                    s.battery_high_w,
                )
            },
        ),
    }
}

async fn version(State(app_state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": *app_state.plan_version.borrow() }))
}

#[derive(Serialize)]
struct RunFileResponse {
    name: String,
    bytes: u64,
}

/// The downloadable run log files, newest first.
async fn runs() -> Json<Vec<RunFileResponse>> {
    Json(
        run_log::list_files()
            .into_iter()
            .map(|(name, bytes)| RunFileResponse { name, bytes })
            .collect(),
    )
}

async fn run_file(Path(name): Path<String>) -> Result<impl IntoResponse, StatusCode> {
    let path = run_log::file_path(&name).ok_or(StatusCode::NOT_FOUND)?;
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    ))
}

/// `plan` events carry the plan version: the current one right away, then one per new plan.
/// `ping` events keep proxies from closing an idle connection and let the page notice when
/// events are not getting through.
async fn events(State(app_state): State<AppState>) -> impl IntoResponse {
    let stream = plan_events(app_state.plan_version.subscribe(), PING_INTERVAL);
    (
        // Ask any proxy in between not to buffer the stream.
        [(HeaderName::from_static("x-accel-buffering"), "no")],
        Sse::new(stream),
    )
}

fn plan_events(
    receiver: watch::Receiver<u64>,
    ping_interval: Duration,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let plan_event = |version: u64| Event::default().event("plan").data(version.to_string());
    futures::stream::unfold((receiver, true), move |(mut receiver, first)| async move {
        if first {
            let version = *receiver.borrow_and_update();
            return Some((Ok(plan_event(version)), (receiver, false)));
        }
        tokio::select! {
            changed = receiver.changed() => {
                // The sender lives as long as the app: an error means shutdown.
                changed.ok()?;
                let version = *receiver.borrow_and_update();
                Some((Ok(plan_event(version)), (receiver, false)))
            }
            _ = tokio::time::sleep(ping_interval) => {
                // EventSource ignores events without data, so send something.
                Some((Ok(Event::default().event("ping").data("1")), (receiver, false)))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    async fn next(events: &mut (impl Stream<Item = Result<Event, Infallible>> + Unpin)) -> String {
        format!("{:?}", events.next().await.unwrap().unwrap())
    }

    #[tokio::test(start_paused = true)]
    async fn events_send_current_version_then_updates_and_pings() {
        let sender = watch::Sender::new(3u64);
        let mut events = Box::pin(plan_events(sender.subscribe(), Duration::from_secs(15)));

        let first = next(&mut events).await;
        assert!(first.contains("plan") && first.contains('3'), "{first}");
        sender.send_modify(|v| *v += 1);
        let update = next(&mut events).await;
        assert!(update.contains("plan") && update.contains('4'), "{update}");
        // Nothing new: the paused clock advances to the ping.
        assert!(next(&mut events).await.contains("ping"));
    }
}
