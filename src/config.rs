use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use subtle::ConstantTimeEq;
use tracing::info;

use crate::error::{AppError, Result};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub accounts: HashMap<String, AccountConfig>,
    pub users: HashMap<String, UserConfig>,
    pub server: ServerConfig,
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
    #[serde(skip)]
    api_key_index: HashMap<String, String>,
}

fn default_max_file_size() -> u64 {
    104_857_600 // 100 MB
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
        self.accounts
            .iter()
            .find(|(_, account)| account.buckets.contains(&bucket.to_string()))
    }

    pub fn build_index(&mut self) {
        self.api_key_index = self
            .users
            .iter()
            .map(|(username, user)| (user.api_key.clone(), username.clone()))
            .collect();
    }

    pub fn find_user_by_api_key(&self, api_key: &str) -> Option<(&String, &UserConfig)> {
        const DUMMY_API_KEY_LEN: usize = 64;

        if let Some(username) = self.api_key_index.get(api_key) {
            let user = self.users.get(username)?;
            if user.api_key.as_bytes().ct_eq(api_key.as_bytes()).into() {
                return Some((username, user));
            }
            return None;
        }

        // Keep the miss path from being just a fast HashMap rejection: compare a
        // fixed-size dummy buffer against the supplied key padded/truncated to
        // the same length. This does not make the HashMap lookup constant-time,
        // but it avoids reintroducing a per-byte string comparison leak here.
        let mut candidate = [0_u8; DUMMY_API_KEY_LEN];
        let api_key_bytes = api_key.as_bytes();
        let copy_len = api_key_bytes.len().min(DUMMY_API_KEY_LEN);
        candidate[..copy_len].copy_from_slice(&api_key_bytes[..copy_len]);
        let dummy = [0_u8; DUMMY_API_KEY_LEN];
        let _ = dummy.as_slice().ct_eq(candidate.as_slice());

        None
    }

    pub fn is_bucket_allowed(&self, username: &str, bucket: &str) -> bool {
        if let Some(user) = self.users.get(username) {
            user.allowed_buckets.contains(&"*".to_string())
                || user.allowed_buckets.contains(&bucket.to_string())
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

        let file = File::open(path).map_err(|e| AppError::ConfigError(e))?;

        let reader = BufReader::new(file);
        let mut config: Config = serde_json::from_reader(reader).map_err(|e| {
            AppError::ConfigError(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        config.build_index();

        info!("Successfully loaded configuration");
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(users: HashMap<String, UserConfig>) -> Config {
        let mut config = Config {
            accounts: HashMap::new(),
            users,
            server: ServerConfig {
                port: 8080,
                host: "127.0.0.1".to_string(),
            },
            max_file_size: default_max_file_size(),
            api_key_index: HashMap::new(),
        };
        config.build_index();
        config
    }

    fn test_user(api_key: &str) -> UserConfig {
        UserConfig {
            api_key: api_key.to_string(),
            role: UserRole::User,
            allowed_buckets: vec!["test-bucket".to_string()],
        }
    }

    #[test]
    fn find_user_by_api_key_returns_some_for_matching_key() {
        let config = test_config(HashMap::from([(
            "alice".to_string(),
            test_user("alice-api-key"),
        )]));

        let (username, user) = config
            .find_user_by_api_key("alice-api-key")
            .expect("matching API key should find a user");

        assert_eq!(username, "alice");
        assert_eq!(user.api_key, "alice-api-key");
    }

    #[test]
    fn find_user_by_api_key_returns_none_for_unknown_key() {
        let config = test_config(HashMap::from([(
            "alice".to_string(),
            test_user("alice-api-key"),
        )]));

        assert!(config.find_user_by_api_key("unknown-api-key").is_none());
    }

    #[test]
    fn find_user_by_api_key_returns_none_when_index_and_user_key_disagree() {
        let mut config = test_config(HashMap::from([(
            "alice".to_string(),
            test_user("alice-api-key"),
        )]));
        config
            .users
            .get_mut("alice")
            .expect("test user should exist")
            .api_key = "rotated-api-key".to_string();

        assert!(config.find_user_by_api_key("alice-api-key").is_none());
    }

    #[test]
    fn find_user_by_api_key_returns_correct_user_for_each_key() {
        let config = test_config(HashMap::from([
            ("alice".to_string(), test_user("alice-api-key")),
            ("bob".to_string(), test_user("bob-api-key")),
        ]));

        let (alice_username, _) = config
            .find_user_by_api_key("alice-api-key")
            .expect("alice should be found");
        let (bob_username, _) = config
            .find_user_by_api_key("bob-api-key")
            .expect("bob should be found");

        assert_eq!(alice_username, "alice");
        assert_eq!(bob_username, "bob");
    }
}
