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
            let body = Json(MeResponse { user: req.username });
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
    match session_token(&headers).and_then(|t| state.sessions.touch(&t)) {
        Some(user) => Json(MeResponse { user }).into_response(),
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
