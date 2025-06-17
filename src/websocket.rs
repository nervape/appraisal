use crate::{error::Error, types::Transaction};
use futures::{future::join_all, SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tracing::info;
use tracing::{debug, error, warn};
use uuid::Uuid;

#[derive(Debug, Deserialize, Clone)]
pub struct TransactionWrapper {
    pub transaction: Transaction,
    pub tx_status: String,
    #[serde(flatten)]
    pub other: serde_json::Value,
}

type WsSender = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Message,
>;

#[derive(Clone)]
pub struct WsClient {
    sender: Arc<Mutex<WsSender>>,
    pending_requests: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>,
    ws_url: Arc<String>,
    reconnect_attempts: Arc<std::sync::atomic::AtomicU32>,
}

impl WsClient {
    pub async fn new(ws_url: &str) -> Result<Self, Error> {
        info!("Connecting to CKB WebSocket at {}", ws_url);
        
        let (sender, receiver) = Self::establish_connection(ws_url).await?;
        let pending_requests: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_requests_clone = pending_requests.clone();

        // Handle incoming WebSocket messages
        let sender = Arc::new(Mutex::new(sender));
        let sender_clone = sender.clone();
        let ws_url_arc = Arc::new(ws_url.to_string());
        let ws_url_clone = ws_url_arc.clone();

        Self::spawn_message_handler(
            receiver,
            pending_requests_clone,
            sender_clone,
            ws_url_clone,
        );

        Ok(Self {
            sender,
            pending_requests,
            ws_url: ws_url_arc,
            reconnect_attempts: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        })
    }
    
    async fn establish_connection(ws_url: &str) -> Result<(
        WsSender,
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
        sender: Arc<Mutex<WsSender>>,
        ws_url: Arc<String>,
    ) {
        tokio::spawn(async move {
            while let Some(msg) = receiver.next().await {
                match msg {
                    Ok(msg) => {
                        if let Ok(response) = serde_json::from_str::<Value>(&msg.to_string()) {
                            let responses = if response.is_array() {
                                response.as_array().unwrap().clone()
                            } else {
                                vec![response]
                            };

                            let mut requests = pending_requests.lock().await;
                            for resp in responses {
                                if let Some(id) = resp.get("id").and_then(Value::as_str) {
                                    debug!("Received response for ID: {}", id);
                                    if let Some(sender) = requests.get(id) {
                                        if sender.send(resp.clone()).await.is_err() {
                                            error!("Failed to send response to channel for ID: {}", id);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {
                        debug!("WebSocket receiver error. Connection may have dropped.");
                        break; // Exit the loop to handle reconnection
                    }
                }
            }

            debug!("WebSocket receiver loop ended. Attempting to reconnect...");
            // By clearing the pending requests, we ensure that any futures waiting on a response
            // will be immediately notified that the sender has been dropped. This prevents them
            // from waiting indefinitely for a response that will never come.
            pending_requests.lock().await.clear();

            // Connection is lost, we need to reconnect and restart the handler
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await; // wait before reconnecting
                if let Ok((new_sender, new_receiver)) = Self::establish_connection(&ws_url).await {
                    info!("Successfully reconnected WebSocket.");
                    *sender.lock().await = new_sender;
                    Self::spawn_message_handler(new_receiver, pending_requests, sender, ws_url);
                    break;
                }
            }
        });
    }
    
    pub async fn get_transactions(
        &self,
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
        let mut requests_lock = self.pending_requests.lock().await;
        for request in &batch_requests {
            let id = request["id"].as_str().unwrap().to_string();
            let (tx, rx) = mpsc::channel(1);
            requests_lock.insert(id, tx);
            response_channels.push(rx);
        }
        drop(requests_lock);

        // Send batch request
        self.sender
            .lock()
            .await
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&batch_requests)?,
            ))
            .await?;

        // Collect responses concurrently
        let responses_futures = response_channels
            .into_iter()
            .map(|mut rx| tokio::spawn(async move { rx.recv().await }));

        let responses = join_all(responses_futures).await;

        let mut transactions = Vec::new();
        for (i, res) in responses.into_iter().enumerate() {
            let response = res.unwrap().ok_or_else(|| {
                Error::TxProcessing(format!("Channel closed for transaction {}", tx_hashes[i]))
            })?;

            if let Some(result) = response.get("result") {
                if result.is_null() {
                    return Err(Error::TxProcessing(format!(
                        "Transaction not found: {}",
                        tx_hashes[i]
                    )));
                }
                let transaction =
                    if let Ok(wrapper) = serde_json::from_value::<TransactionWrapper>(result.clone())
                    {
                        wrapper.transaction
                    } else {
                        match result.get("transaction") {
                            Some(tx) => serde_json::from_value(tx.clone()).map_err(|e| {
                                error!("Failed to parse transaction {}: {}", tx_hashes[i], e);
                                Error::TxProcessing(format!(
                                    "Failed to parse transaction {}: {}",
                                    tx_hashes[i], e
                                ))
                            })?,
                            None => {
                                error!("No transaction field found in response for {}", tx_hashes[i]);
                                return Err(Error::TxProcessing(format!(
                                    "No transaction field found in response for {}",
                                    tx_hashes[i]
                                )));
                            }
                        }
                    };

                transactions.push(transaction);
            } else if let Some(error) = response.get("error") {
                return Err(Error::TxProcessing(format!(
                    "Error fetching transaction {}: {}",
                    tx_hashes[i], error
                )));
            }
        }
        Ok(transactions)
    }
}
