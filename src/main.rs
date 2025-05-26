mod config;
mod s3;
mod server;
mod error;
mod auth;

use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, Level};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use tower_http::trace::{TraceLayer, DefaultMakeSpan, DefaultOnResponse};
use tower_http::classify::{ServerErrorsAsFailures, SharedClassifier};
use axum::extract::Request;

use crate::error::{AppError, Result};

fn redact_sensitive_data(headers: &http::HeaderMap) -> String {
    let mut redacted = String::new();
    for (name, value) in headers.iter() {
        let value = if name == "x-api-key" {
            "***REDACTED***"
        } else {
            value.to_str().unwrap_or("***INVALID***")
        };
        redacted.push_str(&format!("{}: {}\n", name, value));
    }
    redacted
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with custom format
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true);

    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .init();

    info!("Starting S3 proxy server");

    // Load configuration
    let config = Arc::new(config::Config::load("config.json")?);
    info!("Loaded configuration with {} accounts and {} users", 
        config.accounts.len(),
        config.users.len()
    );

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

    // Create router with request logging
    let app = server::create_router(server::AppState {
        config: config.clone(),
        clients,
    }).await
    .layer(
        TraceLayer::new(SharedClassifier::new(ServerErrorsAsFailures::new()))
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_response(DefaultOnResponse::new().level(Level::INFO))
            .on_request(|request: &Request<_>, _span: &tracing::Span| {
                info!(
                    method = %request.method(),
                    uri = %request.uri(),
                    headers = %redact_sensitive_data(request.headers()),
                    "Request started"
                );
            })
    );

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