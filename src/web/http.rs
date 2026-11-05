//! Small response/request helpers shared by the web handlers.

use std::collections::HashMap;

use axum::body::{Body, Bytes};
use axum::http::header::{self, HeaderName, HeaderValue};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;
use serde_json::Value;

/// Maximum accepted size of a JSON request body.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

fn base(status: StatusCode, content_type: &'static str, body: impl Into<Body>) -> Response {
    let mut res = Response::new(body.into());
    *res.status_mut() = status;
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}

pub fn json<T: Serialize>(status: StatusCode, data: &T) -> Response {
    match serde_json::to_vec(data) {
        Ok(bytes) => base(status, "application/json; charset=utf-8", bytes),
        Err(err) => {
            tracing::error!("failed to serialize response: {err}");
            base(
                StatusCode::INTERNAL_SERVER_ERROR,
                "application/json; charset=utf-8",
                r#"{"error":"internal_error"}"#,
            )
        }
    }
}

pub fn api_error(status: StatusCode, message: &str) -> Response {
    json(status, &serde_json::json!({ "error": message }))
}

/// Plain text sent as `text/html`, like the Node server did.
pub fn send(status: StatusCode, body: impl Into<String>) -> Response {
    base(status, "text/html; charset=utf-8", body.into())
}

pub fn redirect(location: &str) -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::FOUND;
    if let Ok(value) = HeaderValue::from_str(location) {
        res.headers_mut().insert(header::LOCATION, value);
    }
    res
}

/// A file response with the one-hour public cache header the dashboard uses.
pub fn file(content_type: &str, body: Vec<u8>) -> Response {
    let mut res = Response::new(Body::from(body));
    if let Ok(value) = HeaderValue::from_str(content_type) {
        res.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    res
}

pub fn add_cookie(res: &mut Response, cookie: &str) {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        res.headers_mut()
            .append(HeaderName::from_static("set-cookie"), value);
    }
}

pub fn parse_query(query: Option<&str>) -> HashMap<String, String> {
    query
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default()
}

/// Why a request body could not be turned into a JSON object.
pub enum BodyError {
    TooLarge,
    Invalid,
}

pub async fn read_body(body: Body) -> Result<Bytes, BodyError> {
    axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| BodyError::TooLarge)
}

/// Parses an optional JSON object body (an empty body is `{}`).
pub async fn read_json_object(body: Body) -> Result<Value, BodyError> {
    let bytes = read_body(body).await?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(Default::default()));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(BodyError::Invalid),
    }
}
