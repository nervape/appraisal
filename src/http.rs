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
use tracing::info;

#[derive(Debug, Deserialize)]
pub struct TransactionRequest {
    pub tx_hash: String,
    #[serde(default)]
    pub enrich: bool, // if not provided, default to false
}

pub struct HttpServer {
    ws_client: WsClient, // shared with the MQTT sub
}

impl HttpServer {
    pub fn new(ws_client: WsClient) -> Self {
        Self { ws_client }
    }

    pub async fn start(self, address: String, port: u16) {
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

        let addr = format!("{}:{}", address, port);
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

async fn get_and_enrich_transaction(
    hash: &str,
    enrich: bool,
    ws_client: WsClient,
) -> Result<Json<Transaction>, (StatusCode, String)> {
    let normalized_hash = match validate_tx_hash(hash) {
        Ok(h) => h,
        Err(e) => return Err((StatusCode::BAD_REQUEST, Error::Http(e).to_string())),
    };

    info!(
        "Processing request for tx: {}, enrich: {}",
        normalized_hash, enrich
    );

    match ws_client.get_transactions(&[normalized_hash.clone()]).await {
        Ok(mut txs) => {
            if let Some(tx) = txs.pop() {
                if enrich {
                    match enrich_transaction(tx, &ws_client).await {
                        Ok(enriched_tx) => Ok(Json(enriched_tx)),
                        Err(e) => Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Failed to enrich transaction: {}", e),
                        )),
                    }
                } else {
                    Ok(Json(tx))
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

async fn get_transaction_handler(
    Path(hash): Path<String>,
    ws_client: WsClient,
) -> Result<Json<Transaction>, (StatusCode, String)> {
    get_and_enrich_transaction(&hash, true, ws_client).await
}

async fn post_transaction_handler(
    JsonExtractor(payload): JsonExtractor<TransactionRequest>,
    ws_client: WsClient,
) -> Result<Json<Transaction>, (StatusCode, String)> {
    get_and_enrich_transaction(&payload.tx_hash, payload.enrich, ws_client).await
}
