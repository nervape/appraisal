use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("MQTT error: {0}")]
    Mqtt(#[from] rumqttc::ClientError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Transaction processing error: {0}")]
    TxProcessing(String),

    #[error("HTTP error: {0}")]
    Http(String),
}
