use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{
    config::Credentials, error::SdkError, operation::get_object::GetObjectOutput,
    primitives::ByteStream, types::Object, Client,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadObjectMetadata {
    pub content_length: Option<i64>,
    pub content_type: Option<String>,
    pub e_tag: Option<String>,
    pub last_modified: Option<String>,
    pub content_encoding: Option<String>,
    pub cache_control: Option<String>,
    pub content_disposition: Option<String>,
    pub content_language: Option<String>,
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
    pub async fn list_objects(
        &self,
        bucket: &str,
        params: ListObjectsParams,
    ) -> Result<ListObjectsPage> {
        info!(
            "Listing objects in bucket {} with prefix {:?}, start-after {:?}, continuation-token {:?}, max-keys {}",
            bucket,
            params.prefix,
            params.start_after,
            params.continuation_token,
            params.max_keys
        );

        let response = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .set_prefix(params.prefix)
            .set_start_after(params.start_after)
            .set_continuation_token(params.continuation_token)
            .set_max_keys(Some(params.max_keys))
            .send()
            .await?;

        let objects = response.contents.unwrap_or_default();
        let page = ListObjectsPage {
            key_count: response.key_count.unwrap_or(objects.len() as i32),
            objects,
            is_truncated: response.is_truncated.unwrap_or(false),
            next_continuation_token: response.next_continuation_token,
        };

        info!(
            "Found {} objects in bucket {} (is_truncated: {})",
            page.objects.len(),
            bucket,
            page.is_truncated
        );
        Ok(page)
    }

    #[instrument(skip(self), fields(bucket = %bucket, key = %key, range = ?range))]
    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        range: Option<String>,
    ) -> Result<GetObjectOutput> {
        info!("Getting object {}/{} with range {:?}", bucket, key, range);

        let mut request = self.client.get_object().bucket(bucket).key(key);
        if let Some(range) = range {
            request = request.range(range);
        }

        match request.send().await {
            Ok(response) => Ok(response),
            Err(e) => {
                if let SdkError::ServiceError(context) = &e {
                    if context.err().is_no_such_key() {
                        return Err(AppError::ObjectNotFound(
                            bucket.to_string(),
                            key.to_string(),
                        ));
                    }
                    if context.raw().status().as_u16() == 416 {
                        return Err(AppError::RangeNotSatisfiable);
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
                last_modified: response.last_modified().and_then(|date| {
                    date.fmt(aws_sdk_s3::primitives::DateTimeFormat::HttpDate)
                        .ok()
                }),
                content_encoding: response.content_encoding().map(String::from),
                cache_control: response.cache_control().map(String::from),
                content_disposition: response.content_disposition().map(String::from),
                content_language: response.content_language().map(String::from),
            }),
            Err(error) => {
                if let SdkError::ServiceError(context) = &error {
                    if context.err().is_not_found() {
                        return Err(AppError::ObjectNotFound(
                            bucket.to_string(),
                            key.to_string(),
                        ));
                    }
                }
                Err(AppError::InternalError(format!(
                    "S3 HeadObject error: {error}"
                )))
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

        let mut request = self.client.put_object().bucket(bucket).key(key).body(body);

        if let Some(content_type) = content_type {
            request = request.content_type(content_type);
        }

        request.send().await?;
        info!("Successfully put object {}/{}", bucket, key);
        Ok(())
    }
}
