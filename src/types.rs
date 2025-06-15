use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Transaction {
    pub hash: String,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub cell_deps: Vec<CellDep>,
    pub header_deps: Vec<String>,
    pub outputs_data: Vec<String>,
    pub version: String,
    pub witnesses: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CellDep {
    pub dep_type: String,
    pub out_point: OutPoint,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Input {
    pub previous_output: OutPoint,
    pub since: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock: Option<Script>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_script: Option<Script>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OutPoint {
    pub index: String,
    pub tx_hash: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Output {
    pub capacity: String,
    pub lock: Script,
    #[serde(rename = "type")]
    pub type_script: Option<Script>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Script {
    pub args: String,
    pub code_hash: String,
    pub hash_type: String,
}
