//! Static files: the built React app, its SPA shell and the favicon.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use axum::response::Response;

use super::http;

const FALLBACK_FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="10" fill="#111827"/><circle cx="32" cy="32" r="20" fill="#38bdf8"/><path d="M20 34c4 8 20 8 24 0" fill="none" stroke="#fff" stroke-width="5" stroke-linecap="round"/><circle cx="25" cy="26" r="3" fill="#fff"/><circle cx="39" cy="26" r="3" fill="#fff"/></svg>"##;

/// True for paths ending in a file extension (`/assets/app.js`), which are never SPA routes.
pub fn is_asset_request(path: &str) -> bool {
    match path.rfind('.') {
        Some(dot) => {
            let ext = &path[dot + 1..];
            !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        }
        None => false,
    }
}

pub async fn serve_app_shell(dist_dir: &Path) -> Response {
    match tokio::fs::read_to_string(dist_dir.join("index.html")).await {
        Ok(html) => http::send(StatusCode::OK, html),
        Err(_) => http::send(StatusCode::INTERNAL_SERVER_ERROR, "Web build not found."),
    }
}

pub async fn serve_favicon(candidates: &[PathBuf]) -> Response {
    for candidate in candidates {
        if tokio::fs::metadata(candidate)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            if let Ok(bytes) = tokio::fs::read(candidate).await {
                return http::file("image/png", bytes);
            }
        }
    }
    http::file("image/svg+xml", FALLBACK_FAVICON.as_bytes().to_vec())
}

fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Maps a request path to a file inside `dist_dir`, refusing anything that could escape it.
fn resolve_path(dist_dir: &Path, request_path: &str) -> Option<PathBuf> {
    let decoded = urlencoding::decode(request_path).ok()?;
    let mut out = dist_dir.to_path_buf();
    let mut any = false;
    for segment in decoded.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains(['\\', ':', '\0']) {
            return None;
        }
        out.push(segment);
        any = true;
    }
    any.then_some(out)
}

/// Serves `request_path` from `dist_dir`, or `None` when it is not a regular file inside it.
pub async fn serve_static(dist_dir: &Path, request_path: &str) -> Option<Response> {
    let candidate = resolve_path(dist_dir, request_path)?;
    // Resolve symlinks and make sure the final location is still inside the web root.
    let root = tokio::fs::canonicalize(dist_dir).await.ok()?;
    let real = tokio::fs::canonicalize(&candidate).await.ok()?;
    if !real.starts_with(&root) || !tokio::fs::metadata(&real).await.ok()?.is_file() {
        return None;
    }
    let bytes = tokio::fs::read(&real).await.ok()?;
    Some(http::file(content_type(&candidate), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_detection() {
        assert!(is_asset_request("/assets/app.js"));
        assert!(is_asset_request("/favicon.png"));
        assert!(!is_asset_request("/dashboard"));
        assert!(!is_asset_request("/operator/bracket-sorter"));
        assert!(!is_asset_request("/trailing."));
    }

    #[test]
    fn traversal_is_rejected() {
        let root = Path::new("dist");
        assert!(resolve_path(root, "/../secret.txt").is_none());
        assert!(resolve_path(root, "/a/%2e%2e/b").is_none());
        assert!(resolve_path(root, "/a\\b").is_none());
        assert_eq!(
            resolve_path(root, "/assets/app.js"),
            Some(PathBuf::from("dist/assets/app.js"))
        );
        assert!(resolve_path(root, "/").is_none());
    }
}
