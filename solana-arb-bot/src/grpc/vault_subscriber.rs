//! Vault Account Subscriber
//! Subscribes to vault/reserve accounts for pools that store reserves separately
//! (DLMM, DAMM V2, CPMM)

use anyhow::{anyhow, Result};
use futures::StreamExt;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts,
};

/// Vault balance update
#[derive(Debug, Clone)]
pub struct VaultUpdate {
    pub vault_address: Pubkey,
    pub pool_address: Pubkey,
    pub balance: u64,
    pub slot: u64,
    pub timestamp: Instant,
}

/// Mapping of vault address to pool address
pub struct VaultMapping {
    pub vault_to_pool: HashMap<Pubkey, Pubkey>,
    pub pool_to_vaults: HashMap<Pubkey, Vec<Pubkey>>,
}

impl VaultMapping {
    pub fn new() -> Self {
        Self {
            vault_to_pool: HashMap::new(),
            pool_to_vaults: HashMap::new(),
        }
    }

    pub fn add_vault(&mut self, vault: Pubkey, pool: Pubkey) {
        self.vault_to_pool.insert(vault, pool);
        self.pool_to_vaults
            .entry(pool)
            .or_insert_with(Vec::new)
            .push(vault);
    }

    pub fn get_pool(&self, vault: &Pubkey) -> Option<&Pubkey> {
        self.vault_to_pool.get(vault)
    }
}

impl Default for VaultMapping {
    fn default() -> Self {
        Self::new()
    }
}

/// Vault subscriber for real-time reserve updates
pub struct VaultSubscriber {
    endpoint: String,
    x_token: Option<String>,
    vault_mapping: Arc<RwLock<VaultMapping>>,
    updates_tx: mpsc::Sender<VaultUpdate>,
    updates_rx: Arc<RwLock<mpsc::Receiver<VaultUpdate>>>,
    is_connected: Arc<RwLock<bool>>,
}

impl VaultSubscriber {
    pub fn new(endpoint: &str, x_token: Option<&str>) -> Self {
        let (tx, rx) = mpsc::channel(1000);

        Self {
            endpoint: endpoint.to_string(),
            x_token: x_token.map(|s| s.to_string()),
            vault_mapping: Arc::new(RwLock::new(VaultMapping::new())),
            updates_tx: tx,
            updates_rx: Arc::new(RwLock::new(rx)),
            is_connected: Arc::new(RwLock::new(false)),
        }
    }

    /// Add vault accounts to subscribe to
    pub async fn add_vaults(&self, vaults: Vec<(Pubkey, Pubkey)>) {
        let mut mapping = self.vault_mapping.write().await;
        for (vault, pool) in vaults {
            mapping.add_vault(vault, pool);
        }
    }

