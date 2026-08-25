use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use http::{HeaderValue, header::HeaderName};
use metrics::{counter, histogram};
use uuid::Uuid;

pub static REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

#[derive(Clone, Debug)]
pub struct RequestId(pub String);

pub async fn request_id(mut request: Request, next: Next) -> Response {
    let request_id = match request
        .headers()
        .get(&REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| is_valid_request_id(value))
        .map(str::to_owned)
    {
        Some(request_id) => request_id,
        None => Uuid::now_v7().to_string(),
    };
    request
        .extensions_mut()
        .insert(RequestId(request_id.clone()));
    let mut response = next.run(request).await;
    if let Ok(header_value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(REQUEST_ID_HEADER.clone(), header_value);
    }
    response
}

pub async fn request_metrics(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |path| path.as_str().to_owned());
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16().to_string();
    counter!(
        "http_requests_total",
        "method" => method.clone(),
        "route" => route.clone(),
        "status" => status
    )
    .increment(1);
    histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "route" => route
    )
    .record(started.elapsed().as_secs_f64());
    response
}

pub fn panic_response(
    _panic: Box<dyn std::any::Any + Send + 'static>,
) -> http::Response<axum::body::Body> {
    let body = concat!(
        "{\"type\":\"https://resume-matcher.example/problems/internal-error\",",
        "\"title\":\"Internal Server Error\",\"status\":500,",
        "\"detail\":\"The service could not complete the request.\"}"
    );
    let mut response = http::Response::new(axum::body::Body::from(body));
    *response.status_mut() = http::StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/problem+json"),
    );
    response
}

fn is_valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
