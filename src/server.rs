use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
    Router,
};
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use aws_sdk_s3::primitives::ByteStream;

use crate::config::Config;
use crate::s3::S3Client;

pub struct AppState {
    pub config: Arc<Config>,
    pub clients: HashMap<String, Arc<S3Client>>,
}

pub async fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/:bucket/*key", get(get_object))
        .route("/:bucket/*key", put(put_object))
        .route("/:bucket", get(list_objects))
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(state))
}

async fn get_object(
    State(state): State<Arc<AppState>>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    // Find the account for this bucket
    let (account_id, _account_config) = match state.config.find_account_for_bucket(&bucket) {
        Some(account) => account,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Get or create the S3 client for this account
    let client = state.clients.get(account_id).unwrap();

    // Get the object
    match client.get_object(&bucket, &key).await {
        Ok(body) => {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", "application/octet-stream".parse().unwrap());
            let bytes = body.collect().await.unwrap().to_vec();
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn put_object(
    State(state): State<Arc<AppState>>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Find the account for this bucket
    let (account_id, _account_config) = match state.config.find_account_for_bucket(&bucket) {
        Some(account) => account,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Get or create the S3 client for this account
    let client = state.clients.get(account_id).unwrap();

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let body = ByteStream::from(body);
    
    match client.put_object(&bucket, &key, body, content_type).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn list_objects(
    State(state): State<Arc<AppState>>,
    Path(bucket): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Find the account for this bucket
    let (account_id, _account_config) = match state.config.find_account_for_bucket(&bucket) {
        Some(account) => account,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Get or create the S3 client for this account
    let client = state.clients.get(account_id).unwrap();

    let prefix = params.get("prefix").cloned();
    
    // List objects from the account
    match client.list_objects(&bucket, prefix.clone()).await {
        Ok(objects) => {
            // Convert to XML response
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
    <Name>{}</Name>
    <Prefix>{}</Prefix>
    <Contents>
        {}
    </Contents>
</ListBucketResult>"#,
                bucket,
                prefix.unwrap_or_default(),
                objects
                    .iter()
                    .map(|obj| format!(
                        r#"<Key>{}</Key><Size>{}</Size><LastModified>{}</LastModified>"#,
                        obj.key().unwrap_or_default(),
                        obj.size().unwrap_or(0),
                        obj.last_modified().map(|dt| dt.to_string()).unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            
            let mut headers = HeaderMap::new();
            headers.insert("content-type", "application/xml".parse().unwrap());
            
            (StatusCode::OK, headers, xml).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
} 