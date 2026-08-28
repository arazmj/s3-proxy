use aws_sdk_s3::{operation::get_object::GetObjectOutput, primitives::ByteStream};
use axum::{
    body::{Body, Bytes},
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    middleware,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{stream, TryStream};
use http_body::Frame;
use metrics_exporter_prometheus::PrometheusHandle;
use pin_project_lite::pin_project;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use sync_wrapper::SyncWrapper;
use tracing::{info, instrument};

use crate::auth::{
    auth_middleware, check_bucket_access, check_write_permission, security_headers_middleware,
    AuthState,
};
use crate::config::Config;
use crate::error::{AppError, Result};
use crate::s3::{HeadObjectMetadata, ListObjectsParams, S3Client};

type BoxError = Box<dyn StdError + Send + Sync>;

pin_project! {
    struct LimitedStream<S> {
        #[pin]
        stream: S,
        remaining: u64,
        exceeded: Arc<AtomicBool>,
        done: bool,
    }
}

impl<S> LimitedStream<S> {
    fn new(stream: S, limit: u64, exceeded: Arc<AtomicBool>) -> Self {
        Self {
            stream,
            remaining: limit,
            exceeded,
            done: false,
        }
    }
}

impl<S, E> futures::Stream for LimitedStream<S>
where
    S: TryStream<Error = E>,
    S::Ok: Into<Bytes>,
    E: Into<BoxError>,
{
    type Item = std::result::Result<Bytes, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        if *this.done {
            return Poll::Ready(None);
        }

        match futures::ready!(this.stream.as_mut().try_poll_next(cx)) {
            Some(Ok(chunk)) => {
                let bytes = chunk.into();
                if bytes.len() as u64 > *this.remaining {
                    this.exceeded.store(true, Ordering::Relaxed);
                    *this.done = true;
                    Poll::Ready(Some(Err(std::io::Error::other(
                        "request body exceeds configured maximum",
                    )
                    .into())))
                } else {
                    *this.remaining -= bytes.len() as u64;
                    Poll::Ready(Some(Ok(bytes)))
                }
            }
            Some(Err(error)) => {
                *this.done = true;
                Poll::Ready(Some(Err(error.into())))
            }
            None => {
                *this.done = true;
                Poll::Ready(None)
            }
        }
    }
}

pin_project! {
    struct SyncStreamBody<S> {
        #[pin]
        stream: SyncWrapper<S>,
    }
}

impl<S> SyncStreamBody<S> {
    fn new(stream: S) -> Self {
        Self {
            stream: SyncWrapper::new(stream),
        }
    }
}

impl<S, E> http_body::Body for SyncStreamBody<S>
where
    S: TryStream<Error = E>,
    S::Ok: Into<Bytes>,
    E: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        let stream = self.project().stream.get_pin_mut();
        match futures::ready!(stream.try_poll_next(cx)) {
            Some(Ok(chunk)) => Poll::Ready(Some(Ok(Frame::data(chunk.into())))),
            Some(Err(error)) => Poll::Ready(Some(Err(error.into()))),
            None => Poll::Ready(None),
        }
    }
}

fn byte_stream_from_body(body: Body, limit: u64) -> (ByteStream, Arc<AtomicBool>) {
    let exceeded = Arc::new(AtomicBool::new(false));
    let stream = LimitedStream::new(body.into_data_stream(), limit, exceeded.clone());
    (
        ByteStream::from_body_1_x(SyncStreamBody::new(stream)),
        exceeded,
    )
}

pub struct AppState {
    pub config: Arc<Config>,
    pub clients: HashMap<String, Arc<S3Client>>,
    pub prometheus_handle: PrometheusHandle,
}

impl AppState {
    fn get_account_and_client(&self, bucket: &str) -> Result<(&str, &Arc<S3Client>)> {
        let (account_id, _account_config) = self
            .config
            .find_account_for_bucket(bucket)
            .ok_or_else(|| AppError::BucketNotFound(bucket.to_string()))?;

        let client = self
            .clients
            .get(account_id)
            .ok_or_else(|| AppError::InternalError("S3 client not found".to_string()))?;

        Ok((account_id, client))
    }
}

