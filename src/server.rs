use aws_sdk_s3::{operation::get_object::GetObjectOutput, primitives::ByteStream};
use axum::{
    body::{Body, Bytes},
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, put},
    Router,
};
use futures::stream;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, instrument};

use crate::auth::{auth_middleware, check_bucket_access, check_write_permission, AuthState};
use crate::config::Config;
use crate::error::{AppError, Result};
use crate::s3::S3Client;

pub struct AppState {
    pub config: Arc<Config>,
    pub clients: HashMap<String, Arc<S3Client>>,
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
    let is_range_request = range.is_some();
    let (_, client) = state.get_account_and_client(&bucket)?;
    let response = client.get_object(&bucket, &key, range).await?;
    let headers = get_object_headers(&response);
    let body_stream = stream::unfold(response.body, |mut byte_stream| async {
        match byte_stream.try_next().await {
            Ok(Some(bytes)) => Some((Ok::<Bytes, std::io::Error>(bytes), byte_stream)),
            Ok(None) => None,
            Err(error) => Some((Err(std::io::Error::other(error)), byte_stream)),
        }
    });
    let body = Body::from_stream(body_stream);

    let status = if is_range_request {
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
