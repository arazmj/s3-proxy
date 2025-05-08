# S3 Proxy

A Rust-based S3 proxy server that aggregates multiple S3 data sources into a single S3-compatible endpoint.

## Features

- S3-compatible API endpoints
- Aggregates multiple S3 sources
- Caches objects in a target S3 bucket
- Supports standard S3 operations (GET, PUT, LIST)
- Configurable through a configuration file

## Building

```bash
cargo build --release
```

## Configuration

The proxy is configured through a JSON configuration file. Here's an example configuration:

```json
{
  "sources": {
    "source1": {
      "endpoint_url": "http://source1:9000",
      "region": "us-east-1",
      "access_key_id": "minioadmin",
      "secret_access_key": "minioadmin",
      "bucket": "source1-bucket",
      "prefix": "optional/prefix"
    },
    "source2": {
      "endpoint_url": "http://source2:9000",
      "region": "us-east-1",
      "access_key_id": "minioadmin",
      "secret_access_key": "minioadmin",
      "bucket": "source2-bucket",
      "prefix": null
    }
  },
  "target": {
    "endpoint_url": "http://target:9000",
    "region": "us-east-1",
    "access_key_id": "minioadmin",
    "secret_access_key": "minioadmin",
    "bucket": "target-bucket",
    "prefix": null
  },
  "port": 8080
}
```

## Running

```bash
RUST_LOG=info ./target/release/s3-proxy
```

## API Endpoints

The proxy implements the following S3-compatible endpoints:

- `GET /{bucket}?prefix={prefix}` - List objects in a bucket
- `GET /{bucket}/{key}` - Get an object
- `PUT /{bucket}/{key}` - Put an object

## Usage with S3 Clients

The proxy is compatible with any S3 client. Here's an example using the AWS CLI:

```bash
# List objects
aws --endpoint-url http://localhost:8080 s3 ls s3://my-bucket/

# Get an object
aws --endpoint-url http://localhost:8080 s3 cp s3://my-bucket/my-object.txt .

# Put an object
aws --endpoint-url http://localhost:8080 s3 cp my-object.txt s3://my-bucket/
```

## License

MIT 