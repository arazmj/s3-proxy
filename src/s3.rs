use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{
    config::Credentials,
    primitives::{ByteStream, DateTimeFormat},
    types::Object,
    Client,
    error::SdkError,
};
use tracing::{info, instrument};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadObjectMetadata {
    pub content_length: Option<i64>,
    pub content_type: Option<String>,
    pub e_tag: Option<String>,
    pub last_modified: Option<String>,
    pub content_encoding: Option<String>,
}

use crate::error::{AppError, Result};

pub struct S3Client {
    client: Client,
}

impl S3Client {
    #[instrument(skip(endpoint_url, region, access_key_id, secret_access_key))]
    pub async fn new(
        endpoint_url: String,
        region: String,
        access_key_id: String,
        secret_access_key: String,
    ) -> Result<Self> {
        info!("Creating new S3 client for endpoint {}", endpoint_url);
        
        let config = aws_config::defaults(BehaviorVersion::latest())
            .endpoint_url(endpoint_url)
            .region(Region::new(region))
            .credentials_provider(Credentials::new(
                access_key_id,
                secret_access_key,
                None,
                None,
                "s3-proxy",
            ))
            .load()
            .await;

        let client = Client::new(&config);
        Ok(Self { client })
    }

    #[instrument(skip(self), fields(bucket = %bucket))]
    pub async fn list_objects(&self, bucket: &str, prefix: Option<String>) -> Result<Vec<Object>> {
        info!("Listing objects in bucket {} with prefix {:?}", bucket, prefix);
        
        let mut objects = Vec::new();
        let mut continuation_token = None;

        loop {
            let response = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .set_prefix(prefix.clone())
                .set_continuation_token(continuation_token)
                .send()
                .await?;

            if let Some(contents) = response.contents {
                objects.extend(contents);
            }

            continuation_token = response.next_continuation_token;
            if continuation_token.is_none() {
                break;
            }
        }

        info!("Found {} objects in bucket {}", objects.len(), bucket);
        Ok(objects)
    }

    #[instrument(skip(self), fields(bucket = %bucket, key = %key))]
    pub async fn get_object(&self, bucket: &str, key: &str) -> Result<ByteStream> {
        info!("Getting object {}/{}", bucket, key);
        
        match self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(response) => Ok(response.body),
            Err(e) => {
                if let SdkError::ServiceError(context) = &e {
                    if context.err().is_no_such_key() {
                        return Err(AppError::ObjectNotFound(bucket.to_string(), key.to_string()));
                    }
                }
                Err(e.into())
            }
        }
    }

    #[instrument(skip(self), fields(bucket = %bucket, key = %key))]
    pub async fn head_object(&self, bucket: &str, key: &str) -> Result<HeadObjectMetadata> {
        info!("Getting object metadata {}/{}", bucket, key);

        match self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(response) => Ok(HeadObjectMetadata {
                content_length: response.content_length(),
                content_type: response.content_type().map(String::from),
                e_tag: response.e_tag().map(String::from),
                last_modified: response
                    .last_modified()
                    .and_then(|dt| dt.fmt(DateTimeFormat::HttpDate).ok()),
                content_encoding: response.content_encoding().map(String::from),
            }),
            Err(e) => {
                if let SdkError::ServiceError(context) = &e {
                    if context.err().is_not_found() {
                        return Err(AppError::ObjectNotFound(bucket.to_string(), key.to_string()));
                    }
                }
                Err(AppError::InternalError(format!("S3 HeadObject error: {}", e)))
            }
        }
    }

    #[instrument(skip(self, body), fields(bucket = %bucket, key = %key))]
    pub async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: ByteStream,
        content_type: Option<String>,
    ) -> Result<()> {
        info!("Putting object {}/{}", bucket, key);
        
        let mut request = self
            .client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body);

        if let Some(content_type) = content_type {
            request = request.content_type(content_type);
        }

        request.send().await?;
        info!("Successfully put object {}/{}", bucket, key);
        Ok(())
    }
} 