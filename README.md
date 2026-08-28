# S3 Proxy

A small Rust-based, S3-compatible HTTP proxy. It sits in front of one or more
S3 backends (AWS S3, MinIO, etc.), authenticates clients with an API key,
enforces per-user bucket allow-lists and read/write roles, and forwards the
request to whichever backend account owns the target bucket.

The proxy is **not** a caching layer or a multi-source aggregator — each
bucket name maps to exactly one configured backend account.

## Features

- S3-compatible HTTP endpoints: `GET /{bucket}`, `GET /{bucket}/{key}`,
  `PUT /{bucket}/{key}` (nested keys with `/` are supported).
- Multi-backend routing: configure several S3 accounts (each with its own
  endpoint, region, and credentials) and a list of buckets each owns; the
  proxy dispatches requests to the right one based on the bucket in the URL.
- API-key authentication via the `x-api-key` header.
- Per-user bucket allow-lists (`["bucket1", "bucket2"]` or `["*"]`).
- Three roles: `admin`, `user`, `readonly`. `admin` and `user` may write;
  `readonly` is GET-only.
- Per-user sliding-window rate limiting (default 100 req/min).
- Configurable max upload size (default 100 MiB).
- Path-traversal defense in the request validator (`.`, `..`, `//`,
  trailing `/` are rejected).
- Security response headers (`X-Content-Type-Options`, `X-Frame-Options`,
  `Strict-Transport-Security`).
- JSON error responses (`{"error": "...", "status": <code>}`) and
  S3-compliant XML for `ListObjects`.

## Building

```bash
cargo build --release
```

## Configuration

The proxy reads `config.json` from the current working directory at startup.

### Schema

```jsonc
{
  // Backend S3 accounts. The key (e.g. "minio") is an internal account id
  // used only in logs; the proxy picks an account by matching the bucket in
  // the URL against each account's "buckets" list.
  "accounts": {
    "<account-id>": {
      "endpoint_url":      "http://host:port",   // S3 endpoint (any S3-compatible service)
      "region":            "us-east-1",
      "access_key_id":     "...",
      "secret_access_key": "...",
      "buckets":           ["bucket1", "bucket2"] // buckets this account owns
    }
  },

  // API consumers. The key (e.g. "admin") is the username surfaced in logs.
  "users": {
    "<username>": {
      "api_key":         "secret-string",        // value the client sends in x-api-key
      "role":            "admin",                // "admin" | "user" | "readonly"
      "allowed_buckets": ["bucket1"]             // or ["*"] for any bucket
    }
  },

  // HTTP listener.
  "server": {
    "host": "127.0.0.1",
    "port": 8080
  },

  // Optional. Maximum body size accepted on PUT. Default: 104857600 (100 MiB).
  "max_file_size": 104857600,

  // Optional. Per-user sliding-window request limit.
  "rate_limit": {
    "max_requests": 100,
    "window_secs": 60
  }
}
```

A working example for a local MinIO instance ships in [`config.json`](config.json).

### Roles

| Role       | GET | PUT |
|------------|:---:|:---:|
| `admin`    |  ✅ |  ✅ |
| `user`     |  ✅ |  ✅ |
| `readonly` |  ✅ |  ❌ |

A user is restricted to the buckets named in `allowed_buckets`; use `["*"]`
to grant access to every bucket the proxy knows about.

## Running

```bash
RUST_LOG=info ./target/release/s3-proxy
```

Logs are emitted via `tracing-subscriber` and respect the standard
`RUST_LOG` env-filter syntax (e.g. `RUST_LOG=s3_proxy=debug,info`).
The `x-api-key` header is automatically redacted from request logs.

## API

| Method | Path                  | Description                                                  |
|--------|-----------------------|--------------------------------------------------------------|
| GET    | `/{bucket}`           | List objects with S3 V2 pagination query parameters.        |
| GET    | `/{bucket}/{key}`     | Stream an object. Supports single `Range: bytes=…` requests. |
| HEAD   | `/{bucket}/{key}`     | Return object metadata without downloading the object.      |
| PUT    | `/{bucket}/{key}`     | Upload an object. Body is forwarded verbatim.                |
| DELETE | `/{bucket}/{key}`     | Delete an object (writers only).                             |
| GET    | `/livez`              | Unauthenticated process liveness check.                     |
| GET    | `/readyz`             | Unauthenticated configuration/client readiness check.       |

All requests must include `x-api-key: <value>`; otherwise the proxy responds
with `401 Unauthorized`.

### Status codes

| Code | Meaning                                                       |
|-----:|---------------------------------------------------------------|
|  200 | Success.                                                      |
|  400 | Malformed path (e.g. `..`, `//`, trailing `/`).                 |
|  401 | Missing/invalid API key or bucket not in user's allow-list.    |
|  403 | Authenticated user lacks write permission.                     |
|  413 | Upload exceeds `max_file_size`.                               |
|  429 | Per-user request limit exceeded.                              |
|  503 | Readiness checks fail until backend clients are initialized.  |
|  404 | Bucket or object not found.                                   |
|  500 | Internal / upstream S3 error.                                 |

Error bodies are always JSON:

```json
{ "error": "Object not found: bucket1/missing.txt", "status": 404 }
```

## Usage with S3 clients

The endpoint is S3-compatible, so any S3 SDK works. Note that the proxy
authenticates by `x-api-key`, **not** SigV4, so most CLIs need to be told
to send the header explicitly. With `curl`:

```bash
# List
curl -H 'x-api-key: admin-secret-key' \
     'http://localhost:8080/bucket1?prefix=logs/'

# Get
curl -H 'x-api-key: admin-secret-key' \
     http://localhost:8080/bucket1/path/to/file.txt -o file.txt

# Put
curl -H 'x-api-key: admin-secret-key' \
     -T ./file.txt \
     http://localhost:8080/bucket1/path/to/file.txt
```

## Local development / smoke test

[`test.sh`](test.sh) spins up a MinIO container, creates the buckets used in
the bundled `config.json`, and exercises admin / user / readonly access plus
rate limiting against a locally running proxy. Start the proxy in one
terminal (`cargo run`) and then run `./test.sh`.

## Tests

```bash
cargo test
```

## License

MIT
