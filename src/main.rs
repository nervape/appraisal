use crate::config::Config;
use crate::service::TxDetailService;
use tracing::{error, info};

mod config;
mod error;
mod service;
mod types;
mod websocket;

#[tokio::main]
async fn main() -> Result<(), error::Error> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    info!("Starting CKB Transaction Detail Service");

    let config = Config::from_env()?;
    info!("Configuration loaded: {:?}", config);

    let service = TxDetailService::new(&config).await?;
    info!("Service initialized, starting main loop");

    match service.start().await {
        Ok(_) => info!("Service stopped gracefully"),
        Err(e) => error!("Service error: {}", e),
    }

    Ok(())
}