pub async fn create_router(state: AppState) -> Router {
    let config = state.config.clone();
    let state = Arc::new(state);

    let authenticated = Router::new()
        .route(
            "/:bucket/*key",
            get(get_object)
                .put(put_object)
                .head(head_object)
                .delete(delete_object),
        )
        .route("/:bucket", get(list_objects))
        .route_layer(middleware::from_fn_with_state(config, auth_middleware))
        .route_layer(middleware::from_fn(crate::metrics::record_http_metrics));

    let health = Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route_layer(middleware::from_fn(crate::metrics::record_http_metrics));

    Router::new()
        .route("/metrics", get(metrics_handler))
        .merge(authenticated)
        .merge(health)
        .with_state(state)
        .layer(middleware::from_fn(security_headers_middleware))
}

fn health_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers
}

async fn livez() -> impl IntoResponse {
    (StatusCode::OK, health_headers(), r#"{"status":"ok"}"#)
}

async fn readyz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let ready = !state.config.accounts.is_empty()
        && state
            .config
            .accounts
            .keys()
            .all(|account_id| state.clients.contains_key(account_id));

    if ready {
        (StatusCode::OK, health_headers(), r#"{"status":"ready"}"#)
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            health_headers(),
            r#"{"status":"not_ready"}"#,
        )
    }
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

    (StatusCode::OK, headers, state.prometheus_handle.render())
}

fn parse_range_header(value: &str) -> Result<String> {
    let value = value.trim();
    let (unit, spec) = value
        .split_once('=')
        .ok_or_else(|| AppError::InvalidRequest("Invalid Range header".to_string()))?;

    if !unit.eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return Err(AppError::InvalidRequest("Invalid Range header".to_string()));
    }

    let (start, end) = spec
        .split_once('-')
        .ok_or_else(|| AppError::InvalidRequest("Invalid Range header".to_string()))?;

    if start.is_empty() && end.is_empty() {
        return Err(AppError::InvalidRequest("Invalid Range header".to_string()));
    }

    let start_value = if start.is_empty() {
        None
    } else {
        Some(
            start
                .parse::<u64>()
                .map_err(|_| AppError::InvalidRequest("Invalid Range header".to_string()))?,
        )
    };

    let end_value = if end.is_empty() {
        None
    } else {
        Some(
            end.parse::<u64>()
                .map_err(|_| AppError::InvalidRequest("Invalid Range header".to_string()))?,
        )
    };

    if let (Some(start), Some(end)) = (start_value, end_value) {
        if end < start {
            return Err(AppError::InvalidRequest("Invalid Range header".to_string()));
        }
    }

    Ok(format!("bytes={spec}"))
}

fn requested_range(headers: &HeaderMap) -> Result<Option<String>> {
    headers
        .get(header::RANGE)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| AppError::InvalidRequest("Invalid Range header".to_string()))
                .and_then(parse_range_header)
        })
        .transpose()
}

