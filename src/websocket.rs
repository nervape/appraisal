use crate::types::TransactionWrapper;
use crate::{error::Error, types::Transaction};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tracing::info;
use tracing::{debug, error, warn};
use uuid::Uuid;

pub struct WsClient {
    sender: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::Message,
    >,
    pending_requests: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>,
    ws_url: String,
    reconnect_attempts: std::sync::atomic::AtomicU32,
}

impl WsClient {
    pub async fn new(ws_url: &str) -> Result<Self, Error> {
        info!("Connecting to CKB WebSocket at {}", ws_url);
        
        let (sender, receiver) = Self::establish_connection(ws_url).await?;
        let pending_requests: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_requests_clone = pending_requests.clone();

        // Handle incoming WebSocket messages
        Self::spawn_message_handler(receiver, pending_requests_clone);

        Ok(Self {
            sender,
            pending_requests,
            ws_url: ws_url.to_string(),
            reconnect_attempts: std::sync::atomic::AtomicU32::new(0),
        })
    }
    
    async fn establish_connection(ws_url: &str) -> Result<(
        futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::Message,
        >,
        futures::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >
    ), Error> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url).await?;
        let (sender, receiver) = ws_stream.split();
        Ok((sender, receiver))
    }
    
    fn spawn_message_handler(
        mut receiver: futures::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
        pending_requests: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>,
    ) {
        tokio::spawn(async move {
            while let Some(msg) = receiver.next().await {
                if let Ok(msg) = msg {
                    if let Ok(response) = serde_json::from_str::<Value>(&msg.to_string()) {
                        // Handle both single response and batch responses
                        match response {
                            Value::Array(responses) => {
                                let requests = pending_requests.lock().await;
                                for resp in responses {
                                    if let Some(id) = resp.get("id").and_then(Value::as_str) {
                                        debug!("Received response for ID: {}", id);
                                        if let Some(sender) = requests.get(id) {
                                            if let Err(e) = sender.send(resp.clone()).await {
                                                error!("Failed to send response to channel: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                            Value::Object(_) => {
                                let requests = pending_requests.lock().await;
                                if let Some(id) = response.get("id").and_then(Value::as_str) {
                                    if let Some(sender) = requests.get(id) {
                                        if let Err(e) = sender.send(response.clone()).await {
                                            error!("Failed to send response to channel: {}", e);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            
            debug!("WebSocket receiver loop ended");
        });
    }
    
    async fn reconnect(&mut self) -> Result<(), Error> {
        let attempt = self.reconnect_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        warn!("WebSocket connection lost, attempting to reconnect (attempt #{})...", attempt);
        
        // Implement exponential backoff with maximum delay cap
        let backoff_secs = std::cmp::min(
            1 * (1 << std::cmp::min(attempt, 6)), // Start with 1 second and double up to 6 times
            60 // Cap at 1 minute
        );
        
        info!("Waiting {} seconds before reconnecting...", backoff_secs);
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        
        // Try to reconnect
        match Self::establish_connection(&self.ws_url).await {
            Ok((new_sender, receiver)) => {
                info!("Successfully reconnected to WebSocket");
                self.sender = new_sender;
                
                // Spawn a new message handler for the new connection
                let pending_requests_clone = self.pending_requests.clone();
                Self::spawn_message_handler(receiver, pending_requests_clone);
                
                // Reset reconnection counter on successful connection
                self.reconnect_attempts.store(0, std::sync::atomic::Ordering::SeqCst);
                
                Ok(())
            },
            Err(e) => {
                error!("Failed to reconnect to WebSocket: {}", e);
                Err(e)
            }
        }
    }

    pub async fn get_transactions(
        &mut self,
        tx_hashes: &[String],
    ) -> Result<Vec<Transaction>, Error> {
        if tx_hashes.is_empty() {
            return Ok(vec![]);
        }

        info!("Fetching {} transactions", tx_hashes.len());

        // Create batch request
        let batch_requests: Vec<Value> = tx_hashes
            .iter()
            .map(|hash| {
                json!({
                    "jsonrpc": "2.0",
                    "method": "get_transaction",
                    "params": [hash],
                    "id": Uuid::new_v4().to_string()
                })
            })
            .collect();

        // Create channels for responses
        let mut response_channels = Vec::new();
        for request in &batch_requests {
            let id = request["id"].as_str().unwrap().to_string();
            let (tx, rx) = mpsc::channel(1);
            {
                let mut requests = self.pending_requests.lock().await;
                requests.insert(id, tx);
            }
            response_channels.push(rx);
        }

        // Send batch request with retry logic
        let mut retry_count = 0;
        let max_retries = 3;
        
        while retry_count < max_retries {
            match self.sender
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::to_string(&batch_requests)?,
                ))
                .await
            {
                Ok(_) => break,
                Err(e) => {
                    error!("Failed to send WebSocket request: {}", e);
                    retry_count += 1;
                    
                    if retry_count >= max_retries {
                        return Err(Error::WebSocket(e));
                    }
                    
                    // Try to reconnect before retrying
                    if let Err(reconnect_err) = self.reconnect().await {
                        error!("Failed to reconnect: {}", reconnect_err);
                        return Err(reconnect_err);
                    }
                }
            }
        }

        // Collect responses with timeout
        let mut transactions = Vec::new();
        for (i, mut rx) in response_channels.iter_mut().enumerate() {
            // Add timeout to prevent hanging indefinitely
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Some(response)) => {
                    if let Some(result) = response.get("result") {
                        if result.is_null() {
                            return Err(Error::TxProcessing(format!(
                                "Transaction not found: {}",
                                tx_hashes[i]
                            )));
                        }

                        // Try to parse as TransactionWrapper first
                        let transaction = if let Ok(wrapper) =
                            serde_json::from_value::<TransactionWrapper>(result.clone())
                        {
                            wrapper.transaction
                        } else {
                            // If that fails, try to get the transaction directly from the transaction field
                            match result.get("transaction") {
                                Some(tx) => match serde_json::from_value(tx.clone()) {
                                    Ok(t) => t,
                                    Err(e) => {
                                        error!(
                                            "Failed to parse transaction {}: {}",
                                            tx_hashes[i], e
                                        );
                                        return Err(Error::TxProcessing(format!(
                                            "Failed to parse transaction {}: {}",
                                            tx_hashes[i], e
                                        )));
                                    }
                                },
                                None => {
                                    error!(
                                        "No transaction field found in response for {}",
                                        tx_hashes[i]
                                    );
                                    return Err(Error::TxProcessing(format!(
                                        "No transaction field found in response for {}",
                                        tx_hashes[i]
                                    )));
                                }
                            }
                        };

                        transactions.push(transaction);
                    }
                },
                Ok(None) => {
                    return Err(Error::TxProcessing(format!(
                        "Channel closed for transaction {}",
                        tx_hashes[i]
                    )));
                },
                Err(_) => {
                    return Err(Error::TxProcessing(format!(
                        "Timeout waiting for response for transaction {}",
                        tx_hashes[i]
                    )));
                }
            }
        }

        Ok(transactions)
    }
}
