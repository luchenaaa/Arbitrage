//! Jito Bundle Sender
//! Sends transactions as Jito bundles for MEV protection
//! Supports both REST API and gRPC protocols
//! Supports parallel sending to all endpoints for better landing rate

use anyhow::{anyhow, Result};
use futures::future::join_all;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    signature::{Keypair, Signature},
    transaction::VersionedTransaction,
};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{JitoConfig, JitoProtocol, Settings};
use crate::jito_grpc::JitoGrpcClient;

/// Jito bundle sender - supports both REST and gRPC
pub struct JitoSender {
    http_client: reqwest::Client,
    grpc_clients: Vec<JitoGrpcClient>,
    config: JitoConfig,
    rpc: Arc<RpcClient>,
}

impl JitoSender {
    pub fn new(rpc: Arc<RpcClient>, settings: &Settings, auth_keypair: Option<Arc<Keypair>>) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        // Create gRPC clients for each endpoint if using gRPC protocol
        let grpc_clients = if settings.jito.protocol == JitoProtocol::Grpc {
            settings.jito.endpoints.iter()
                .map(|endpoint| JitoGrpcClient::new(endpoint, auth_keypair.clone()))
                .collect()
        } else {
            vec![]
        };

        if settings.jito.protocol == JitoProtocol::Grpc {
            tracing::info!("🔌 Using Jito gRPC protocol with {} endpoints", grpc_clients.len());
        } else {
            tracing::info!("🌐 Using Jito REST API protocol");
        }

