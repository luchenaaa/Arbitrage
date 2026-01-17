//! Yellowstone gRPC Subscriber (v10.x)
//! Real-time pool state updates via gRPC (works with Triton, Chainstack, Helius, etc.)

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestFilterSlots, SubscribeUpdate,
};

/// Pool update from gRPC subscription
#[derive(Debug, Clone)]
pub struct PoolUpdate {
    pub address: Pubkey,
    pub data: Vec<u8>,
    pub slot: u64,
    pub timestamp: Instant,
}

/// Slot update from gRPC
#[derive(Debug, Clone, Copy)]
pub struct SlotUpdate {
    pub slot: u64,
    pub parent: Option<u64>,
    pub status: SlotStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    Processed,
    Confirmed,
    Finalized,
}

/// gRPC connection status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}

/// gRPC subscriber for real-time pool updates
pub struct GrpcSubscriber {
    endpoint: String,
    x_token: Option<String>,
    pool_addresses: Vec<Pubkey>,
    updates_tx: mpsc::Sender<PoolUpdate>,
    updates_rx: Arc<RwLock<mpsc::Receiver<PoolUpdate>>>,
    slot_tx: mpsc::Sender<SlotUpdate>,
    slot_rx: Arc<RwLock<mpsc::Receiver<SlotUpdate>>>,
    status: Arc<RwLock<ConnectionStatus>>,
    current_slot: Arc<RwLock<u64>>,
    reconnect_attempts: Arc<RwLock<u32>>,
}

impl GrpcSubscriber {
    /// Create new gRPC subscriber
    pub fn new(endpoint: &str, x_token: Option<&str>, pool_addresses: Vec<Pubkey>) -> Self {
        let (updates_tx, updates_rx) = mpsc::channel(1000);
        let (slot_tx, slot_rx) = mpsc::channel(100);

        Self {
            endpoint: endpoint.to_string(),
            x_token: x_token.map(|s| s.to_string()),
            pool_addresses,
            updates_tx,
            updates_rx: Arc::new(RwLock::new(updates_rx)),
            slot_tx,
            slot_rx: Arc::new(RwLock::new(slot_rx)),
            status: Arc::new(RwLock::new(ConnectionStatus::Disconnected)),
            current_slot: Arc::new(RwLock::new(0)),
            reconnect_attempts: Arc::new(RwLock::new(0)),
        }
    }

