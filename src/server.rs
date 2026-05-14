use axum::{
    body::Bytes,
    extract::{Path, Query, State, Extension},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, put},
    Router,
};
use std::collections::HashMap;
use std::sync::Arc;
use aws_sdk_s3::primitives::ByteStream;
use tracing::{info, instrument};

use crate::config::Config;
use crate::s3::S3Client;
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
        .route("/:bucket/*key", get(get_object))
        .route("/:bucket/*key", put(put_object))
        .route("/:bucket/*key", delete(delete_object))
        .route("/:bucket", get(list_objects))
        .layer(axum::middleware::from_fn_with_state(
            state.config.clone(),
            auth_middleware,
        ))
        .with_state(Arc::new(state))
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
    use axum::body::Body;
    use futures::future::poll_fn;
    use http::{Method, Request};
    use tower::Service;

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

    fn test_state() -> AppState {
        let json = r#"{
            "accounts": {
                "test-account": {
                    "endpoint_url": "http://localhost:9000",
                    "region": "us-east-1",
                    "access_key_id": "access",
                    "secret_access_key": "secret",
                    "buckets": ["bucket"]
                }
            },
            "users": {
                "writer": {
                    "api_key": "write-key",
                    "role": "user",
                    "allowed_buckets": ["bucket"]
                },
                "reader": {
                    "api_key": "read-key",
                    "role": "readonly",
                    "allowed_buckets": ["bucket"]
                }
            },
            "server": { "host": "127.0.0.1", "port": 8080 }
        }"#;

        AppState {
            config: Arc::new(serde_json::from_str(json).expect("valid config")),
            clients: HashMap::new(),
        }
    }

    async fn send_delete(api_key: &str) -> StatusCode {
        let mut app = create_router(test_state()).await;
        let request = Request::builder()
            .method(Method::DELETE)
            .uri("/bucket/path/to/object.txt")
            .header("x-api-key", api_key)
            .body(Body::empty())
            .unwrap();

        poll_fn(|cx| <Router as Service<Request<Body>>>::poll_ready(&mut app, cx))
            .await
            .unwrap();
        <Router as Service<Request<Body>>>::call(&mut app, request)
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn delete_route_is_wired_and_accepted() {
        assert_eq!(send_delete("write-key").await, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn readonly_user_gets_forbidden_on_delete() {
        assert_eq!(send_delete("read-key").await, StatusCode::FORBIDDEN);
    }
}