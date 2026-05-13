use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use tracing::info;

use crate::error::{AppError, Result};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub accounts: HashMap<String, AccountConfig>,
    pub users: HashMap<String, UserConfig>,
    pub server: ServerConfig,
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
    #[serde(default = "default_rate_limit")]
    pub rate_limit: RateLimitConfig,
}

fn default_max_file_size() -> u64 {
    104_857_600 // 100 MB
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct RateLimitConfig {
    /// Maximum number of requests a single authenticated user may make per
    /// `window_secs`. A value of `0` disables rate limiting entirely.
    #[serde(default = "default_rate_limit_max_requests")]
    pub max_requests: u32,
    /// Sliding-window length, in seconds, over which `max_requests` is
    /// enforced.
    #[serde(default = "default_rate_limit_window_secs")]
    pub window_secs: u64,
}

fn default_rate_limit_max_requests() -> u32 { 100 }
fn default_rate_limit_window_secs() -> u64 { 60 }
fn default_rate_limit() -> RateLimitConfig {
    RateLimitConfig {
        max_requests: default_rate_limit_max_requests(),
        window_secs: default_rate_limit_window_secs(),
    }
}

#[derive(Debug, Deserialize)]
pub struct AccountConfig {
    pub endpoint_url: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub buckets: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct UserConfig {
    pub api_key: String,
    pub role: UserRole,
    pub allowed_buckets: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    User,
    Readonly,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub port: u16,
    pub host: String,
}

impl Config {
    pub fn find_account_for_bucket(&self, bucket: &str) -> Option<(&String, &AccountConfig)> {
        self.accounts.iter().find(|(_, account)| {
            account.buckets.contains(&bucket.to_string())
        })
    }

    pub fn find_user_by_api_key(&self, api_key: &str) -> Option<(&String, &UserConfig)> {
        self.users.iter().find(|(_, user)| user.api_key == api_key)
    }

    pub fn is_bucket_allowed(&self, username: &str, bucket: &str) -> bool {
        if let Some(user) = self.users.get(username) {
            user.allowed_buckets.contains(&"*".to_string()) || user.allowed_buckets.contains(&bucket.to_string())
        } else {
            false
        }
    }

    pub fn can_write(&self, username: &str) -> bool {
        if let Some(user) = self.users.get(username) {
            matches!(user.role, UserRole::Admin | UserRole::User)
        } else {
            false
        }
    }

    pub fn load(path: &str) -> Result<Self> {
        info!("Loading configuration from {}", path);
        
        let file = File::open(path)
            .map_err(|e| AppError::ConfigError(e))?;
            
        let reader = BufReader::new(file);
        let config = serde_json::from_reader(reader)
            .map_err(|e| AppError::ConfigError(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
            
        info!("Successfully loaded configuration");
        Ok(config)
    }
} 