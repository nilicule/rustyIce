//! Server-side filesystem browser. Used by the admin UI's autodj editor
//! to pick a folder for the music library without making the operator
//! type the path by hand.
//!
//! Auth: gated by the existing session middleware in `router.rs`, so any
//! authenticated user can browse. The endpoint never exposes file
//! contents — only directory listings.

use crate::state::AdminState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
pub struct ListDirQuery {
    /// Absolute path on the server. When missing/empty, defaults to the
    /// process's home directory.
    pub path: Option<String>,
}

#[derive(Serialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Serialize)]
pub struct ListDirResponse {
    /// Canonicalized absolute path of the listed directory.
    pub path: String,
    /// Parent directory, or `None` if at the filesystem root.
    pub parent: Option<String>,
    /// Directory entries, sorted alphabetically with directories first.
    pub entries: Vec<DirEntry>,
}

#[derive(Serialize)]
pub struct ListDirError {
    pub error: String,
    pub path: String,
}

pub async fn list_dir(
    State(_state): State<AdminState>,
    Query(q): Query<ListDirQuery>,
) -> impl IntoResponse {
    let requested = q.path.as_deref().unwrap_or("").trim();
    let path = if requested.is_empty() {
        match std::env::home_dir() {
            Some(p) => p,
            None => PathBuf::from("/"),
        }
    } else {
        PathBuf::from(requested)
    };

    // Canonicalize so the response always reports an absolute, symlink-
    // resolved path. If canonicalization fails (path doesn't exist /
    // permission denied), surface the underlying error.
    let canonical = match std::fs::canonicalize(&path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(ListDirError {
                    error: format!("cannot open: {e}"),
                    path: path.display().to_string(),
                }),
            )
                .into_response();
        }
    };

    if !canonical.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ListDirError {
                error: "not a directory".to_string(),
                path: canonical.display().to_string(),
            }),
        )
            .into_response();
    }

    let read = match std::fs::read_dir(&canonical) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::FORBIDDEN,
                Json(ListDirError {
                    error: format!("read_dir: {e}"),
                    path: canonical.display().to_string(),
                }),
            )
                .into_response();
        }
    };

    let mut entries: Vec<DirEntry> = read
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip dotfiles to keep the picker tidy. Users who need them
            // can type the path by hand.
            if name.starts_with('.') {
                return None;
            }
            let is_dir = entry.file_type().ok().is_some_and(|ft| ft.is_dir());
            Some(DirEntry { name, is_dir })
        })
        .collect();

    // Directories first, then alphabetical within each group.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    let parent = canonical.parent().and_then(|p| {
        // `Path::new("/").parent()` is None already; the explicit check
        // here is for clarity.
        if p.as_os_str().is_empty() {
            None
        } else {
            Some(p.display().to_string())
        }
    });

    (
        StatusCode::OK,
        Json(ListDirResponse {
            path: canonical.display().to_string(),
            parent,
            entries,
        }),
    )
        .into_response()
}

// Use `Path` so we don't trip the unused-import warning when the only
// reference is inside test paths.
#[allow(dead_code)]
fn _force_use(_: &Path) {}
