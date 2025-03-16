use crate::{
    config::Config, enrichment::enrich_transaction, error::Error, types::Transaction,
    websocket::WsClient,
};
use rumqttc::{AsyncClient, Event, MqttOptions, QoS};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub struct TxDetailService {
    ws_client: Arc<Mutex<WsClient>>,
    mqtt_client: AsyncClient,
    mqtt_eventloop: rumqttc::EventLoop,
    config: Config,
    reconnect_attempts: Arc<AtomicU32>,
    is_reconnecting: Arc<AtomicBool>,
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
        
        // Increase keep alive for better connection stability
        mqtt_options.set_keep_alive(Duration::from_secs(30));

        mqtt_options.set_max_packet_size(10 * 1024 * 1024, 10 * 1024 * 1024);
        
        // Use clean_session(false) to maintain session state on server
        mqtt_options.set_clean_session(false);
        
        // Set manual_acks to false to let the library handle acks automatically
        mqtt_options.set_manual_acks(false);

        // Create both client and eventloop together
        let (mqtt_client, mqtt_eventloop) = AsyncClient::new(mqtt_options, 10);

        Ok(Self {
            ws_client,
            mqtt_client,
            mqtt_eventloop,
            config: config.clone(),
            reconnect_attempts: Arc::new(AtomicU32::new(0)),
            is_reconnecting: Arc::new(AtomicBool::new(false)),
        })
    }

    async fn process_transaction(&mut self, tx: Transaction) -> Result<Transaction, Error> {
        let mut ws_client = self.ws_client.lock().await;
        enrich_transaction(tx, &mut ws_client).await
    }
    
    async fn reconnect_mqtt(&mut self) -> Result<(), Error> {
        // Mark that we're attempting to reconnect
        self.is_reconnecting.store(true, Ordering::SeqCst);
        
        let attempt = self.reconnect_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        info!("Attempting to reconnect to MQTT broker (attempt #{})...", attempt);
        
        // Implement exponential backoff with maximum delay cap
        let backoff_secs = std::cmp::min(
            5 * (1 << std::cmp::min(attempt, 6)), // Start with 5 seconds and double up to 6 times
            300 // Cap at 5 minutes
        );
        
        info!("Waiting {} seconds before reconnecting...", backoff_secs);
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        
        // Recreate MQTT client and eventloop with improved settings
        let mut mqtt_options =
            MqttOptions::new(&self.config.mqtt_client_id, &self.config.mqtt_host, self.config.mqtt_port);

        if self.config.mqtt_username.is_some() {
            mqtt_options.set_credentials(
                self.config.mqtt_username.clone().unwrap(),
                self.config.mqtt_password.clone().unwrap_or_default(),
            );
        }
        
        // Increase keep alive for better connection stability
        mqtt_options.set_keep_alive(Duration::from_secs(30));
        
        // Use clean_session(false) to maintain session state on server
        mqtt_options.set_clean_session(false);
        
        // Set manual_acks to false to let the library handle acks automatically
        mqtt_options.set_manual_acks(false);

        let (new_client, new_eventloop) = AsyncClient::new(mqtt_options, 10);
        self.mqtt_client = new_client;
        self.mqtt_eventloop = new_eventloop;
        
        self.is_reconnecting.store(false, Ordering::SeqCst);
        
        Ok(())
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
                Ok(_) => {
                    info!("Successfully subscribed to MQTT topic");
                    // Reset reconnection attempts on successful subscription
                    self.reconnect_attempts.store(0, Ordering::SeqCst);
                },
                Err(e) => {
                    error!("Failed to subscribe to MQTT topic: {}", e);
                    self.reconnect_mqtt().await?;
                    continue;
                }
            }

            let mut last_successful_poll = std::time::Instant::now();
            let mut health_check_interval = tokio::time::interval(Duration::from_secs(30));
            
            'connection: loop {
                tokio::select! {
                    notification = self.mqtt_eventloop.poll() => {
                        match notification {
                            Ok(event) => {
                                // Update last successful poll time
                                last_successful_poll = std::time::Instant::now();
                                
                                match event {
                                    Event::Incoming(rumqttc::Packet::Publish(msg)) => {
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
                                    },
                                    Event::Incoming(rumqttc::Packet::ConnAck(_)) => {
                                        info!("MQTT connection established");
                                        self.reconnect_attempts.store(0, Ordering::SeqCst);
                                    },
                                    Event::Incoming(rumqttc::Packet::Disconnect) => {
                                        warn!("MQTT server sent disconnect packet");
                                        break 'connection;
                                    },
                                    _ => {
                                        // Other events we don't need to handle specifically
                                    }
                                }
                            },
                            Err(e) => {
                                error!("MQTT connection error: {}", e);
                                break 'connection;
                            }
                        }
                    },
                    
                    // Periodic health check
                    _ = health_check_interval.tick() => {
                        // Check if we're in a stalled state - no successful poll for too long
                        let elapsed = last_successful_poll.elapsed();
                        if elapsed > Duration::from_secs(60) {  // No successful poll for 1 minute
                            error!("MQTT connection appears stalled - no events for {} seconds", elapsed.as_secs());
                            break 'connection;
                        }
                        
                        debug!("MQTT connection health check passed");
                    }
                }
            }

            // If we reach here, it means the MQTT connection was lost
            error!("MQTT connection lost, attempting to reconnect...");
            self.reconnect_mqtt().await?;
        }
    }
}
