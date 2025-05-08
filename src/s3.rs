use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{
    config::Credentials,
    primitives::ByteStream,
    types::Object,
    Client,
};

pub struct S3Client {
    client: Client,
}

impl S3Client {
    pub async fn new(
        endpoint_url: String,
        region: String,
        access_key_id: String,
        secret_access_key: String,
    ) -> Result<Self, anyhow::Error> {
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

    pub async fn list_objects(&self, bucket: &str, prefix: Option<String>) -> Result<Vec<Object>, anyhow::Error> {
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

        Ok(objects)
    }

    pub async fn get_object(&self, bucket: &str, key: &str) -> Result<ByteStream, anyhow::Error> {
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await?;

        Ok(response.body)
    }

    pub async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: ByteStream,
        content_type: Option<String>,
    ) -> Result<(), anyhow::Error> {
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
        Ok(())
    }
} 