use crate::{config::Config, error::Error, types::Transaction, websocket::WsClient};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::collections::{HashMap, HashSet};
use tokio::sync::Semaphore;
use tracing::{error, info};

pub struct TxDetailService {
    ws_client: WsClient,
    mqtt_client: AsyncClient,
    mqtt_eventloop: rumqttc::EventLoop,
    config: Config,
}

impl TxDetailService {
    pub async fn new(config: &Config) -> Result<Self, Error> {
        let ws_client = WsClient::new(&config.ckb_ws_url).await?;

        let mqtt_options =
            MqttOptions::new(&config.mqtt_client_id, &config.mqtt_host, config.mqtt_port);

        // Create both client and eventloop together
        let (mqtt_client, mqtt_eventloop) = AsyncClient::new(mqtt_options, 10);

        Ok(Self {
            ws_client,
            mqtt_client,
            mqtt_eventloop,
            config: config.clone(),
        })
    }

    async fn process_transaction(&mut self, mut tx: Transaction) -> Result<Transaction, Error> {
        info!("Processing transaction: {}", tx.hash);

        // Collect all the information we need first
        let mut input_details: Vec<(usize, String, String)> = Vec::new();

        // First pass: collect all the information we need
        for (idx, input) in tx.inputs.iter().enumerate() {
            input_details.push((
                idx,
                input.previous_output.tx_hash.clone(),
                input.previous_output.index.clone(),
            ));
        }

        // Group by tx_hash and fetch source transactions
        let tx_hashes: HashSet<String> = input_details
            .iter()
            .map(|(_, hash, _)| hash.clone())
            .collect();

        let tx_hashes_vec: Vec<String> = tx_hashes.into_iter().collect();

        if !tx_hashes_vec.is_empty() {
            info!(
                "Querying {} unique source transactions",
                tx_hashes_vec.len()
            );
            let source_txs = self.ws_client.get_transactions(&tx_hashes_vec).await?;

            // Create lookup map
            let tx_map: HashMap<String, Transaction> = tx_hashes_vec
                .iter()
                .zip(source_txs)
                .map(|(hash, tx)| (hash.clone(), tx))
                .collect();

            // Process each input
            for (idx, tx_hash, index) in input_details {
                if let Some(source_tx) = tx_map.get(&tx_hash) {
                    let cell_index = usize::from_str_radix(&index.trim_start_matches("0x"), 16)
                        .map_err(|e| Error::TxProcessing(e.to_string()))?;

                    if let Some(output) = source_tx.outputs.get(cell_index) {
                        // Copy the output fields directly into the input
                        let input = &mut tx.inputs[idx];
                        input.capacity = Some(output.capacity.clone());
                        input.lock = Some(output.lock.clone());
                        input.type_script = output.type_script.clone();
                    } else {
                        return Err(Error::TxProcessing(format!(
                            "Output index {} not found in transaction {}",
                            cell_index, tx_hash
                        )));
                    }
                } else {
                    return Err(Error::TxProcessing(format!(
                        "Source transaction {} not found",
                        tx_hash
                    )));
                }
            }
        }

        // Verify all inputs have details
        for (i, input) in tx.inputs.iter().enumerate() {
            if input.capacity.is_none() || input.lock.is_none() {
                return Err(Error::TxProcessing(format!(
                    "Failed to process input at index {}",
                    i
                )));
            }
        }

        info!("Successfully processed transaction {}", tx.hash);
        Ok(tx)
    }

    pub async fn start(mut self) -> Result<(), Error> {
        info!("Starting transaction processing service");

        let semaphore = std::sync::Arc::new(Semaphore::new(self.config.concurrent_requests));
        self.mqtt_client
            .subscribe("ckb.transactions.proposed", QoS::AtLeastOnce)
            .await?;

        while let Ok(notification) = self.mqtt_eventloop.poll().await {
            if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(msg)) = notification {
                if let Ok(tx_str) = String::from_utf8(msg.payload.to_vec()) {
                    if let Ok(tx) = serde_json::from_str::<Transaction>(&tx_str) {
                        let permit = semaphore.clone().acquire_owned().await.unwrap();

                        match self.process_transaction(tx).await {
                            Ok(detailed_tx) => {
                                if let Err(e) = self
                                    .mqtt_client
                                    .publish(
                                        "ckb.transactions.proposed.detailed",
                                        QoS::AtLeastOnce,
                                        false,
                                        serde_json::to_string(&detailed_tx)?,
                                    )
                                    .await
                                {
                                    error!("Failed to publish detailed transaction: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("Error processing transaction: {}", e);
                            }
                        }

                        drop(permit);
                    }
                }
            }
        }

        Ok(())
    }
}