    /// Connect and start subscribing to pool updates
    pub async fn connect(&self) -> Result<()> {
        *self.status.write().await = ConnectionStatus::Connecting;
        tracing::info!("🔌 Connecting to Yellowstone gRPC: {}", self.endpoint);

        // Build client with proper configuration
        let mut builder = GeyserGrpcClient::build_from_shared(self.endpoint.clone())
            .map_err(|e| anyhow!("Failed to build gRPC client: {}", e))?
            .x_token(self.x_token.clone())
            .map_err(|e| anyhow!("Failed to set x_token: {}", e))?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60));

        // Set large message size for account data (1GB like the example)
        builder = builder.max_decoding_message_size(1024 * 1024 * 1024); // 1GB

        let mut client = builder
            .connect()
            .await
            .map_err(|e| anyhow!("Failed to connect to gRPC: {}", e))?;

        *self.status.write().await = ConnectionStatus::Connected;
        *self.reconnect_attempts.write().await = 0;
        tracing::info!("✅ Connected to Yellowstone gRPC");

        // Build subscription request for pool accounts and slots
        let request = self.build_subscribe_request();

        // Subscribe using bidirectional stream
        let (mut subscribe_tx, mut stream) = client
            .subscribe_with_request(Some(request))
            .await
            .map_err(|e| anyhow!("Failed to subscribe: {}", e))?;

        tracing::info!(
            "📡 Subscribed to {} pool accounts",
            self.pool_addresses.len()
        );

        // Spawn background task to process updates
        let updates_tx = self.updates_tx.clone();
        let slot_tx = self.slot_tx.clone();
        let status = self.status.clone();
        let current_slot = self.current_slot.clone();
        let pool_addresses = self.pool_addresses.clone();

        tokio::spawn(async move {
            while let Some(update_result) = stream.next().await {
                match update_result {
                    Ok(update) => {
                        Self::process_update_inner(
                            &update,
                            &pool_addresses,
                            &updates_tx,
                            &slot_tx,
                            &current_slot,
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::error!("❌ gRPC stream error: {}", e);
                        *status.write().await = ConnectionStatus::Disconnected;
                        break;
                    }
                }
            }

            tracing::warn!("⚠️ gRPC stream ended");
            *status.write().await = ConnectionStatus::Disconnected;
        });

        // Spawn ping task to keep connection alive
        let status_ping = self.status.clone();
        tokio::spawn(async move {
            let mut ping_count = 0i32;
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;

                if *status_ping.read().await != ConnectionStatus::Connected {
                    break;
                }

                // Send ping via the subscribe channel
                ping_count = ping_count.wrapping_add(1);
                let ping_request = SubscribeRequest {
                    ping: Some(yellowstone_grpc_proto::geyser::SubscribeRequestPing {
                        id: ping_count,
                    }),
                    ..Default::default()
                };

                if let Err(e) = subscribe_tx.send(ping_request).await {
                    tracing::warn!("Failed to send ping: {}", e);
                    break;
                }
            }
        });

        Ok(())
    }

    /// Connect with automatic reconnection
    pub async fn connect_with_reconnect(&self) -> Result<()> {
        const MAX_RECONNECT_ATTEMPTS: u32 = 10;
        const BASE_DELAY_MS: u64 = 1000;

        loop {
            match self.connect().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let attempts = {
                        let mut attempts = self.reconnect_attempts.write().await;
                        *attempts += 1;
                        *attempts
                    };

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
                        "⚠️ Connection failed (attempt {}), retrying in {}ms: {}",
                        attempts,
                        delay,
                        e
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }

    /// Build subscription request for pool accounts and slots
    fn build_subscribe_request(&self) -> SubscribeRequest {
        let mut accounts_filter = HashMap::new();
        let mut slots_filter = HashMap::new();

        // Subscribe to each pool address
        let account_keys: Vec<String> = self.pool_addresses.iter().map(|p| p.to_string()).collect();

        accounts_filter.insert(
            "pools".to_string(),
            SubscribeRequestFilterAccounts {
                account: account_keys,
                owner: vec![],
                filters: vec![],
                nonempty_txn_signature: None,
            },
        );

        // Subscribe to slot updates for timing
        slots_filter.insert(
            "slots".to_string(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: Some(true),
                interslot_updates: Some(false),
            },
        );

        SubscribeRequest {
            accounts: accounts_filter,
            slots: slots_filter,
            transactions: HashMap::new(),
            transactions_status: HashMap::new(),
            blocks: HashMap::new(),
            blocks_meta: HashMap::new(),
            entry: HashMap::new(),
            commitment: Some(CommitmentLevel::Processed as i32),
            accounts_data_slice: vec![],
            ping: None,
            from_slot: None,
        }
    }

    /// Process a gRPC update internally
    async fn process_update_inner(
        update: &SubscribeUpdate,
        pool_addresses: &[Pubkey],
        updates_tx: &mpsc::Sender<PoolUpdate>,
        slot_tx: &mpsc::Sender<SlotUpdate>,
        current_slot: &Arc<RwLock<u64>>,
    ) {
        match &update.update_oneof {
            Some(UpdateOneof::Account(account_update)) => {
                if let Some(account) = &account_update.account {
                    if let Ok(pubkey_bytes) = <[u8; 32]>::try_from(account.pubkey.as_slice()) {
                        let address = Pubkey::new_from_array(pubkey_bytes);

                        // Check if this is one of our pool addresses
                        if pool_addresses.contains(&address) {
                            tracing::debug!(
                                "📥 Pool update: {} at slot {}",
                                address,
                                account_update.slot
                            );

                            let pool_update = PoolUpdate {
                                address,
                                data: account.data.clone().into(),
                                slot: account_update.slot,
                                timestamp: Instant::now(),
                            };

                            if updates_tx.send(pool_update).await.is_err() {
                                tracing::warn!("Failed to send pool update - channel closed");
                            }
                        }
                    }
                }
            }
            Some(UpdateOneof::Slot(slot_update)) => {
                // SlotStatus from proto: SlotProcessed=0, SlotConfirmed=1, SlotFinalized=2, etc.
                use yellowstone_grpc_proto::geyser::SlotStatus as ProtoSlotStatus;
                let status = match ProtoSlotStatus::try_from(slot_update.status) {
                    Ok(ProtoSlotStatus::SlotProcessed) => SlotStatus::Processed,
                    Ok(ProtoSlotStatus::SlotConfirmed) => SlotStatus::Confirmed,
                    Ok(ProtoSlotStatus::SlotFinalized) => SlotStatus::Finalized,
                    _ => SlotStatus::Processed, // Default for other statuses (FirstShredReceived, etc.)
                };

                // Update current slot
                {
                    let mut current = current_slot.write().await;
                    if slot_update.slot > *current {
                        *current = slot_update.slot;
                    }
                }

                let update = SlotUpdate {
                    slot: slot_update.slot,
                    parent: slot_update.parent,
                    status,
                };

                let _ = slot_tx.send(update).await;
            }
            Some(UpdateOneof::Ping(_)) => {
                tracing::trace!("Received ping from gRPC server");
            }
            Some(UpdateOneof::Pong(pong)) => {
                tracing::trace!("Received pong: {}", pong.id);
            }
            _ => {}
        }
    }

    /// Try to receive a pool update (non-blocking)
    pub async fn try_recv(&self) -> Option<PoolUpdate> {
        let mut rx = self.updates_rx.write().await;
        rx.try_recv().ok()
    }

    /// Receive a pool update (blocking with timeout)
    pub async fn recv_timeout(&self, timeout: Duration) -> Option<PoolUpdate> {
        let mut rx = self.updates_rx.write().await;
        tokio::time::timeout(timeout, rx.recv()).await.ok().flatten()
    }

    /// Get connection status
    pub async fn status(&self) -> ConnectionStatus {
        *self.status.read().await
    }

    /// Check if connected
    pub async fn is_connected(&self) -> bool {
        *self.status.read().await == ConnectionStatus::Connected
    }

    /// Get current slot
    pub async fn current_slot(&self) -> u64 {
        *self.current_slot.read().await
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

    /// Get all pending slot updates
    pub async fn drain_slot_updates(&self) -> Vec<SlotUpdate> {
        let mut updates = Vec::new();
        let mut rx = self.slot_rx.write().await;

        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }

        updates
    }
}