#[axum::debug_handler]
#[instrument(skip(state), fields(bucket = %bucket, key = %key))]
async fn get_object(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthState>,
    Path((bucket, key)): Path<(String, String)>,
    request_headers: HeaderMap,
) -> Result<impl IntoResponse> {
    info!("Getting object {}/{}", bucket, key);

    // Check bucket access
    check_bucket_access(&state.config, &auth.username, &bucket)?;

    let range = requested_range(&request_headers)?;
    let (_, client) = state.get_account_and_client(&bucket)?;
    let response = client.get_object(&bucket, &key, range).await?;
    let is_partial_response = response.content_range().is_some();
    let headers = get_object_headers(&response);
    let body_stream = stream::unfold(response.body, |mut byte_stream| async {
        match byte_stream.try_next().await {
            Ok(Some(bytes)) => Some((Ok::<Bytes, std::io::Error>(bytes), byte_stream)),
            Ok(None) => None,
            Err(error) => Some((Err(std::io::Error::other(error)), byte_stream)),
        }
    });
    let body = Body::from_stream(body_stream);

    let status = if is_partial_response {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    Ok((status, headers, body))
}

fn insert_header_if_valid(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn get_object_headers(response: &GetObjectOutput) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    if let Some(content_type) = response.content_type() {
        insert_header_if_valid(&mut headers, header::CONTENT_TYPE, content_type);
    }
    if let Some(content_length) = response.content_length() {
        insert_header_if_valid(
            &mut headers,
            header::CONTENT_LENGTH,
            &content_length.to_string(),
        );
    }
    if let Some(e_tag) = response.e_tag() {
        insert_header_if_valid(&mut headers, header::ETAG, e_tag);
    }
    if let Some(last_modified) = response.last_modified() {
        use aws_sdk_s3::primitives::DateTimeFormat;
        if let Ok(last_modified) = last_modified.fmt(DateTimeFormat::HttpDate) {
            insert_header_if_valid(&mut headers, header::LAST_MODIFIED, &last_modified);
        }
    }
    if let Some(content_encoding) = response.content_encoding() {
        insert_header_if_valid(&mut headers, header::CONTENT_ENCODING, content_encoding);
    }
    if let Some(content_range) = response.content_range() {
        insert_header_if_valid(&mut headers, header::CONTENT_RANGE, content_range);
    }
    if let Some(cache_control) = response.cache_control() {
        insert_header_if_valid(&mut headers, header::CACHE_CONTROL, cache_control);
    }
    if let Some(content_disposition) = response.content_disposition() {
        insert_header_if_valid(
            &mut headers,
            header::CONTENT_DISPOSITION,
            content_disposition,
        );
    }
    if let Some(content_language) = response.content_language() {
        insert_header_if_valid(&mut headers, header::CONTENT_LANGUAGE, content_language);
    }

    headers
}

fn head_object_headers(metadata: &HeadObjectMetadata) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    if let Some(content_length) = metadata.content_length {
        insert_header_if_valid(
            &mut headers,
            header::CONTENT_LENGTH,
            &content_length.to_string(),
        );
    }
    if let Some(content_type) = &metadata.content_type {
        insert_header_if_valid(&mut headers, header::CONTENT_TYPE, content_type);
    }
    if let Some(e_tag) = &metadata.e_tag {
        insert_header_if_valid(&mut headers, header::ETAG, e_tag);
    }
    if let Some(last_modified) = &metadata.last_modified {
        insert_header_if_valid(&mut headers, header::LAST_MODIFIED, last_modified);
    }
    if let Some(content_encoding) = &metadata.content_encoding {
        insert_header_if_valid(&mut headers, header::CONTENT_ENCODING, content_encoding);
    }
    if let Some(cache_control) = &metadata.cache_control {
        insert_header_if_valid(&mut headers, header::CACHE_CONTROL, cache_control);
    }
    if let Some(content_disposition) = &metadata.content_disposition {
        insert_header_if_valid(
            &mut headers,
            header::CONTENT_DISPOSITION,
            content_disposition,
        );
    }
    if let Some(content_language) = &metadata.content_language {
        insert_header_if_valid(&mut headers, header::CONTENT_LANGUAGE, content_language);
    }

    Ok(headers)
}

