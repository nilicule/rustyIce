use crate::api::{actions, auth, config, mounts, stats, title};
use crate::metrics::metrics_handler;
use crate::state::AdminState;
use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::IntoResponse;
use axum::{
    routing::{delete, get, post, put},
    Router,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

async fn serve_asset(Path(path): Path<String>) -> impl IntoResponse {
    match Assets::get(&path) {
        Some(content) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.essence_str().to_string())],
                content.data.to_vec(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_index() -> impl IntoResponse {
    match Assets::get("index.html") {
        Some(content) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            content.data.to_vec(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub fn build_admin_router(state: AdminState) -> Router {
    // Admin-only routes: anything that exposes listener-level detail or mutates
    // server state goes here.
    let protected = Router::new()
        .route("/api/mounts/{path}/listeners", get(mounts::list_listeners))
        .route("/api/mounts/{path}/source", delete(actions::kick_source))
        .route("/api/mounts/{path}/title", put(title::set_title))
        .route("/api/mounts/{path}/title", delete(title::clear_title))
        .route("/api/listeners/{id}", delete(actions::kick_listener))
        .route("/api/config", get(config::get_config))
        .route("/api/config/server", put(config::put_server))
        .route("/api/config/transcode", put(config::put_transcode))
        .route("/api/config/mounts", put(config::put_mounts))
        .route("/api/logout", post(auth::logout))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    // Public routes: landing page, static assets, read-only metadata, and the
    // login endpoint itself.
    let public = Router::new()
        .route("/", get(serve_index))
        .route("/{path}", get(serve_asset))
        .route("/api/mounts", get(mounts::list_mounts))
        .route("/api/stats", get(stats::global_stats))
        .route("/api/login", post(auth::login))
        .route("/api/me", get(auth::me))
        .route("/metrics", get(metrics_handler));

    public.merge(protected).with_state(state)
}
