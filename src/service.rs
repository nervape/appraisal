use crate::{
    config::Config, enrichment::enrich_transaction, error::Error, types::Transaction,
    websocket::WsClient,
};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, error, info};

pub struct TxDetailService {
    ws_client: Arc<Mutex<WsClient>>,
    mqtt_client: AsyncClient,
    mqtt_eventloop: rumqttc::EventLoop,
    config: Config,
}

impl TxDetailService {
    pub async fn new(config: &Config) -> Result<Self, Error> {
        let ws_client = WsClient::new(&config.ckb_ws_url).await?;
        let ws_client = Arc::new(Mutex::new(ws_client));

        let mut mqtt_options =
            MqttOptions::new(&config.mqtt_client_id, &config.mqtt_host, config.mqtt_port);

        if config.mqtt_username.is_some() {
            mqtt_options.set_credentials(
                config.mqtt_username.clone().unwrap(),
                config.mqtt_password.clone().unwrap_or_default(),
            );
        }
        mqtt_options.set_keep_alive(Duration::from_secs(5));

        // Create both client and eventloop together
        let (mqtt_client, mqtt_eventloop) = AsyncClient::new(mqtt_options, 10);

        Ok(Self {
            ws_client,
            mqtt_client,
            mqtt_eventloop,
            config: config.clone(),
        })
    }

    async fn process_transaction(&mut self, tx: Transaction) -> Result<Transaction, Error> {
        let mut ws_client = self.ws_client.lock().await;
        enrich_transaction(tx, &mut ws_client).await
    }

    pub async fn start(mut self) -> Result<(), Error> {
        let ws_client_clone = Arc::clone(&self.ws_client);

        let http_server = crate::http::HttpServer::new(ws_client_clone);

        // Spawn HTTP server in a separate task
        let http_port = self.config.http_port;
        let http_address = self.config.http_address.to_owned();
        tokio::spawn(async move {
            http_server.start(http_address, http_port).await;
        });

        info!("Starting transaction processing service");

        let semaphore = std::sync::Arc::new(Semaphore::new(self.config.concurrent_requests));
        
        loop {
            match self.mqtt_client
                .subscribe(self.config.mqtt_subscribe_topic.clone(), QoS::AtLeastOnce)
                .await
            {
                Ok(_) => info!("Successfully subscribed to MQTT topic"),
                Err(e) => {
                    error!("Failed to subscribe to MQTT topic: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            }

            while let Ok(notification) = self.mqtt_eventloop.poll().await {
                if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(msg)) = notification {
                    if let Ok(tx_str) = String::from_utf8(msg.payload.to_vec()) {
                        debug!("Received transaction: {}", tx_str);
                        if let Ok(tx) = serde_json::from_str::<Transaction>(&tx_str) {
                            let permit = semaphore.clone().acquire_owned().await.unwrap();

                            match self.process_transaction(tx).await {
                                Ok(detailed_tx) => {
                                    if let Err(e) = self
                                        .mqtt_client
                                        .publish(
                                            self.config.mqtt_publish_topic.clone(),
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

            // If we reach here, it means the MQTT connection was lost
            error!("MQTT connection lost, attempting to reconnect in 5 seconds...");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;

            // Recreate MQTT client and eventloop
            let mut mqtt_options =
                MqttOptions::new(&self.config.mqtt_client_id, &self.config.mqtt_host, self.config.mqtt_port);

            if self.config.mqtt_username.is_some() {
                mqtt_options.set_credentials(
                    self.config.mqtt_username.clone().unwrap(),
                    self.config.mqtt_password.clone().unwrap_or_default(),
                );
            }

            let (new_client, new_eventloop) = AsyncClient::new(mqtt_options, 10);
            self.mqtt_client = new_client;
            self.mqtt_eventloop = new_eventloop;
        }
    }
}
