use crate::{
    enrichment::enrich_transaction, error::Error, types::Transaction, websocket::WsClient,
};
use axum::{
    extract::{Json as JsonExtractor, Path},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

#[derive(Debug, Deserialize)]
pub struct TransactionRequest {
    pub tx_hash: String,
    #[serde(default)]
    pub enrich: bool, // if not provided, default to false
}

pub struct HttpServer {
    ws_client: Arc<Mutex<WsClient>>, // shared with the MQTT sub
}

impl HttpServer {
    pub fn new(ws_client: Arc<Mutex<WsClient>>) -> Self {
        Self { ws_client }
    }

    pub async fn start(self, port: u16) {
        let ws_client = self.ws_client.clone();
        let ws_client_post = self.ws_client.clone();

        let app = Router::new()
            .route(
                "/tx/:hash", // tx hash. with or without 0x- prefiix
                get(move |hash| get_transaction_handler(hash, ws_client.clone())),
            )
            .route(
                "/tx", // using the same route with get
                post(move |payload| post_transaction_handler(payload, ws_client_post.clone())),
            );

        let addr = format!("0.0.0.0:{}", port);
        info!("Starting HTTP server on {}", addr);

        axum::Server::bind(&addr.parse().unwrap())
            .serve(app.into_make_service())
            .await
            .unwrap();
    }
}

// helper to normalize tx hash, dealing the 0x prefix. with or without is fine
fn normalize_tx_hash(hash: &str) -> String {
    if hash.starts_with("0x") {
        hash.to_string()
    } else {
        format!("0x{}", hash)
    }
}

// Helper function to validate transaction hash
fn validate_tx_hash(hash: &str) -> Result<String, String> {
    let hash = normalize_tx_hash(hash);

    // Remove "0x" prefix for length check
    let hash_without_prefix = hash.strip_prefix("0x").unwrap_or(&hash);

    // Check if the hash is a valid hex string and has the correct length (64 characters = 32 bytes)
    if hash_without_prefix.len() != 64
        || !hash_without_prefix.chars().all(|c| c.is_ascii_hexdigit())
    {
        Err("Invalid transaction hash format".to_string())
    } else {
        Ok(hash)
    }
}

async fn get_transaction_handler(
    Path(hash): Path<String>,
    ws_client: Arc<Mutex<WsClient>>,
) -> Result<Json<Transaction>, (StatusCode, String)> {
    // check tx hash first

    let normalized_hash = match validate_tx_hash(&hash) {
        Ok(h) => h,
        Err(e) => return Err((StatusCode::BAD_REQUEST, e)),
    };

    let mut ws_client_guard = ws_client.lock().await;

    match ws_client_guard
        .get_transactions(&[normalized_hash.clone()])
        .await
    {
        Ok(txs) => {
            if let Some(tx) = txs.first() {
                match enrich_transaction(tx.clone(), &mut ws_client_guard).await {
                    Ok(enriched_tx) => Ok(Json(enriched_tx)),
                    Err(e) => Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to enrich transaction: {}", e),
                    )),
                }
            } else {
                Err((
                    StatusCode::NOT_FOUND,
                    format!("Transaction {} not found", hash),
                ))
            }
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to fetch transaction: {}", e),
        )),
    }
}

async fn post_transaction_handler(
    JsonExtractor(payload): JsonExtractor<TransactionRequest>,
    ws_client: Arc<Mutex<WsClient>>,
) -> Result<Json<Transaction>, (StatusCode, String)> {
    // check tx hash first

    let normalized_hash = match validate_tx_hash(&payload.tx_hash) {
        Ok(h) => h,
        Err(e) => return Err((StatusCode::BAD_REQUEST, e)),
    };

    let mut ws_client_guard = ws_client.lock().await;

    info!(
        "Processing POST request for tx: {}, enrich: {}",
        normalized_hash, payload.enrich
    );

    match ws_client_guard
        .get_transactions(&[normalized_hash.clone()])
        .await
    {
        Ok(txs) => {
            if let Some(tx) = txs.first() {
                if payload.enrich {
                    match enrich_transaction(tx.clone(), &mut ws_client_guard).await {
                        Ok(enriched_tx) => Ok(Json(enriched_tx)),
                        Err(e) => Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Failed to enrich transaction: {}", e),
                        )),
                    }
                } else {
                    Ok(Json(tx.clone()))
                }
            } else {
                Err((
                    StatusCode::NOT_FOUND,
                    format!("Transaction {} not found", payload.tx_hash),
                ))
            }
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to fetch transaction: {}", e),
        )),
    }
}
