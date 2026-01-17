//! WebSocket Subscriber for pool account updates
//! Alternative to gRPC when Yellowstone is not available

use anyhow::{anyhow, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::{ConnectionStatus, PoolUpdate};

/// WebSocket subscriber for real-time pool updates
pub struct WsSubscriber {
    endpoint: String,
    pool_addresses: Vec<Pubkey>,
    updates_tx: mpsc::Sender<PoolUpdate>,
    updates_rx: Arc<RwLock<mpsc::Receiver<PoolUpdate>>>,
    status: Arc<RwLock<ConnectionStatus>>,
    subscription_ids: Arc<RwLock<HashMap<Pubkey, u64>>>,
}

impl WsSubscriber {
    pub fn new(endpoint: &str, pool_addresses: Vec<Pubkey>) -> Self {
        let (updates_tx, updates_rx) = mpsc::channel(1000);

        Self {
            endpoint: endpoint.to_string(),
            pool_addresses,
            updates_tx,
            updates_rx: Arc::new(RwLock::new(updates_rx)),
            status: Arc::new(RwLock::new(ConnectionStatus::Disconnected)),
            subscription_ids: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Connect and start subscribing to pool updates
    pub async fn connect(&self) -> Result<()> {
        *self.status.write().await = ConnectionStatus::Reconnecting;

        let (ws_stream, _) = connect_async(&self.endpoint)
            .await
            .map_err(|e| anyhow!("WebSocket connection failed: {}", e))?;

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to each pool account
        for (idx, pool_address) in self.pool_addresses.iter().enumerate() {
            let subscribe_msg = serde_json::json!({
                "jsonrpc": "2.0",
                "id": idx + 1,
                "method": "accountSubscribe",
                "params": [
                    pool_address.to_string(),
                    {
                        "encoding": "base64",
                        "commitment": "processed"
                    }
                ]
            });

            write
                .send(Message::Text(subscribe_msg.to_string()))
                .await
                .map_err(|e| anyhow!("Failed to subscribe to {}: {}", pool_address, e))?;
        }

        *self.status.write().await = ConnectionStatus::Connected;
        tracing::info!("📡 WebSocket connected, subscribed to {} pools", self.pool_addresses.len());

        // Spawn message handler
        let updates_tx = self.updates_tx.clone();
        let pool_addresses = self.pool_addresses.clone();
        let status = self.status.clone();
        let subscription_ids = self.subscription_ids.clone();

        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Err(e) = Self::process_message(
                            &text,
                            &pool_addresses,
                            &updates_tx,
                            &subscription_ids,
                        ).await {
                            tracing::debug!("Failed to process WS message: {}", e);
                        }
                    }
                    Ok(Message::Ping(_)) => {
                        // Ping handled automatically by tungstenite
                        tracing::trace!("Received WebSocket ping");
                    }
                    Ok(Message::Pong(_)) => {
                        tracing::trace!("Received WebSocket pong");
                    }
                    Ok(Message::Close(_)) => {
                        tracing::warn!("⚠️ WebSocket closed by server");
                        *status.write().await = ConnectionStatus::Disconnected;
                        break;
                    }
                    Err(e) => {
                        tracing::error!("❌ WebSocket error: {}", e);
                        *status.write().await = ConnectionStatus::Failed;
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(())
    }


    /// Connect with automatic reconnection
    pub async fn connect_with_reconnect(&self) -> Result<()> {
        const MAX_RECONNECT_ATTEMPTS: u32 = 10;
        const BASE_DELAY_MS: u64 = 1000;

        let mut attempts = 0u32;
        loop {
            match self.connect().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    attempts += 1;
                    if attempts >= MAX_RECONNECT_ATTEMPTS {
                        *self.status.write().await = ConnectionStatus::Failed;
                        return Err(anyhow!(
                            "Max reconnection attempts ({}) reached: {}",
                            MAX_RECONNECT_ATTEMPTS,
                            e
                        ));
                    }

                    *self.status.write().await = ConnectionStatus::Reconnecting;
                    let delay = BASE_DELAY_MS * (2u64.pow(attempts.min(6)));
                    tracing::warn!(
                        "⚠️ WS connection failed (attempt {}), retrying in {}ms: {}",
                        attempts,
                        delay,
                        e
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }

    /// Process incoming WebSocket message
    async fn process_message(
        text: &str,
        pool_addresses: &[Pubkey],
        updates_tx: &mpsc::Sender<PoolUpdate>,
        subscription_ids: &Arc<RwLock<HashMap<Pubkey, u64>>>,
    ) -> Result<()> {
        let json: serde_json::Value = serde_json::from_str(text)?;

        // Handle subscription confirmation
        if let Some(result) = json.get("result") {
            if let Some(id) = json.get("id").and_then(|v| v.as_u64()) {
                if let Some(sub_id) = result.as_u64() {
                    let idx = (id - 1) as usize;
                    if idx < pool_addresses.len() {
                        let mut subs = subscription_ids.write().await;
                        subs.insert(pool_addresses[idx], sub_id);
                        tracing::debug!("Subscribed to {} with id {}", pool_addresses[idx], sub_id);
                    }
                }
            }
            return Ok(());
        }

        // Handle account notification
        if let Some(params) = json.get("params") {
            if let Some(result) = params.get("result") {
                let subscription = params.get("subscription").and_then(|v| v.as_u64());
                
                if let Some(sub_id) = subscription {
                    // Find pool address by subscription ID
                    let subs = subscription_ids.read().await;
                    let pool_address = subs.iter()
                        .find(|(_, &id)| id == sub_id)
                        .map(|(addr, _)| *addr);
                    drop(subs);

                    if let Some(address) = pool_address {
                        if let Some(value) = result.get("value") {
                            if let Some(data_arr) = value.get("data").and_then(|d| d.as_array()) {
                                if let Some(data_b64) = data_arr.first().and_then(|v| v.as_str()) {
                                    let data = base64::engine::general_purpose::STANDARD
                                        .decode(data_b64)?;
                                    let slot = result.get("context")
                                        .and_then(|c| c.get("slot"))
                                        .and_then(|s| s.as_u64())
                                        .unwrap_or(0);

                                    let update = PoolUpdate {
                                        address,
                                        data,
                                        slot,
                                        timestamp: Instant::now(),
                                    };

                                    let _ = updates_tx.try_send(update);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Get connection status
    pub async fn status(&self) -> ConnectionStatus {
        *self.status.read().await
    }

    /// Check if connected
    pub async fn is_connected(&self) -> bool {
        *self.status.read().await == ConnectionStatus::Connected
    }

    /// Get all pending updates
    pub async fn drain_updates(&self) -> Vec<PoolUpdate> {
        let mut updates = Vec::new();
        let mut rx = self.updates_rx.write().await;
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }
        updates
    }
}
