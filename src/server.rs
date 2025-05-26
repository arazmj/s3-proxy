use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, put},
    Router,
};
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use aws_sdk_s3::primitives::ByteStream;
use tracing::{info, instrument};

use crate::config::Config;
use crate::s3::S3Client;
use crate::error::{AppError, Result};

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
        .route("/:bucket", get(list_objects))
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(state))
}

#[instrument(skip(state), fields(bucket = %bucket, key = %key))]
async fn get_object(
    State(state): State<Arc<AppState>>,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<impl IntoResponse> {
    info!("Getting object {}/{}", bucket, key);
    
    let (_, client) = state.get_account_and_client(&bucket)?;
    let body = client.get_object(&bucket, &key).await?;
    let bytes = body.collect().await.map_err(|e| AppError::InternalError(e.to_string()))?.to_vec();
    
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/octet-stream".parse().unwrap());
    
    Ok((StatusCode::OK, headers, bytes))
}

#[instrument(skip(state, body), fields(bucket = %bucket, key = %key))]
async fn put_object(
    State(state): State<Arc<AppState>>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse> {
    info!("Putting object {}/{}", bucket, key);
    
    let (_, client) = state.get_account_and_client(&bucket)?;

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let body = ByteStream::from(body);
    
    client.put_object(&bucket, &key, body, content_type).await?;
    Ok(StatusCode::OK)
}

fn format_xml_content(objects: &[aws_sdk_s3::types::Object]) -> String {
    objects
        .iter()
        .map(|obj| {
            format!(
                r#"        <Contents>
            <Key>{}</Key>
            <Size>{}</Size>
            <LastModified>{}</LastModified>
        </Contents>"#,
                obj.key().unwrap_or_default(),
                obj.size().unwrap_or(0),
                obj.last_modified().map(|dt| dt.to_string()).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[instrument(skip(state), fields(bucket = %bucket))]
async fn list_objects(
    State(state): State<Arc<AppState>>,
    Path(bucket): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse> {
    info!("Listing objects in bucket {}", bucket);
    
    let (_, client) = state.get_account_and_client(&bucket)?;
    let prefix = params.get("prefix").cloned();
    let objects = client.list_objects(&bucket, prefix.clone()).await?;
    
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
    <Name>{}</Name>
    <Prefix>{}</Prefix>
{}
</ListBucketResult>"#,
        bucket,
        prefix.unwrap_or_default(),
        format_xml_content(&objects)
    );
    
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/xml".parse().unwrap());
    
    Ok((StatusCode::OK, headers, xml))
} 