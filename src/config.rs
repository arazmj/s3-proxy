use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3AccountConfig {
    pub endpoint_url: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub buckets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub port: u16,
    pub host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub accounts: HashMap<String, S3AccountConfig>,
    pub server: ServerConfig,
}

impl Config {
    pub fn find_account_for_bucket(&self, bucket: &str) -> Option<(&str, &S3AccountConfig)> {
        self.accounts
            .iter()
            .find(|(_, account)| account.buckets.contains(&bucket.to_string()))
            .map(|(id, config)| (id.as_str(), config))
    }

    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let config = serde_json::from_reader(reader)?;
        Ok(config)
    }
} 