/// Pool state cache updated by gRPC
pub struct PoolStateCache {
    states: Arc<RwLock<HashMap<Pubkey, CachedPoolData>>>,
}

#[derive(Clone)]
pub struct CachedPoolData {
    pub data: Vec<u8>,
    pub slot: u64,
    pub updated_at: Instant,
}

impl PoolStateCache {
    pub fn new() -> Self {
        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Update cache with new pool data
    pub async fn update(&self, update: PoolUpdate) {
        let mut states = self.states.write().await;
        states.insert(
            update.address,
            CachedPoolData {
                data: update.data,
                slot: update.slot,
                updated_at: update.timestamp,
            },
        );
    }

    /// Get cached pool data
    pub async fn get(&self, address: &Pubkey) -> Option<CachedPoolData> {
        let states = self.states.read().await;
        states.get(address).cloned()
    }

    /// Get all cached pool data
    pub async fn get_all(&self) -> HashMap<Pubkey, CachedPoolData> {
        self.states.read().await.clone()
    }

    /// Check if data is stale (older than max_age)
    pub async fn is_stale(&self, address: &Pubkey, max_age: Duration) -> bool {
        let states = self.states.read().await;
        match states.get(address) {
            Some(cached) => cached.updated_at.elapsed() > max_age,
            None => true,
        }
    }

    /// Clear all cached data
    pub async fn clear(&self) {
        let mut states = self.states.write().await;
        states.clear();
    }

    /// Get number of cached pools
    pub async fn len(&self) -> usize {
        self.states.read().await.len()
    }

    /// Check if cache is empty
    pub async fn is_empty(&self) -> bool {
        self.states.read().await.is_empty()
    }
}

impl Default for PoolStateCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Transaction confirmation tracker via gRPC
pub struct TxConfirmationTracker {
    pending: Arc<RwLock<HashMap<String, TxStatus>>>,
}

#[derive(Debug, Clone)]
pub struct TxStatus {
    pub signature: String,
    pub submitted_at: Instant,
    pub slot_submitted: u64,
    pub status: TxConfirmationStatus,
    pub confirmed_slot: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxConfirmationStatus {
    Pending,
    Processed,
    Confirmed,
    Finalized,
    Failed,
    Expired,
}

impl TxConfirmationTracker {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Track a new transaction
    pub async fn track(&self, signature: String, slot: u64) {
        let mut pending = self.pending.write().await;
        pending.insert(
            signature.clone(),
            TxStatus {
                signature,
                submitted_at: Instant::now(),
                slot_submitted: slot,
                status: TxConfirmationStatus::Pending,
                confirmed_slot: None,
                error: None,
            },
        );
    }

