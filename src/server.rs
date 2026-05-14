use axum::{
    body::Bytes,
    extract::{Path, Query, State, Extension},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::collections::HashMap;
use std::sync::Arc;
use aws_sdk_s3::primitives::ByteStream;
use tracing::{info, instrument};

use crate::config::Config;
use crate::s3::{HeadObjectMetadata, S3Client};
use crate::error::{AppError, Result};
use crate::auth::{AuthState, auth_middleware, check_bucket_access, check_write_permission};

pub struct AppState {
    pub config: Arc<Config>,
    pub clients: HashMap<String, Arc<S3Client>>,
}

impl AppState {
    fn get_account_and_client(&self, bucket: &str) -> Result<(&str, &Arc<S3Client>)> {
        let (account_id, _account_config) = self.config
            .find_account_for_bucket(bucket)
            .ok_or_else(|| AppError::BucketNotFound(bucket.to_string()))?;

        let client = self.clients
            .get(account_id)
            .ok_or_else(|| AppError::InternalError("S3 client not found".to_string()))?;

        Ok((account_id, client))
    }
}

pub async fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/:bucket/*key", get(get_object).put(put_object).head(head_object))
        .route("/:bucket", get(list_objects))
        .layer(axum::middleware::from_fn_with_state(
            state.config.clone(),
            auth_middleware,
        ))
        .with_state(Arc::new(state))
}

fn insert_metadata_header(headers: &mut HeaderMap, name: HeaderName, value: &str) -> Result<()> {
    let value = HeaderValue::from_str(value)
        .map_err(|e| AppError::InternalError(format!("invalid HeadObject header value: {}", e)))?;
    headers.insert(name, value);
    Ok(())
}

fn head_object_headers(metadata: &HeadObjectMetadata) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    if let Some(content_length) = metadata.content_length {
        insert_metadata_header(&mut headers, header::CONTENT_LENGTH, &content_length.to_string())?;
    }

    if let Some(content_type) = &metadata.content_type {
        insert_metadata_header(&mut headers, header::CONTENT_TYPE, content_type)?;
    }

    if let Some(e_tag) = &metadata.e_tag {
        insert_metadata_header(&mut headers, header::ETAG, e_tag)?;
    }

    if let Some(last_modified) = &metadata.last_modified {
        insert_metadata_header(&mut headers, header::LAST_MODIFIED, last_modified)?;
    }

    if let Some(content_encoding) = &metadata.content_encoding {
        insert_metadata_header(&mut headers, header::CONTENT_ENCODING, content_encoding)?;
    }

    Ok(headers)
}

#[axum::debug_handler]
#[instrument(skip(state), fields(bucket = %bucket, key = %key))]
async fn get_object(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<impl IntoResponse> {
    info!("Getting object {}/{}", bucket, key);
    
    // Check bucket access
    check_bucket_access(&state.config, &auth.username, &bucket)?;
    
    let (_, client) = state.get_account_and_client(&bucket)?;
    let body = client.get_object(&bucket, &key).await?;
    let bytes = body.collect().await.map_err(|e| AppError::InternalError(e.to_string()))?.to_vec();
    
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/octet-stream".parse().unwrap());
    
    Ok((StatusCode::OK, headers, bytes))
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
    body: Bytes,
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

    let body = ByteStream::from(body);
    
    client.put_object(&bucket, &key, body, content_type).await?;
    Ok(StatusCode::OK)
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
    let prefix = params.get("prefix").cloned();
    let objects = client.list_objects(&bucket, prefix.clone()).await?;

    let key_count = objects.len();
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Name>{name}</Name>
    <Prefix>{prefix}</Prefix>
    <KeyCount>{key_count}</KeyCount>
    <MaxKeys>{key_count}</MaxKeys>
    <IsTruncated>false</IsTruncated>
{contents}
</ListBucketResult>"#,
        name = escape_xml(&bucket),
        prefix = escape_xml(&prefix.unwrap_or_default()),
        key_count = key_count,
        contents = format_xml_content(&objects),
    );

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/xml".parse().unwrap());

    Ok((StatusCode::OK, headers, xml))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> Config {
        serde_json::from_str(
            r#"{
                "accounts": {},
                "users": {},
                "server": { "host": "127.0.0.1", "port": 8080 }
            }"#,
        )
        .expect("valid config")
    }

    #[tokio::test]
    async fn router_with_head_object_route_has_routes() {
        let router = create_router(AppState {
            config: Arc::new(make_config()),
            clients: HashMap::new(),
        })
        .await;

        assert!(router.has_routes());
    }

    #[tokio::test]
    async fn head_object_route_requires_auth() {
        use axum::body::Body;
        use axum::http::{Method, Request};
        use tower::Service;

        let mut router = create_router(AppState {
            config: Arc::new(make_config()),
            clients: HashMap::new(),
        })
        .await;

        let response = router
            .call(
                Request::builder()
                    .method(Method::HEAD)
                    .uri("/bucket/key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn head_object_headers_include_metadata_and_accept_ranges() {
        let headers = head_object_headers(&HeadObjectMetadata {
            content_length: Some(42),
            content_type: Some("text/plain".to_string()),
            e_tag: Some(r#""abc123""#.to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
            content_encoding: Some("gzip".to_string()),
        })
        .expect("valid headers");

        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(headers[header::CONTENT_LENGTH], "42");
        assert_eq!(headers[header::CONTENT_TYPE], "text/plain");
        assert_eq!(headers[header::ETAG], r#""abc123""#);
        assert_eq!(headers[header::LAST_MODIFIED], "Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(headers[header::CONTENT_ENCODING], "gzip");
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
        assert_eq!(escape_xml("plain/path/to/file.txt"), "plain/path/to/file.txt");
    }

    #[test]
    fn escape_xml_does_not_double_escape() {
        // Each special char should produce exactly one entity.
        assert_eq!(escape_xml("&amp;"), "&amp;amp;");
    }
}