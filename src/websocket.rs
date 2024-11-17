use crate::types::TransactionWrapper;
use crate::{error::Error, types::Transaction};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::error;
use tracing::info;
use uuid::Uuid;

pub struct WsClient {
    sender: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::Message,
    >,
    pending_requests: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>,
}

impl WsClient {
    pub async fn new(ws_url: &str) -> Result<Self, Error> {
        info!("Connecting to CKB WebSocket at {}", ws_url);
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url).await?;
        let (sender, mut receiver) = ws_stream.split();
        let pending_requests: Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_requests_clone = pending_requests.clone();

        // Handle incoming WebSocket messages
        tokio::spawn(async move {
            while let Some(msg) = receiver.next().await {
                if let Ok(msg) = msg {
                    if let Ok(response) = serde_json::from_str::<Value>(&msg.to_string()) {
                        // Handle both single response and batch responses
                        match response {
                            Value::Array(responses) => {
                                let requests = pending_requests_clone.lock().await;
                                for resp in responses {
                                    if let Some(id) = resp.get("id").and_then(Value::as_str) {
                                        info!("ID: {}", id);
                                        if let Some(sender) = requests.get(id) {
                                            info!("Sending response to sender");
                                            let _ = sender.send(resp.clone()).await;
                                        }
                                    }
                                }
                            }
                            Value::Object(_) => {
                                let requests = pending_requests_clone.lock().await;
                                if let Some(id) = response.get("id").and_then(Value::as_str) {
                                    if let Some(sender) = requests.get(id) {
                                        let _ = sender.send(response.clone()).await;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Self {
            sender,
            pending_requests,
        })
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

        // Send batch request
        self.sender
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&batch_requests)?,
            ))
            .await?;

        // Collect responses
        let mut transactions = Vec::new();
        for (i, rx) in response_channels.iter_mut().enumerate() {
            match rx.recv().await {
                Some(response) => {
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
                }
                None => {
                    return Err(Error::TxProcessing(format!(
                        "Channel closed for transaction {}",
                        tx_hashes[i]
                    )));
                }
            }
        }

        Ok(transactions)
    }
}