        Self {
            http_client,
            grpc_clients,
            config: settings.jito.clone(),
            rpc,
        }
    }

    /// Send a single transaction as a Jito bundle
    pub async fn send_bundle(&self, transaction: VersionedTransaction, tip_lamports: u64) -> Result<String> {
        let transactions = vec![transaction];
        self.send_bundle_transactions(transactions, tip_lamports).await
    }

    /// Send multiple transactions as a Jito bundle
    pub async fn send_bundle_transactions(
        &self,
        transactions: Vec<VersionedTransaction>,
        tip_lamports: u64,
    ) -> Result<String> {
        match self.config.protocol {
            JitoProtocol::Grpc => {
                if self.config.parallel_send {
                    self.send_grpc_parallel(&transactions).await
                } else {
                    self.send_grpc_single(&transactions).await
                }
            }
            JitoProtocol::Rest => {
                if self.config.parallel_send {
                    self.send_rest_parallel(&transactions).await
                } else {
                    let endpoint = self.config.endpoints.first()
                        .ok_or_else(|| anyhow!("No Jito endpoints configured"))?;
                    self.send_rest_single(endpoint, &transactions).await
                }
            }
        }
    }

    // ==================== gRPC Methods ====================

    /// Send bundle via gRPC to all endpoints in parallel
    async fn send_grpc_parallel(&self, transactions: &[VersionedTransaction]) -> Result<String> {
        if self.grpc_clients.is_empty() {
            return Err(anyhow!("No gRPC clients configured"));
        }

        tracing::info!("🚀 Sending bundle via gRPC to {} endpoints in parallel...", self.grpc_clients.len());

        let futures: Vec<_> = self.grpc_clients.iter()
            .enumerate()
            .map(|(i, client)| {
                let txs = transactions.to_vec();
                let endpoint = self.config.endpoints.get(i).cloned().unwrap_or_default();
                async move {
                    match client.send_bundle(&txs).await {
                        Ok(sig) => {
                            tracing::info!("✅ gRPC bundle landed via {}", endpoint);
                            Ok((endpoint, sig))
                        }
                        Err(e) => {
                            tracing::warn!("❌ gRPC {} failed: {}", endpoint, e);
                            Err(e)
                        }
                    }
                }
            })
            .collect();

        let results = join_all(futures).await;

        // Find first success
        for result in &results {
            if let Ok((endpoint, sig)) = result {
                let success_count = results.iter().filter(|r| r.is_ok()).count();
                tracing::info!("🎉 gRPC bundle landed via: {} ({}/{} endpoints succeeded)", 
                    endpoint, success_count, self.grpc_clients.len());
                return Ok(sig.clone());
            }
        }

        let errors: Vec<String> = results.iter()
            .filter_map(|r| r.as_ref().err().map(|e| e.to_string()))
            .collect();
        
        tracing::error!("❌ All {} gRPC endpoints failed!", self.grpc_clients.len());
        Err(anyhow!("All gRPC endpoints failed. Errors: {:?}", errors))
    }

    /// Send bundle via gRPC to first endpoint only
    async fn send_grpc_single(&self, transactions: &[VersionedTransaction]) -> Result<String> {
        let client = self.grpc_clients.first()
            .ok_or_else(|| anyhow!("No gRPC clients configured"))?;
        
        client.send_bundle(transactions).await
    }

    // ==================== REST Methods ====================

    /// Send bundle via REST to all endpoints in parallel
    async fn send_rest_parallel(&self, transactions: &[VersionedTransaction]) -> Result<String> {
        let endpoints = &self.config.endpoints;
        
        if endpoints.is_empty() {
            return Err(anyhow!("No Jito endpoints configured"));
        }

        tracing::info!("🚀 Sending bundle via REST to {} endpoints in parallel...", endpoints.len());

        let futures: Vec<_> = endpoints.iter()
            .map(|endpoint| {
                let endpoint = endpoint.clone();
                let txs = transactions.to_vec();
                let client = self.http_client.clone();
                let uuid = self.config.uuid.clone();
                
                async move {
                    match Self::send_rest_internal(&client, &endpoint, &txs, &uuid).await {
                        Ok(sig) => {
                            tracing::info!("✅ REST bundle landed via {}", endpoint);
                            Ok((endpoint, sig))
                        }
                        Err(e) => {
                            tracing::warn!("❌ REST {} failed: {}", endpoint, e);
                            Err(e)
                        }
                    }
                }
            })
            .collect();

        let results = join_all(futures).await;

        // Find first success
        for result in &results {
            if let Ok((endpoint, sig)) = result {
                let success_count = results.iter().filter(|r| r.is_ok()).count();
                tracing::info!("🎉 REST bundle landed via: {} ({}/{} endpoints succeeded)", 
                    endpoint, success_count, endpoints.len());
                return Ok(sig.clone());
            }
        }

        let errors: Vec<String> = results.iter()
            .filter_map(|r| r.as_ref().err().map(|e| e.to_string()))
            .collect();
        
        tracing::error!("❌ All {} REST endpoints failed!", endpoints.len());
        Err(anyhow!("All REST endpoints failed. Errors: {:?}", errors))
    }

    /// Send bundle via REST to a single endpoint
    async fn send_rest_single(&self, endpoint: &str, transactions: &[VersionedTransaction]) -> Result<String> {
        Self::send_rest_internal(&self.http_client, endpoint, transactions, &self.config.uuid).await
    }

    /// Internal REST send implementation
    async fn send_rest_internal(
        client: &reqwest::Client,
        endpoint: &str,
        transactions: &[VersionedTransaction],
        uuid: &str,
    ) -> Result<String> {
        // Build API URL
        let api_url = if uuid.is_empty() {
            format!("{}/api/v1/bundles", endpoint)
        } else {
            format!("{}/api/v1/bundles?uuid={}", endpoint, uuid)
        };

        // Serialize transactions to base58 (Jito REST API format)
        let encoded_txs: Vec<String> = transactions.iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx)
                    .expect("Failed to serialize transaction");
                bs58::encode(&serialized).into_string()
            })
            .collect();

        // Build JSON-RPC request
        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [encoded_txs]
        });

        tracing::debug!("📤 Sending bundle to {}", endpoint);

        let response = client
            .post(&api_url)
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| anyhow!("HTTP request failed: {}", e))?;

        let status = response.status();
        let body: serde_json::Value = response.json().await
            .map_err(|e| anyhow!("Failed to parse response: {}", e))?;

        if !status.is_success() {
            return Err(anyhow!("Jito API error ({}): {:?}", status, body));
        }

        // Check for JSON-RPC error
        if let Some(error) = body.get("error") {
            return Err(anyhow!("Jito error: {:?}", error));
        }

        // Get bundle ID from response
        let bundle_id = body["result"]
            .as_str()
            .ok_or_else(|| anyhow!("No bundle ID in response: {:?}", body))?;

        tracing::info!("📦 Bundle submitted: {}", bundle_id);

        // Return first transaction signature
        let first_sig = transactions.first()
            .map(|tx| tx.signatures[0].to_string())
            .unwrap_or_else(|| bundle_id.to_string());

        Ok(first_sig)
    }

    // ==================== Utility Methods ====================

    /// Wait for transaction confirmation
    pub async fn wait_for_confirmation(&self, signature: &str, timeout_ms: u64) -> bool {
        let sig = match signature.parse::<Signature>() {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!("Invalid signature format: {}", signature);
                return false;
            }
        };

        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);
        let check_interval = Duration::from_millis(200);

        while start.elapsed() < timeout {
            match self.rpc.get_signature_status(&sig) {
                Ok(Some(status)) => {
                    match status {
                        Ok(_) => {
                            tracing::info!("✅ Transaction confirmed: {}", signature);
                            return true;
                        }
                        Err(e) => {
                            tracing::warn!("❌ Transaction failed: {} - {}", signature, e);
                            return false;
                        }
                    }
                }
                Ok(None) => {
                    // Not yet processed
                }
                Err(e) => {
                    tracing::debug!("RPC error checking status: {}", e);
                }
            }
            
            tokio::time::sleep(check_interval).await;
        }

        tracing::warn!("⏰ Confirmation timeout for: {}", signature);
        false
    }
}
