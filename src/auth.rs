use axum::{
    extract::Request, extract::State, http::header, middleware::Next, response::IntoResponse,
    response::Response,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::config::{Config, RateLimitConfig};
use crate::error::{AppError, Result};

#[derive(Debug, Clone)]
pub struct AuthState {
    pub username: String,
    #[allow(dead_code)]
    pub role: String,
}

#[derive(Default)]
struct RateLimiter {
    requests: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            requests: HashMap::new(),
        }
    }

    /// Returns `true` when `username` has already made `cfg.max_requests`
    /// requests within the trailing `cfg.window_secs` seconds.
    ///
    /// A `max_requests` of `0` disables the limiter entirely.
    fn is_rate_limited(&mut self, username: &str, cfg: RateLimitConfig) -> bool {
        if cfg.max_requests == 0 {
            return false;
        }

        let now = Instant::now();
        let window = Duration::from_secs(cfg.window_secs);
        let max_requests = cfg.max_requests as usize;

        // Clean up users whose entire window has expired so the map doesn't
        // accumulate stale `Vec<Instant>` allocations for one-shot callers.
        self.requests
            .retain(|_, times| times.iter().any(|&t| now.duration_since(t) < window));

        let requests = self.requests.entry(username.to_string()).or_default();
        requests.retain(|&time| now.duration_since(time) < window);

        if requests.len() >= max_requests {
            return true;
        }

        requests.push(now);
        false
    }
}

// Global rate-limiter state. A `std::sync::Mutex` is fine here because the
// critical section is short, synchronous, and never `.await`s — using
// `tokio::sync::RwLock` (which we did before) was misleading since every call
// took the write lock anyway.
fn rate_limiter() -> &'static Mutex<RateLimiter> {
    use std::sync::OnceLock;
    static INSTANCE: OnceLock<Mutex<RateLimiter>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(RateLimiter::new()))
}

fn add_secure_headers(headers: &mut http::HeaderMap) {
    headers.insert("X-Content-Type-Options", "nosniff".parse().unwrap());
    headers.insert("X-Frame-Options", "DENY".parse().unwrap());
    headers.insert(
        "Strict-Transport-Security",
        "max-age=63072000; includeSubDomains; preload"
            .parse()
            .unwrap(),
    );
    headers.insert(
        "Content-Security-Policy",
        "default-src 'none'; frame-ancestors 'none'"
            .parse()
            .unwrap(),
    );
    headers.insert("Referrer-Policy", "no-referrer".parse().unwrap());
}