#[axum::debug_handler]
#[instrument(skip(state), fields(bucket = %bucket, key = %key))]
async fn head_object(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<impl IntoResponse> {
    info!("Getting object metadata {}/{}", bucket, key);

    check_bucket_access(&state.config, &auth.username, &bucket)?;

    let (_, client) = state.get_account_and_client(&bucket)?;
    let metadata = client.head_object(&bucket, &key).await?;
    let headers = head_object_headers(&metadata)?;

    Ok((StatusCode::OK, headers, ()))
}

#[axum::debug_handler]
#[instrument(skip(state, body), fields(bucket = %bucket, key = %key))]
async fn put_object(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<impl IntoResponse> {
    info!("Putting object {}/{}", bucket, key);

    // Check bucket access and write permission
    check_bucket_access(&state.config, &auth.username, &bucket)?;
    check_write_permission(&state.config, &auth.username)?;

    let (_, client) = state.get_account_and_client(&bucket)?;

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let (body, limit_exceeded) = byte_stream_from_body(body, state.config.max_file_size);
    let result = client.put_object(&bucket, &key, body, content_type).await;
    if limit_exceeded.load(Ordering::Relaxed) {
        return Err(AppError::PayloadTooLarge(state.config.max_file_size));
    }
    result?;
    Ok(StatusCode::OK)
}

#[axum::debug_handler]
#[instrument(skip(state), fields(bucket = %bucket, key = %key))]
async fn delete_object(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<impl IntoResponse> {
    info!("Deleting object {}/{}", bucket, key);

    check_bucket_access(&state.config, &auth.username, &bucket)?;
    check_write_permission(&state.config, &auth.username)?;

    let (_, client) = state.get_account_and_client(&bucket)?;
    client.delete_object(&bucket, &key).await?;

    Ok(StatusCode::NO_CONTENT)
}

fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn format_last_modified(obj: &aws_sdk_s3::types::Object) -> String {
    use aws_sdk_s3::primitives::DateTimeFormat;
    obj.last_modified()
        .and_then(|dt| dt.fmt(DateTimeFormat::DateTime).ok())
        .unwrap_or_default()
}

fn format_xml_content(objects: &[aws_sdk_s3::types::Object]) -> String {
    objects
        .iter()
        .map(|obj| {
            format!(
                r#"    <Contents>
        <Key>{}</Key>
        <Size>{}</Size>
        <LastModified>{}</LastModified>
    </Contents>"#,
                escape_xml(obj.key().unwrap_or_default()),
                obj.size().unwrap_or(0),
                format_last_modified(obj),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct ListObjectsResponseView<'a> {
    bucket: &'a str,
    prefix: Option<&'a str>,
    start_after: Option<&'a str>,
    continuation_token: Option<&'a str>,
    max_keys: i32,
    objects: &'a [aws_sdk_s3::types::Object],
    is_truncated: bool,
    next_continuation_token: Option<&'a str>,
    key_count: i32,
}

fn parse_list_objects_params(params: &HashMap<String, String>) -> Result<ListObjectsParams> {
    let max_keys = match params.get("max-keys") {
        Some(value) => {
            let parsed = value
                .parse::<i32>()
                .map_err(|_| AppError::InvalidRequest("max-keys must be an integer".to_string()))?;
            if parsed < 0 {
                return Err(AppError::InvalidRequest(
                    "max-keys must not be negative".to_string(),
                ));
            }
            parsed.min(1000)
        }
        None => 1000,
    };

    Ok(ListObjectsParams {
        prefix: params.get("prefix").cloned(),
        start_after: params.get("start-after").cloned(),
        continuation_token: params.get("continuation-token").cloned(),
        max_keys,
    })
}

fn optional_xml_element(name: &str, value: Option<&str>) -> String {
    value
        .map(|value| format!("    <{name}>{}</{name}>\n", escape_xml(value)))
        .unwrap_or_default()
}

fn format_list_objects_xml(view: &ListObjectsResponseView<'_>) -> String {
    let start_after = optional_xml_element("StartAfter", view.start_after);
    let continuation_token = optional_xml_element("ContinuationToken", view.continuation_token);
    let next_continuation_token =
        optional_xml_element("NextContinuationToken", view.next_continuation_token);

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Name>{name}</Name>
    <Prefix>{prefix}</Prefix>
{start_after}{continuation_token}    <KeyCount>{key_count}</KeyCount>
    <MaxKeys>{max_keys}</MaxKeys>
    <IsTruncated>{is_truncated}</IsTruncated>
{next_continuation_token}{contents}
</ListBucketResult>"#,
        name = escape_xml(view.bucket),
        prefix = escape_xml(view.prefix.unwrap_or_default()),
        start_after = start_after,
        continuation_token = continuation_token,
        key_count = view.key_count,
        max_keys = view.max_keys,
        is_truncated = view.is_truncated,
        next_continuation_token = next_continuation_token,
        contents = format_xml_content(view.objects),
    )
}

#[axum::debug_handler]
#[instrument(skip(state), fields(bucket = %bucket))]
async fn list_objects(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthState>,
    Path(bucket): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse> {
    info!("Listing objects in bucket {}", bucket);

    // Check bucket access
    check_bucket_access(&state.config, &auth.username, &bucket)?;

    let (_, client) = state.get_account_and_client(&bucket)?;
    let list_params = parse_list_objects_params(&params)?;
    let prefix = list_params.prefix.clone();
    let start_after = list_params.start_after.clone();
    let continuation_token = list_params.continuation_token.clone();
    let max_keys = list_params.max_keys;
    let page = client.list_objects(&bucket, list_params).await?;

    let xml = format_list_objects_xml(&ListObjectsResponseView {
        bucket: &bucket,
        prefix: prefix.as_deref(),
        start_after: start_after.as_deref(),
        continuation_token: continuation_token.as_deref(),
        max_keys,
        objects: &page.objects,
        is_truncated: page.is_truncated,
        next_continuation_token: page.next_continuation_token.as_deref(),
        key_count: page.key_count,
    });

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/xml".parse().unwrap());

    Ok((StatusCode::OK, headers, xml))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::Request};
    use metrics_exporter_prometheus::PrometheusBuilder;
    use tower::Service;

    fn test_prometheus_handle() -> PrometheusHandle {
        use std::sync::OnceLock;
        static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
        HANDLE
            .get_or_init(|| PrometheusBuilder::new().install_recorder().unwrap())
            .clone()
    }

    fn list_view<'a>() -> ListObjectsResponseView<'a> {
        ListObjectsResponseView {
            bucket: "bucket",
            prefix: None,
            start_after: None,
            continuation_token: None,
            max_keys: 1000,
            objects: &[],
            is_truncated: false,
            next_continuation_token: None,
            key_count: 0,
        }
    }

    #[test]
    fn head_object_headers_include_metadata() {
        let headers = head_object_headers(&HeadObjectMetadata {
            content_length: Some(42),
            content_type: Some("text/plain".to_string()),
            e_tag: Some(r#""abc123""#.to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
            content_encoding: Some("gzip".to_string()),
            cache_control: Some("max-age=60".to_string()),
            content_disposition: Some("attachment".to_string()),
            content_language: Some("en".to_string()),
        })
        .unwrap();

        assert_eq!(headers[header::CONTENT_LENGTH], "42");
        assert_eq!(headers[header::CONTENT_TYPE], "text/plain");
        assert_eq!(headers[header::ETAG], r#""abc123""#);
        assert_eq!(
            headers[header::LAST_MODIFIED],
            "Wed, 21 Oct 2015 07:28:00 GMT"
        );
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(headers[header::CACHE_CONTROL], "max-age=60");
    }

    #[tokio::test]
    async fn router_accepts_explicit_head_route() {
        let config = serde_json::from_str(
            r#"{
                "accounts": {},
                "users": {},
                "server": { "host": "127.0.0.1", "port": 8080 }
            }"#,
        )
        .unwrap();
        let router = create_router(AppState {
            config: Arc::new(config),
            clients: HashMap::new(),
            prometheus_handle: test_prometheus_handle(),
        })
        .await;

        assert!(router.has_routes());
    }

    fn health_test_state(has_account: bool) -> AppState {
        let accounts = if has_account {
            r#"{
                "account-a": {
                    "endpoint_url": "http://localhost:9000",
                    "region": "us-east-1",
                    "access_key_id": "access-key",
                    "secret_access_key": "secret-key",
                    "buckets": ["bucket-a"]
                }
            }"#
        } else {
            "{}"
        };
        let config = serde_json::from_str(&format!(
            r#"{{
                "accounts": {accounts},
                "users": {{}},
                "server": {{ "host": "127.0.0.1", "port": 8080 }}
            }}"#
        ))
        .unwrap();

        AppState {
            config: Arc::new(config),
            clients: HashMap::new(),
            prometheus_handle: test_prometheus_handle(),
        }
    }

    async fn health_get(path: &str, has_account: bool) -> axum::response::Response {
        let mut router = create_router(health_test_state(has_account)).await;
        router
            .call(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn livez_is_unauthenticated_and_not_cached() {
        let response = health_get("/livez", true).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], br#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn readyz_requires_initialized_account_clients() {
        let no_accounts = health_get("/readyz", false).await;
        assert_eq!(no_accounts.status(), StatusCode::SERVICE_UNAVAILABLE);

        let missing_client = health_get("/readyz", true).await;
        assert_eq!(missing_client.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn metrics_endpoint_is_unauthenticated_and_not_cached() {
        let instrumented = health_get("/livez", false).await;
        assert_eq!(instrumented.status(), StatusCode::OK);

        let response = health_get("/metrics", false).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("http_requests_total"));
        assert!(body.contains("route_template=\"/livez\""));
    }

    #[tokio::test]
    async fn security_headers_cover_public_and_auth_error_responses() {
        let public = health_get("/livez", false).await;
        assert_eq!(public.headers()["X-Content-Type-Options"], "nosniff");
        assert!(public.headers().get("X-XSS-Protection").is_none());

        let mut router = create_router(health_test_state(false)).await;
        let unauthorized = router
            .call(
                Request::builder()
                    .uri("/bucket")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthorized.headers()["Content-Security-Policy"],
            "default-src 'none'; frame-ancestors 'none'"
        );
    }

    #[test]
    fn max_keys_zero_is_valid_and_values_over_limit_are_clamped() {
        let zero = HashMap::from([("max-keys".to_string(), "0".to_string())]);
        assert_eq!(parse_list_objects_params(&zero).unwrap().max_keys, 0);

        let large = HashMap::from([("max-keys".to_string(), "1001".to_string())]);
        assert_eq!(parse_list_objects_params(&large).unwrap().max_keys, 1000);
    }

    #[test]
    fn max_keys_negative_or_non_numeric_is_invalid() {
        for value in ["-1", "not-a-number"] {
            let params = HashMap::from([("max-keys".to_string(), value.to_string())]);
            assert!(parse_list_objects_params(&params).is_err());
        }
    }

    #[test]
    fn list_objects_xml_renders_pagination_tokens() {
        let mut view = list_view();
        view.is_truncated = true;
        view.continuation_token = Some("current&token");
        view.next_continuation_token = Some("next<token");
        let xml = format_list_objects_xml(&view);

        assert!(xml.contains("<ContinuationToken>current&amp;token</ContinuationToken>"));
        assert!(xml.contains("<NextContinuationToken>next&lt;token</NextContinuationToken>"));
    }

    #[tokio::test]
    async fn byte_stream_from_body_preserves_streamed_chunks() {
        let body = Body::from_stream(futures::stream::iter([
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"hello ")),
            Ok(Bytes::from_static(b"stream")),
        ]));

        let (stream, exceeded) = byte_stream_from_body(body, 12);
        let bytes = stream
            .collect()
            .await
            .expect("stream should collect")
            .into_bytes();

        assert_eq!(bytes, Bytes::from_static(b"hello stream"));
        assert!(!exceeded.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn byte_stream_from_body_rejects_bodies_over_limit() {
        let body = Body::from_stream(futures::stream::iter([
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"hello")),
            Ok(Bytes::from_static(b"!")),
        ]));

        let (stream, exceeded) = byte_stream_from_body(body, 5);
        assert!(stream.collect().await.is_err());
        assert!(exceeded.load(Ordering::Relaxed));
    }

    #[test]
    fn range_header_accepts_supported_forms() {
        assert_eq!(parse_range_header("bytes=0-499").unwrap(), "bytes=0-499");
        assert_eq!(parse_range_header("bytes=500-").unwrap(), "bytes=500-");
        assert_eq!(parse_range_header("bytes=-500").unwrap(), "bytes=-500");
        assert_eq!(parse_range_header("Bytes=0-0").unwrap(), "bytes=0-0");
    }

    #[test]
    fn range_header_rejects_invalid_or_multiple_ranges() {
        for value in [
            "bytes=",
            "items=0-100",
            "bytes=abc-100",
            "bytes=0-abc",
            "bytes=100-0",
            "bytes=0-1,3-4",
        ] {
            assert!(parse_range_header(value).is_err(), "{value}");
        }
    }

    #[test]
    fn get_object_headers_always_set_accept_ranges() {
        let response = GetObjectOutput::builder().build();
        let headers = get_object_headers(&response);

        assert_eq!(
            headers
                .get(header::ACCEPT_RANGES)
                .and_then(|value| value.to_str().ok()),
            Some("bytes")
        );
    }

    #[test]
    fn escape_xml_handles_all_special_chars() {
        assert_eq!(
            escape_xml(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn escape_xml_passes_through_safe_text() {
        assert_eq!(
            escape_xml("plain/path/to/file.txt"),
            "plain/path/to/file.txt"
        );
    }

    #[test]
    fn escape_xml_does_not_double_escape() {
        // Each special char should produce exactly one entity.
        assert_eq!(escape_xml("&amp;"), "&amp;amp;");
    }
}
