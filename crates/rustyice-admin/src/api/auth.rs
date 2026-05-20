use crate::state::AdminState;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

const COOKIE_NAME: &str = "rustyice_session";

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct MeResponse {
    pub user: String,
    /// "admin" or "operator". Defaults to "admin" if the configured user has
    /// no explicit role (backward compatibility — see `UserRole`).
    pub role: &'static str,
}

/// Resolve a session token to the user's role string. Looks up the
/// authenticated user in the running config and returns "admin" or
/// "operator". Returns `None` if the session is missing/expired or the
/// user is no longer present in the config.
pub fn session_role(state: &AdminState, headers: &HeaderMap) -> Option<(String, &'static str)> {
    let token = session_token(headers)?;
    let username = state.sessions.touch(&token)?;
    let cfg = state.config.load_full();
    let role = cfg
        .auth
        .users
        .iter()
        .find(|u| u.username == username)
        .map(|u| match u.role {
            rustyice_core::config::UserRole::Admin => "admin",
            rustyice_core::config::UserRole::Operator => "operator",
        })
        // If the session is valid but the user isn't in the config anymore
        // (e.g. config was edited out from under them), fall back to the
        // most-restrictive role.
        .unwrap_or("operator");
    Some((username, role))
}

pub async fn login(
    State(state): State<AdminState>,
    Json(req): Json<LoginRequest>,
) -> Response {
    match state.auth.verify_admin(&req.username, &req.password).await {
        Ok(true) => {
            let token = state.sessions.create(req.username.clone());
            let cookie = format!(
                "{COOKIE_NAME}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400"
            );
            // Look up the role from the running config for the response body.
            let cfg = state.config.load_full();
            let role = cfg
                .auth
                .users
                .iter()
                .find(|u| u.username == req.username)
                .map(|u| match u.role {
                    rustyice_core::config::UserRole::Admin => "admin",
                    rustyice_core::config::UserRole::Operator => "operator",
                })
                .unwrap_or("operator");
            let body = Json(MeResponse { user: req.username, role });
            ([(header::SET_COOKIE, cookie)], body).into_response()
        }
        Ok(false) => (StatusCode::UNAUTHORIZED, "invalid credentials").into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "auth backend error").into_response(),
    }
}

pub async fn logout(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = session_token(&headers) {
        state.sessions.revoke(&token);
    }
    // Clearing cookie: Max-Age=0.
    let cookie = format!("{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response()
}

pub async fn me(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Response {
    match session_role(&state, &headers) {
        Some((user, role)) => Json(MeResponse { user, role }).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Middleware that requires a valid session cookie. Used on destructive and
/// detail-listing endpoints.
pub async fn require_session(
    State(state): State<AdminState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    match session_token(&headers).and_then(|t| state.sessions.touch(&t)) {
        Some(_) => next.run(request).await,
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Middleware that additionally requires the session's user to be an Admin.
/// Returns `401` for an unauthenticated request, `403` for an authenticated
/// non-admin.
pub async fn require_admin(
    State(state): State<AdminState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    match session_role(&state, &headers) {
        Some((_, "admin")) => next.run(request).await,
        Some(_) => StatusCode::FORBIDDEN.into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|pair| {
                let pair = pair.trim();
                let (name, value) = pair.split_once('=')?;
                (name == COOKIE_NAME).then(|| value.to_string())
            })
        })
}