fn validate_request(config: &Config, request: &Request) -> Result<()> {
    // Check content length for PUT requests
    if request.method() == http::Method::PUT {
        // Check write permissions
        if let Some(api_key) = request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
        {
            if let Some((username, _)) = config.find_user_by_api_key(api_key) {
                check_write_permission(config, username)?;
            }
        }

        if let Some(content_length) = request.headers().get(header::CONTENT_LENGTH) {
            if let Ok(s) = content_length.to_str() {
                if let Ok(length) = s.parse::<u64>() {
                    if length > config.max_file_size {
                        return Err(AppError::InvalidRequest(format!(
                            "File size {} exceeds maximum allowed size of {} bytes",
                            length, config.max_file_size
                        )));
                    }
                }
            }
        }
    }

    // Validate path components.
    //
    // Routes are `/<bucket>` (list) and `/<bucket>/<key>` where `<key>` is
    // matched by the `*key` wildcard and may itself contain `/` (e.g.
    // `/mybucket/path/to/file.txt`). We therefore allow any number of
    // segments after the bucket, but still reject:
    //   - empty paths
    //   - empty bucket name
    //   - empty intermediate/trailing segments (e.g. `//`, trailing `/`)
    //   - `.` / `..` segments (defense in depth against path traversal)
    if let Some(path) = request.uri().path().strip_prefix('/') {
        if path.is_empty() {
            return Err(AppError::InvalidRequest("Invalid path format".to_string()));
        }
        let parts: Vec<&str> = path.split('/').collect();
        let bucket = parts[0];
        if bucket.is_empty() {
            return Err(AppError::InvalidRequest("Invalid path format".to_string()));
        }
        for segment in &parts[1..] {
            if segment.is_empty() || *segment == "." || *segment == ".." {
                return Err(AppError::InvalidRequest("Invalid path format".to_string()));
            }
        }
    } else {
        return Err(AppError::InvalidRequest("Invalid path format".to_string()));
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use axum::body::Body;
    use std::collections::HashMap;

    fn make_config() -> Config {
        // Build a minimal config by deserializing JSON to avoid relying on
        // private struct construction.
        let json = r#"{
            "accounts": {},
            "users": {},
            "server": { "host": "127.0.0.1", "port": 8080 }
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("valid config");
        // Sanity: defaults applied.
        let _ = cfg.max_file_size;
        cfg
    }

    fn rate_cfg(max_requests: u32, window_secs: u64) -> RateLimitConfig {
        RateLimitConfig {
            max_requests,
            window_secs,
        }
    }

    fn req(method: http::Method, path: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn list_bucket_path_is_valid() {
        let cfg = make_config();
        validate_request(&cfg, &req(http::Method::GET, "/mybucket")).expect("should accept");
    }

    #[test]
    fn flat_object_key_is_valid() {
        let cfg = make_config();
        validate_request(&cfg, &req(http::Method::GET, "/mybucket/file.txt"))
            .expect("should accept");
    }

    #[test]
    fn nested_object_key_is_valid() {
        // Regression: previously rejected with "Invalid path format" because
        // the path has more than two `/`-separated segments.
        let cfg = make_config();
        validate_request(
            &cfg,
            &req(
                http::Method::GET,
                "/mybucket/path/to/deeply/nested/file.txt",
            ),
        )
        .expect("nested object keys must be allowed");
    }

    #[test]
    fn empty_path_is_rejected() {
        let cfg = make_config();
        assert!(validate_request(&cfg, &req(http::Method::GET, "/")).is_err());
    }

    #[test]
    fn empty_bucket_is_rejected() {
        let cfg = make_config();
        assert!(validate_request(&cfg, &req(http::Method::GET, "//key")).is_err());
    }

    #[test]
    fn double_slash_in_key_is_rejected() {
        let cfg = make_config();
        assert!(validate_request(&cfg, &req(http::Method::GET, "/bucket/a//b")).is_err());
    }

    #[test]
    fn dotdot_segment_is_rejected() {
        let cfg = make_config();
        assert!(validate_request(&cfg, &req(http::Method::GET, "/bucket/../etc/passwd")).is_err());
    }

    #[test]
    fn secure_headers_exclude_deprecated_xss_protection() {
        let mut headers = http::HeaderMap::new();

        add_secure_headers(&mut headers);

        assert!(headers.get("X-XSS-Protection").is_none());
        assert_eq!(headers.get("X-Content-Type-Options").unwrap(), "nosniff");
        assert!(headers.get("Strict-Transport-Security").is_some());
        assert!(headers.get("Content-Security-Policy").is_some());
    }

    // Suppress unused-import warnings for items only used in non-test code.
    #[allow(dead_code)]
    fn _unused_imports_marker() {
        let _ = HashMap::<String, String>::new();
    }

    #[test]
    fn rate_limiter_allows_under_limit() {
        let mut rl = RateLimiter::new();
        let cfg = rate_cfg(3, 60);
        for _ in 0..3 {
            assert!(!rl.is_rate_limited("alice", cfg));
        }
    }

    #[test]
    fn rate_limiter_blocks_at_limit() {
        let mut rl = RateLimiter::new();
        let cfg = rate_cfg(2, 60);
        assert!(!rl.is_rate_limited("alice", cfg));
        assert!(!rl.is_rate_limited("alice", cfg));
        // 3rd request in the window must be rejected.
        assert!(rl.is_rate_limited("alice", cfg));
        // Successive checks while still over the limit also fail and do NOT
        // accumulate further timestamps for the user.
        assert!(rl.is_rate_limited("alice", cfg));
    }

    #[test]
    fn rate_limiter_isolates_users() {
        let mut rl = RateLimiter::new();
        let cfg = rate_cfg(1, 60);
        assert!(!rl.is_rate_limited("alice", cfg));
        assert!(rl.is_rate_limited("alice", cfg));
        // Bob has his own bucket and is unaffected by Alice's traffic.
        assert!(!rl.is_rate_limited("bob", cfg));
    }

    #[test]
    fn rate_limiter_recovers_after_window() {
        let mut rl = RateLimiter::new();
        // 1-request-per-1-second window.
        let cfg = rate_cfg(1, 1);
        assert!(!rl.is_rate_limited("alice", cfg));
        assert!(rl.is_rate_limited("alice", cfg));
        std::thread::sleep(Duration::from_millis(1100));
        assert!(
            !rl.is_rate_limited("alice", cfg),
            "request after the window elapses must be allowed"
        );
    }

    #[test]
    fn rate_limiter_zero_max_disables_limit() {
        let mut rl = RateLimiter::new();
        let cfg = rate_cfg(0, 60);
        for _ in 0..1_000 {
            assert!(!rl.is_rate_limited("alice", cfg));
        }
        // And no state should be retained for the user when the limiter is
        // disabled — saves memory and avoids surprising side-effects.
        assert!(rl.requests.is_empty());
    }

    #[test]
    fn rate_limiter_reclaims_idle_users() {
        // Per-user `Vec<Instant>` allocations should not pile up forever for
        // callers that go silent: once their entire window has expired, the
        // map entry is removed on the next call by any user.
        let mut rl = RateLimiter::new();
        let cfg = rate_cfg(5, 1);
        for _ in 0..3 {
            assert!(!rl.is_rate_limited("alice", cfg));
        }
        assert_eq!(rl.requests.len(), 1);
        std::thread::sleep(Duration::from_millis(1100));

        // A request from a different user should evict alice's stale entry.
        assert!(!rl.is_rate_limited("bob", cfg));
        assert!(
            !rl.requests.contains_key("alice"),
            "stale entry for alice must be reclaimed; map = {:?}",
            rl.requests.keys().collect::<Vec<_>>()
        );
    }
}

pub async fn auth_middleware(
    State(config): State<Arc<Config>>,
    mut request: Request,
    next: Next,
) -> Response {
    // Validate request
    if let Err(e) = validate_request(&config, &request) {
        return e.into_response();
    }

    // Get API key from header
    let api_key = match request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
    {
        Some(key) => key,
        None => {
            warn!("No API key provided");
            return AppError::Unauthorized("No API key provided".to_string()).into_response();
        }
    };

    // Find user by API key
    let (username, user) = match config.find_user_by_api_key(api_key) {
        Some(u) => u,
        None => {
            warn!("Invalid API key");
            return AppError::Unauthorized("Invalid API key".to_string()).into_response();
        }
    };

    // Check rate limit
    let limited = {
        let mut limiter = rate_limiter().lock().expect("rate limiter mutex poisoned");
        limiter.is_rate_limited(username, config.rate_limit)
    };
    if limited {
        warn!("Rate limit exceeded for user {}", username);
        return AppError::RateLimited("Rate limit exceeded".to_string()).into_response();
    }

    // Add auth state to request extensions
    request.extensions_mut().insert(AuthState {
        username: username.to_string(),
        role: format!("{:?}", user.role),
    });

    // Process the request
    let mut response = next.run(request).await;

    add_secure_headers(response.headers_mut());

    info!(
        "Authenticated user: {} with role: {:?}",
        username, user.role
    );
    response
}

pub fn check_bucket_access(config: &Config, username: &str, bucket: &str) -> Result<()> {
    if !config.is_bucket_allowed(username, bucket) {
        warn!("User {} not allowed to access bucket {}", username, bucket);
        return Err(AppError::Unauthorized(format!(
            "Not allowed to access bucket: {}",
            bucket
        )));
    }
    Ok(())
}

pub fn check_write_permission(config: &Config, username: &str) -> Result<()> {
    if !config.can_write(username) {
        warn!("User {} not allowed to write", username);
        return Err(AppError::Unauthorized(
            "Write permission denied".to_string(),
        ));
    }
    Ok(())
}
