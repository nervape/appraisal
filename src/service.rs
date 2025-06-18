use crate::{
    config::Config, enrichment::enrich_transaction, error::Error, types::Transaction,
    websocket::WsClient,
};
use rumqttc::{AsyncClient, Event, MqttOptions, QoS};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

pub struct TxDetailService {
    ws_client: Arc<WsClient>,
    mqtt_client: AsyncClient,
    mqtt_eventloop: rumqttc::EventLoop,
    config: Config,
}

async fn process_transaction(
    ws_client: Arc<WsClient>,
    tx: Transaction,
) -> Result<Transaction, Error> {
    enrich_transaction(tx, &ws_client).await
}

impl TxDetailService {
    pub async fn new(config: &Config) -> Result<Self, Error> {
        let ws_client = WsClient::new(&config.ckb_ws_url).await?;
        let ws_client = Arc::new(ws_client);

        let mut mqtt_options =
            MqttOptions::new(&config.mqtt_client_id, &config.mqtt_host, config.mqtt_port);

        if let (Some(username), Some(password)) =
            (config.mqtt_username.as_ref(), config.mqtt_password.as_ref())
        {
            mqtt_options.set_credentials(username, password);
        }

        // Increase keep alive for better connection stability
        mqtt_options.set_keep_alive(Duration::from_secs(60));
        // Use a clean session to avoid session state issues on reconnect
        mqtt_options.set_clean_session(true);

        let (mqtt_client, mqtt_eventloop) = AsyncClient::new(mqtt_options, 10);

        Ok(Self {
            ws_client,
            mqtt_client,
            mqtt_eventloop,
            config: config.clone(),
        })
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
        let semaphore = Arc::new(Semaphore::new(self.config.concurrent_requests));
        let mut reconnect_attempts = 0;

        loop {
            if reconnect_attempts > 0 {
                let backoff_secs = 2u64.pow(reconnect_attempts.min(6) as u32); // Exponential backoff: 2, 4, 8, 16, 32, 64
                warn!(
                    "MQTT connection lost. Attempting to reconnect in {} seconds (attempt #{})...",
                    backoff_secs, reconnect_attempts
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            }

            // The rumqttc event loop will try to reconnect automatically in the background
            // when we call `poll()`. If poll returns an error, it's a disconnect.
            match self.mqtt_eventloop.poll().await {
                Ok(event) => {
                    self.handle_mqtt_event(event, semaphore.clone()).await?;
                    // On any successful event, reset the reconnect counter.
                    if reconnect_attempts > 0 {
                        info!("Successfully reconnected to MQTT broker.");
                        reconnect_attempts = 0;
                    }
                }
                Err(e) => {
                    error!("MQTT connection error: {}. Will attempt to reconnect.", e);
                    reconnect_attempts += 1;
                }
            }
        }
    }

    async fn handle_mqtt_event(
        &self,
        event: Event,
        semaphore: Arc<Semaphore>,
    ) -> Result<(), Error> {
        if let Event::Incoming(rumqttc::Packet::ConnAck(_)) = event {
            info!("MQTT connection established/re-established. Subscribing to topic...");
            self.mqtt_client
                .subscribe(&self.config.mqtt_subscribe_topic, QoS::AtLeastOnce)
                .await?;
        } else if let Event::Incoming(rumqttc::Packet::Publish(msg)) = event {
            let topic = msg.topic.clone();
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let client = self.mqtt_client.clone();
            let ws_client = self.ws_client.clone();
            let publish_topic = self.config.mqtt_publish_topic.clone();

            tokio::spawn(async move {
                if let Ok(tx) = serde_json::from_slice::<Transaction>(&msg.payload) {
                    debug!("Received transaction on topic {}: {}", topic, tx.hash);
                    match process_transaction(ws_client, tx).await {
                        Ok(detailed_tx) => {
                            let payload = serde_json::to_string(&detailed_tx)
                                .expect("Failed to serialize detailed transaction");
                            if let Err(e) = client
                                .publish(publish_topic, QoS::AtLeastOnce, false, payload)
                                .await
                            {
                                error!("Failed to publish detailed transaction: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("Error processing transaction: {}", e);
                        }
                    }
                } else {
                    warn!("Failed to deserialize message on topic {}", topic);
                }
                drop(permit);
            });
        } else if let Event::Incoming(rumqttc::Packet::Disconnect) = event {
            warn!("Received MQTT disconnect packet. The client will attempt to reconnect automatically.");
        } else {
            debug!("Received MQTT event: {:?}", event);
        }

        Ok(())
    }
}
