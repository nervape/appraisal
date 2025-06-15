use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_ckb_ws_url")]
    pub ckb_ws_url: String,
    #[serde(default = "default_mqtt_host")]
    pub mqtt_host: String,
    #[serde(default = "default_mqtt_port")]
    pub mqtt_port: u16,
    #[serde(default = "default_mqtt_client_id")]
    pub mqtt_client_id: String,
    pub mqtt_username: Option<String>,
    pub mqtt_password: Option<String>,
    #[serde(default = "default_mqtt_subscribe_topic")]
    pub mqtt_subscribe_topic: String,
    #[serde(default = "default_mqtt_publish_topic")]
    pub mqtt_publish_topic: String,
    #[serde(default = "default_concurrent_requests")]
    pub concurrent_requests: usize,
    #[serde(default = "default_http_address")]
    pub http_address: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
}

impl Config {
    pub fn from_env() -> Result<Self, crate::error::Error> {
        match envy::from_env::<Config>() {
            Ok(config) => Ok(config),
            Err(e) => Err(crate::error::Error::Config(e.to_string())),
        }
    }
}

fn default_ckb_ws_url() -> String {
    "ws://localhost:8114".to_string()
}

fn default_mqtt_host() -> String {
    "localhost".to_string()
}

fn default_mqtt_port() -> u16 {
    1883
}

fn default_mqtt_client_id() -> String {
    "ckb-tx-detail-service".to_string()
}

fn default_mqtt_subscribe_topic() -> String {
    "ckb.transactions.proposed".to_string()
}

fn default_mqtt_publish_topic() -> String {
    "ckb.transactions.detailed.proposed".to_string()
}

fn default_concurrent_requests() -> usize {
    10
}

fn default_http_address() -> String {
    "0.0.0.0".to_string()
}

fn default_http_port() -> u16 {
    3112
}
