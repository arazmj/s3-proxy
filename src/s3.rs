use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{
    config::Credentials,
    primitives::ByteStream,
    types::Object,
    Client,
    error::SdkError,
};
use tracing::{info, instrument};

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