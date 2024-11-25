use crate::{
    config::Config, enrichment::enrich_transaction, error::Error, types::Transaction,
    websocket::WsClient,
};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tracing::{error, info};

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

    async fn process_transaction(&mut self, tx: Transaction) -> Result<Transaction, Error> {
        let mut ws_client = self.ws_client.lock().await;
        enrich_transaction(tx, &mut ws_client).await
    }

    pub async fn start(mut self) -> Result<(), Error> {
        let ws_client_clone = Arc::clone(&self.ws_client);

        let http_server = crate::http::HttpServer::new(ws_client_clone);

        // Spawn HTTP server in a separate task
        let http_port = self.config.http_port;
        tokio::spawn(async move {
            http_server.start(http_port).await;
        });

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
