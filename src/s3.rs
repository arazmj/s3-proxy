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

#[derive(Debug)]
pub struct ListObjectsParams {
    pub prefix: Option<String>,
    pub start_after: Option<String>,
    pub continuation_token: Option<String>,
    pub max_keys: i32,
}

#[derive(Debug)]
pub struct ListObjectsPage {
    pub objects: Vec<Object>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
    pub key_count: i32,
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

    #[instrument(skip(self, params), fields(bucket = %bucket))]
    pub async fn list_objects(&self, bucket: &str, params: ListObjectsParams) -> Result<ListObjectsPage> {
        let max_keys = match params.max_keys {
            1..=1000 => params.max_keys,
            n if n > 1000 => 1000,
            _ => return Err(AppError::InvalidRequest("max-keys must be at least 1".to_string())),
        };

        info!(
            "Listing objects in bucket {} with prefix {:?}, start-after {:?}, continuation-token {:?}, max-keys {}",
            bucket, params.prefix, params.start_after, params.continuation_token, max_keys
        );

        let response = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .set_prefix(params.prefix)
            .set_start_after(params.start_after)
            .set_continuation_token(params.continuation_token)
            .set_max_keys(Some(max_keys))
            .send()
            .await?;

        let objects = response.contents.unwrap_or_default();
        let key_count = response.key_count.unwrap_or(objects.len() as i32);
        let page = ListObjectsPage {
            objects,
            is_truncated: response.is_truncated.unwrap_or(false),
            next_continuation_token: response.next_continuation_token,
            key_count,
        };

        info!(
            "Found {} objects in bucket {} (is_truncated: {})",
            page.objects.len(), bucket, page.is_truncated
        );
        Ok(page)
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