// NOTE: we can adding infomations of the Inputs from Transactions as a "Enrichment" behavior
// so this file is actually defines how to deal with original Inputs from a transaction query result

use crate::{error::Error, types::Transaction, websocket::WsClient};
use std::collections::{HashMap, HashSet};
use tracing::info;

pub async fn enrich_transaction(
    mut tx: Transaction,
    ws_client: &WsClient,
) -> Result<Transaction, Error> {
    info!("Enriching transaction: {}", tx.hash);

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
        let source_txs = ws_client.get_transactions(&tx_hashes_vec).await?;

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

    info!("Successfully enriched transaction {}", tx.hash);
    Ok(tx)
}
