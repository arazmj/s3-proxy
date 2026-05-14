use std::time::Instant;

use ::metrics::{counter, histogram};
use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};

pub async fn record_http_metrics(request: Request, next: Next) -> Response {
    let method = request.method().as_str().to_owned();
    let route_template = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    let start = Instant::now();

    let response = next.run(request).await;
    let status = response.status().as_u16().to_string();
    let duration = start.elapsed().as_secs_f64();

    counter!(
        "http_requests_total",
        "method" => method.clone(),
        "route_template" => route_template.clone(),
        "status" => status,
    )
    .increment(1);
    histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "route_template" => route_template,
    )
    .record(duration);

    response
}
