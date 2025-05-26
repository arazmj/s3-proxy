use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::{
    list_objects_v2::ListObjectsV2Error,
    get_object::GetObjectError,
    put_object::PutObjectError,
};

#[derive(Error, Debug)]
pub enum AppError {
    // S3 operation errors
    #[error("S3 error: {0}")]
    S3Error(#[from] aws_sdk_s3::Error),
    
    #[error("S3 ListObjects error: {0}")]
    ListObjectsError(#[from] SdkError<ListObjectsV2Error>),
    
    #[error("S3 GetObject error: {0}")]
    GetObjectError(#[from] SdkError<GetObjectError>),
    
    #[error("S3 PutObject error: {0}")]
    PutObjectError(#[from] SdkError<PutObjectError>),
    
    // Resource not found errors
    #[error("Bucket not found: {0}")]
    BucketNotFound(String),
    
    #[error("Object not found: {0}/{1}")]
    ObjectNotFound(String, String),
    
    // System errors
    #[error("Configuration error: {0}")]
    ConfigError(#[from] std::io::Error),
    
    #[error("Internal server error: {0}")]
    InternalError(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            // Not found errors
            AppError::BucketNotFound(bucket) => (
                StatusCode::NOT_FOUND,
                format!("Bucket not found: {}", bucket)
            ),
            AppError::ObjectNotFound(bucket, key) => (
                StatusCode::NOT_FOUND,
                format!("Object not found: {}/{}", bucket, key)
            ),
            
            // S3 operation errors
            AppError::S3Error(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("S3 error: {}", e)
            ),
            AppError::ListObjectsError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("S3 ListObjects error: {}", e)
            ),
            AppError::GetObjectError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("S3 GetObject error: {}", e)
            ),
            AppError::PutObjectError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("S3 PutObject error: {}", e)
            ),
            
            // System errors
            AppError::ConfigError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Configuration error: {}", e)
            ),
            AppError::InternalError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                e
            ),
        };

        let body = format!(
            r#"{{"error": "{}", "status": {}}}"#,
            error_message,
            status.as_u16()
        );

        let mut response = (status, body).into_response();
        response.headers_mut().insert(
            "content-type",
            "application/json".parse().unwrap()
        );
        response
    }
}

pub type Result<T> = std::result::Result<T, AppError>; 