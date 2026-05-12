use crate::state::AdminState;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

pub async fn metrics_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let body = state.prometheus.render();
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}
