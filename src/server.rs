use axum::{
    body::Bytes,
    extract::{Path, Query, State, Extension},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, put},
    Router,
};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use aws_sdk_s3::primitives::ByteStream;
use tracing::{info, instrument};

use crate::config::Config;
use crate::s3::{ListObjectsParams, S3Client};
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
    max_keys: i32,
    objects: &'a [aws_sdk_s3::types::Object],
    is_truncated: bool,
    next_continuation_token: Option<&'a str>,
    key_count: i32,
}

fn parse_list_objects_params(params: &HashMap<String, String>) -> Result<ListObjectsParams> {
    let max_keys = match params.get("max-keys") {
        Some(value) => {
            let parsed = i32::from_str(value)
                .map_err(|_| AppError::InvalidRequest("max-keys must be an integer".to_string()))?;
            if parsed < 1 {
                return Err(AppError::InvalidRequest("max-keys must be at least 1".to_string()));
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

fn format_list_objects_xml(view: &ListObjectsResponseView<'_>) -> String {
    let start_after = view.start_after.map(|value| {
        format!("    <StartAfter>{}</StartAfter>\n", escape_xml(value))
    }).unwrap_or_default();
    let next_continuation_token = view.next_continuation_token.map(|value| {
        format!("    <NextContinuationToken>{}</NextContinuationToken>\n", escape_xml(value))
    }).unwrap_or_default();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Name>{name}</Name>
    <Prefix>{prefix}</Prefix>
{start_after}    <KeyCount>{key_count}</KeyCount>
    <MaxKeys>{max_keys}</MaxKeys>
    <IsTruncated>{is_truncated}</IsTruncated>
{next_continuation_token}{contents}
</ListBucketResult>"#,
        name = escape_xml(view.bucket),
        prefix = escape_xml(view.prefix.unwrap_or_default()),
        start_after = start_after,
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
    let max_keys = list_params.max_keys;
    let prefix = list_params.prefix.clone();
    let start_after = list_params.start_after.clone();
    let page = client.list_objects(&bucket, list_params).await?;

    let xml = format_list_objects_xml(&ListObjectsResponseView {
        bucket: &bucket,
        prefix: prefix.as_deref(),
        start_after: start_after.as_deref(),
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

    #[test]
    fn max_keys_zero_returns_invalid_request() {
        let params = HashMap::from([("max-keys".to_string(), "0".to_string())]);
        let err = parse_list_objects_params(&params).unwrap_err();
        assert!(err.to_string().contains("Invalid request"));

        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn list_objects_xml_includes_next_continuation_token_when_present() {
        let xml = format_list_objects_xml(&ListObjectsResponseView {
            bucket: "bucket",
            prefix: None,
            start_after: None,
            max_keys: 1000,
            objects: &[],
            is_truncated: true,
            next_continuation_token: Some("next&token"),
            key_count: 0,
        });

        assert!(xml.contains("<NextContinuationToken>next&amp;token</NextContinuationToken>"));
    }

    #[test]
    fn list_objects_xml_omits_next_continuation_token_when_absent() {
        let xml = format_list_objects_xml(&ListObjectsResponseView {
            bucket: "bucket",
            prefix: None,
            start_after: None,
            max_keys: 1000,
            objects: &[],
            is_truncated: false,
            next_continuation_token: None,
            key_count: 0,
        });

        assert!(!xml.contains("NextContinuationToken"));
    }

    #[test]
    fn list_objects_xml_renders_max_keys_and_key_count_separately() {
        let xml = format_list_objects_xml(&ListObjectsResponseView {
            bucket: "bucket",
            prefix: None,
            start_after: Some("after<key"),
            max_keys: 1000,
            objects: &[],
            is_truncated: false,
            next_continuation_token: None,
            key_count: 2,
        });

        assert!(xml.contains("<MaxKeys>1000</MaxKeys>"));
        assert!(xml.contains("<KeyCount>2</KeyCount>"));
        assert!(xml.contains("<StartAfter>after&lt;key</StartAfter>"));
    }
}