    /// Update transaction status
    pub async fn update_status(
        &self,
        signature: &str,
        status: TxConfirmationStatus,
        slot: Option<u64>,
        error: Option<String>,
    ) {
        let mut pending = self.pending.write().await;
        if let Some(tx) = pending.get_mut(signature) {
            tx.status = status;
            tx.confirmed_slot = slot;
            tx.error = error;
        }
    }

    /// Get transaction status
    pub async fn get_status(&self, signature: &str) -> Option<TxStatus> {
        let pending = self.pending.read().await;
        pending.get(signature).cloned()
    }

    /// Check if transaction is confirmed
    pub async fn is_confirmed(&self, signature: &str) -> bool {
        let pending = self.pending.read().await;
        pending
            .get(signature)
            .map(|tx| {
                matches!(
                    tx.status,
                    TxConfirmationStatus::Confirmed | TxConfirmationStatus::Finalized
                )
            })
            .unwrap_or(false)
    }

    /// Remove old/completed transactions
    pub async fn cleanup(&self, max_age: Duration) {
        let mut pending = self.pending.write().await;
        pending.retain(|_, tx| {
            tx.submitted_at.elapsed() < max_age
                && !matches!(
                    tx.status,
                    TxConfirmationStatus::Finalized | TxConfirmationStatus::Failed
                )
        });
    }

    /// Mark expired transactions
    pub async fn mark_expired(&self, current_slot: u64, max_slot_age: u64) {
        let mut pending = self.pending.write().await;
        for tx in pending.values_mut() {
            if tx.status == TxConfirmationStatus::Pending
                && current_slot > tx.slot_submitted + max_slot_age
            {
                tx.status = TxConfirmationStatus::Expired;
            }
        }
    }
}

impl Default for TxConfirmationTracker {
    fn default() -> Self {
        Self::new()
    }
}


/// Utility to get latest blockhash via gRPC (faster than RPC)
pub struct GrpcBlockhashFetcher {
    endpoint: String,
    x_token: Option<String>,
}

impl GrpcBlockhashFetcher {
    pub fn new(endpoint: &str, x_token: Option<&str>) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            x_token: x_token.map(|s| s.to_string()),
        }
    }

    /// Build a gRPC client with TLS support
    async fn build_client(
        &self,
    ) -> Result<
        GeyserGrpcClient<impl yellowstone_grpc_client::Interceptor + Clone>,
    > {
        let builder = GeyserGrpcClient::build_from_shared(self.endpoint.clone())
            .map_err(|e| anyhow!("Failed to build client: {}", e))?
            .x_token(self.x_token.clone())
            .map_err(|e| anyhow!("Failed to set x_token: {}", e))?
            .connect_timeout(Duration::from_secs(5));

        Ok(builder.connect().await
            .map_err(|e| anyhow!("Failed to connect: {}", e))?)
    }

    /// Get latest blockhash via gRPC
    /// This is typically faster than RPC for time-sensitive operations
    pub async fn get_latest_blockhash(&self) -> Result<(String, u64)> {
        let mut client = self.build_client().await?;

        let response = client
            .get_latest_blockhash(Some(CommitmentLevel::Processed))
            .await
            .map_err(|e| anyhow!("Failed to get blockhash: {}", e))?;

        Ok((response.blockhash, response.last_valid_block_height))
    }

    /// Get current slot via gRPC
    pub async fn get_slot(&self) -> Result<u64> {
        let mut client = self.build_client().await?;
        let response = client
            .get_slot(Some(CommitmentLevel::Processed))
            .await
            .map_err(|e| anyhow!("Failed to get slot: {}", e))?;

        Ok(response.slot)
    }

    /// Check if blockhash is still valid
    pub async fn is_blockhash_valid(&self, blockhash: &str) -> Result<bool> {
        let mut client = self.build_client().await?;
        let response = client
            .is_blockhash_valid(blockhash.to_string(), Some(CommitmentLevel::Processed))
            .await
            .map_err(|e| anyhow!("Failed to check blockhash: {}", e))?;

        Ok(response.valid)
    }
}
