use axum::{
    extract::Request,
    http::header,
    middleware::Next,
    response::Response,
    extract::State,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::Config;
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

    fn is_rate_limited(&mut self, username: &str) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(60); // 1 minute window
        let max_requests = 100; // max requests per minute

        let requests = self.requests.entry(username.to_string()).or_default();
        
        // Remove old requests
        requests.retain(|&time| now.duration_since(time) < window);
        
        // Check if rate limited
        if requests.len() >= max_requests {
            return true;
        }
        
        // Add new request
        requests.push(now);
        false
    }
}

lazy_static::lazy_static! {
    static ref RATE_LIMITER: RwLock<RateLimiter> = RwLock::new(RateLimiter::new());
}

fn validate_request(config: &Config, request: &Request) -> Result<()> {
    // Check content length for PUT requests
    if request.method() == http::Method::PUT {
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

    // Validate path components
    if let Some(path) = request.uri().path().strip_prefix('/') {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.is_empty() || parts.len() > 2 {
            return Err(AppError::InvalidRequest("Invalid path format".to_string()));
        }
    }

    Ok(())
}

pub async fn auth_middleware(
    State(config): State<Arc<Config>>,
    mut request: Request,
    next: Next,
) -> Result<Response> {
    // Validate request
    validate_request(&config, &request)?;

    // Get API key from header
    let api_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            warn!("No API key provided");
            AppError::Unauthorized("No API key provided".to_string())
        })?;

    // Find user by API key
    let (username, user) = config
        .find_user_by_api_key(api_key)
        .ok_or_else(|| {
            warn!("Invalid API key");
            AppError::Unauthorized("Invalid API key".to_string())
        })?;

    // Check rate limit
    if RATE_LIMITER.write().await.is_rate_limited(&username) {
        warn!("Rate limit exceeded for user {}", username);
        return Err(AppError::InvalidRequest("Rate limit exceeded".to_string()));
    }

    // Add auth state to request extensions
    request.extensions_mut().insert(AuthState {
        username: username.to_string(),
        role: format!("{:?}", user.role),
    });

    // Process the request
    let mut response = next.run(request).await;

    // Add secure headers
    let headers = response.headers_mut();
    headers.insert("X-Content-Type-Options", "nosniff".parse().unwrap());
    headers.insert("X-Frame-Options", "DENY".parse().unwrap());
    headers.insert("X-XSS-Protection", "1; mode=block".parse().unwrap());
    headers.insert("Strict-Transport-Security", "max-age=31536000; includeSubDomains".parse().unwrap());

    info!("Authenticated user: {} with role: {:?}", username, user.role);
    Ok(response)
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
        return Err(AppError::Unauthorized("Write permission denied".to_string()));
    }
    Ok(())
} 