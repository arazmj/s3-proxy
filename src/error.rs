use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::{
    get_object::GetObjectError, list_objects_v2::ListObjectsV2Error, put_object::PutObjectError,
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    // S3 operation errors
    #[error("S3 error: {0}")]
    S3Error(Box<aws_sdk_s3::Error>),

    #[error("S3 ListObjects error: {0}")]
    ListObjectsError(Box<SdkError<ListObjectsV2Error>>),

    #[error("S3 GetObject error: {0}")]
    GetObjectError(Box<SdkError<GetObjectError>>),

    #[error("S3 PutObject error: {0}")]
    PutObjectError(Box<SdkError<PutObjectError>>),

    // Resource not found errors
    #[error("Bucket not found: {0}")]
    BucketNotFound(String),

    #[error("Object not found: {0}/{1}")]
    ObjectNotFound(String, String),

    // System errors
    #[error("Configuration error: {0}")]
    ConfigError(Box<std::io::Error>),

    #[error("Internal server error: {0}")]
    InternalError(String),

    // Authentication and authorization errors
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Too many requests: {0}")]
    RateLimited(String),

    #[error("Requested range is not satisfiable")]
    RangeNotSatisfiable,

    #[error("Request body exceeds maximum allowed size of {0} bytes")]
    PayloadTooLarge(u64),
}

impl From<aws_sdk_s3::Error> for AppError {
    fn from(error: aws_sdk_s3::Error) -> Self {
        Self::S3Error(Box::new(error))
    }
}

impl From<SdkError<ListObjectsV2Error>> for AppError {
    fn from(error: SdkError<ListObjectsV2Error>) -> Self {
        Self::ListObjectsError(Box::new(error))
    }
}

impl From<SdkError<GetObjectError>> for AppError {
    fn from(error: SdkError<GetObjectError>) -> Self {
        Self::GetObjectError(Box::new(error))
    }
}

impl From<SdkError<PutObjectError>> for AppError {
    fn from(error: SdkError<PutObjectError>) -> Self {
        Self::PutObjectError(Box::new(error))
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::ConfigError(Box::new(error))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            // Not found errors
            AppError::BucketNotFound(bucket) => {
                (StatusCode::NOT_FOUND, format!("Bucket not found: {bucket}"))
            }
            AppError::ObjectNotFound(bucket, key) => (
                StatusCode::NOT_FOUND,
                format!("Object not found: {bucket}/{key}"),
            ),

            // S3 operation errors
            AppError::S3Error(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("S3 error: {e}")),
            AppError::ListObjectsError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("S3 ListObjects error: {e}"),
            ),
            AppError::GetObjectError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("S3 GetObject error: {e}"),
            ),
            AppError::PutObjectError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("S3 PutObject error: {e}"),
            ),

            // System errors
            AppError::ConfigError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Configuration error: {e}"),
            ),
            AppError::InternalError(e) => (StatusCode::INTERNAL_SERVER_ERROR, e),

            // Authentication and authorization errors
            AppError::Unauthorized(e) => (StatusCode::UNAUTHORIZED, e),
            AppError::Forbidden(e) => (StatusCode::FORBIDDEN, e),
            AppError::InvalidRequest(e) => (StatusCode::BAD_REQUEST, e),
            AppError::RateLimited(e) => (StatusCode::TOO_MANY_REQUESTS, e),
            AppError::RangeNotSatisfiable => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                "Requested range is not satisfiable".to_string(),
            ),
            AppError::PayloadTooLarge(limit) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("Request body exceeds maximum allowed size of {limit} bytes"),
            ),
        };

        let body = ErrorBody {
            error: &error_message,
            status: status.as_u16(),
        };
        // serde_json::to_string never fails for this struct, but fall back
        // to a static safe payload just in case.
        let body = serde_json::to_string(&body)
            .unwrap_or_else(|_| r#"{"error":"internal error","status":500}"#.to_string());

        let mut response = (status, body).into_response();
        response
            .headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        response
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    status: u16,
}

pub type Result<T> = std::result::Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::Value;

    async fn body_to_json(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).expect("response body must be valid JSON");
        (status, v)
    }

    #[tokio::test]
    async fn quote_in_message_does_not_break_json() {
        // A bucket name with a double-quote in it would previously produce
        // "{\"error\": \"Bucket not found: foo\"bar\", \"status\": 404}", which
        // is not parseable JSON. With proper serialization the quote is
        // escaped and the response stays valid.
        let err = AppError::BucketNotFound("foo\"bar".to_string());
        let (status, json) = body_to_json(err.into_response()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["status"], 404);
        assert_eq!(json["error"], "Bucket not found: foo\"bar");
    }

    #[tokio::test]
    async fn backslash_and_newline_in_message_are_escaped() {
        let err = AppError::InternalError("line1\nline2\\end".to_string());
        let (status, json) = body_to_json(err.into_response()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json["error"], "line1\nline2\\end");
    }

    #[tokio::test]
    async fn unauthorized_status_is_401() {
        let err = AppError::Unauthorized("nope".to_string());
        let (status, json) = body_to_json(err.into_response()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["status"], 401);
        assert_eq!(json["error"], "nope");
    }

    #[tokio::test]
    async fn forbidden_status_is_403() {
        let error = AppError::Forbidden("nope".to_string());
        let (status, json) = body_to_json(error.into_response()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json["status"], 403);
    }
}
