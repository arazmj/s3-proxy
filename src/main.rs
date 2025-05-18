mod config;
mod s3;
mod server;
mod error;

use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::error::{AppError, Result};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting S3 proxy server");

    // Load configuration
    let config = Arc::new(config::Config::load("config.json")?);
    info!("Loaded configuration with {} accounts", config.accounts.len());

    // Initialize S3 clients for each account
    let mut clients = HashMap::new();
    for (account_id, account_config) in &config.accounts {
        info!("Initializing S3 client for account {}", account_id);
        let client = s3::S3Client::new(
            account_config.endpoint_url.clone(),
            account_config.region.clone(),
            account_config.access_key_id.clone(),
            account_config.secret_access_key.clone(),
        ).await?;
        clients.insert(account_id.clone(), Arc::new(client));
    }

    // Create router
    let app = server::create_router(server::AppState {
        config: config.clone(),
        clients,
    }).await;

    // Start server
    let addr = format!("{}:{}", config.server.host, config.server.port);
    info!("Starting server on {}", addr);
    
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to bind to {}: {}", addr, e)))?;
    
    axum::serve(listener, app.into_make_service())
        .await
        .map_err(|e| AppError::InternalError(format!("Server error: {}", e)))?;

    Ok(())
} 