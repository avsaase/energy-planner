use std::sync::Arc;

use askama::Template;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{Html, Redirect},
    routing::{get, post},
};
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
        .route("/start-plan", post(start_plan))
        .with_state(app_state)
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    has_plot: bool,
    plot_html: String,
    interval_count: usize,
}

async fn root(State(app_state): State<AppState>) -> Result<Html<String>, StatusCode> {
    let planning = app_state.current_plan.read().await.clone();

    let template = if let Some(planning) = planning {
        IndexTemplate {
            has_plot: true,
            plot_html: generate_plot(&planning),
            interval_count: planning.intervals.len(),
        }
    } else {
        IndexTemplate {
            has_plot: false,
            plot_html: String::new(),
            interval_count: 0,
        }
    };

    template
        .render()
        .map(Html)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn start_plan(State(app_state): State<AppState>) -> Redirect {
    app_state.start_plan.notify_one();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    Redirect::to(".")
}