    /// Connect and start subscribing
    pub async fn connect(&self) -> Result<()> {
        let mapping = self.vault_mapping.read().await;
        let vault_addresses: Vec<Pubkey> = mapping.vault_to_pool.keys().cloned().collect();
        drop(mapping);

        if vault_addresses.is_empty() {
            tracing::info!("📡 No vault accounts to subscribe to");
            return Ok(());
        }

        tracing::info!(
            "🔌 Connecting vault subscriber to gRPC: {}",
            self.endpoint
        );

        let mut client = GeyserGrpcClient::build_from_shared(self.endpoint.clone())
            .map_err(|e| anyhow!("Failed to build gRPC client: {}", e))?
            .x_token(self.x_token.clone())
            .map_err(|e| anyhow!("Failed to set x_token: {}", e))?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .max_decoding_message_size(1024 * 1024 * 1024) // 1GB
            .connect()
            .await
            .map_err(|e| anyhow!("Failed to connect: {}", e))?;

        *self.is_connected.write().await = true;

        // Build subscription request
        let request = self.build_subscribe_request(&vault_addresses);

        let (_subscribe_tx, mut stream) = client
            .subscribe_with_request(Some(request))
            .await
            .map_err(|e| anyhow!("Failed to subscribe: {}", e))?;

        tracing::info!(
            "📡 Subscribed to {} vault accounts",
            vault_addresses.len()
        );

        // Process updates
        let updates_tx = self.updates_tx.clone();
        let is_connected = self.is_connected.clone();
        let vault_mapping = self.vault_mapping.clone();

        tokio::spawn(async move {
            while let Some(update_result) = stream.next().await {
                match update_result {
                    Ok(update) => {
                        if let Some(vault_update) =
                            Self::process_update(&update, &vault_mapping).await
                        {
                            if updates_tx.send(vault_update).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Vault gRPC stream error: {}", e);
                        *is_connected.write().await = false;
                        break;
                    }
                }
            }
            *is_connected.write().await = false;
        });

        Ok(())
    }

    fn build_subscribe_request(&self, vault_addresses: &[Pubkey]) -> SubscribeRequest {
        let mut accounts_filter = HashMap::new();

        let account_keys: Vec<String> = vault_addresses.iter().map(|p| p.to_string()).collect();

        accounts_filter.insert(
            "vaults".to_string(),
            SubscribeRequestFilterAccounts {
                account: account_keys,
                owner: vec![],
                filters: vec![],
                nonempty_txn_signature: None,
            },
        );

        SubscribeRequest {
            accounts: accounts_filter,
            slots: HashMap::new(),
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

    async fn process_update(
        update: &yellowstone_grpc_proto::geyser::SubscribeUpdate,
        vault_mapping: &Arc<RwLock<VaultMapping>>,
    ) -> Option<VaultUpdate> {
        match &update.update_oneof {
            Some(UpdateOneof::Account(account_update)) => {
                let account = account_update.account.as_ref()?;
                let pubkey_bytes: [u8; 32] = account.pubkey.clone().try_into().ok()?;
                let vault_address = Pubkey::new_from_array(pubkey_bytes);

                let mapping = vault_mapping.read().await;
                let pool_address = mapping.get_pool(&vault_address)?.clone();
                drop(mapping);

                // Parse token account balance
                // Token account data: 64 bytes for mint, 32 for owner, 8 for amount
                let data: &[u8] = &account.data;
                if data.len() < 72 {
                    return None;
                }

                let balance = u64::from_le_bytes(data[64..72].try_into().ok()?);

                Some(VaultUpdate {
                    vault_address,
                    pool_address,
                    balance,
                    slot: account_update.slot,
                    timestamp: Instant::now(),
                })
            }
            _ => None,
        }
    }

    /// Get pending vault updates
    pub async fn drain_updates(&self) -> Vec<VaultUpdate> {
        let mut updates = Vec::new();
        let mut rx = self.updates_rx.write().await;
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }
        updates
    }

    /// Check if connected
    pub async fn is_connected(&self) -> bool {
        *self.is_connected.read().await
    }
}

/// Cache for vault balances
pub struct VaultBalanceCache {
    balances: Arc<RwLock<HashMap<Pubkey, CachedVaultBalance>>>,
}

#[derive(Clone)]
pub struct CachedVaultBalance {
    pub balance: u64,
    pub slot: u64,
    pub updated_at: Instant,
}

impl VaultBalanceCache {
    pub fn new() -> Self {
        Self {
            balances: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn update(&self, update: VaultUpdate) {
        let mut balances = self.balances.write().await;
        balances.insert(
            update.vault_address,
            CachedVaultBalance {
                balance: update.balance,
                slot: update.slot,
                updated_at: update.timestamp,
            },
        );
    }

    pub async fn get(&self, vault: &Pubkey) -> Option<CachedVaultBalance> {
        let balances = self.balances.read().await;
        balances.get(vault).cloned()
    }

    pub async fn get_pool_balances(&self, vaults: &[Pubkey]) -> Vec<u64> {
        let balances = self.balances.read().await;
        vaults
            .iter()
            .map(|v| balances.get(v).map(|b| b.balance).unwrap_or(0))
            .collect()
    }
}

impl Default for VaultBalanceCache {
    fn default() -> Self {
        Self::new()
    }
}
