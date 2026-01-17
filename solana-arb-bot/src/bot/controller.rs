//! Bot Controller
//! Main bot loop that orchestrates arbitrage detection and execution

use anyhow::{anyhow, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::{EncodableKey, Keypair}, signer::Signer};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// How often to update baseline prices for emergency drop detection (3 minutes)
const BASELINE_UPDATE_INTERVAL_SECS: u64 = 180;

use crate::arbitrage::{ArbExecutor, ArbitrageDetector, Opportunity};
use crate::config::Settings;
use crate::grpc::{
    ConnectionStatus, GrpcSubscriber, PoolStateCache, VaultBalanceCache, VaultSubscriber,
    WsSubscriber,
};
use crate::pools::{
    CpmmAdapter, DammV2Adapter, DlmmAdapter, PoolAdapter, PoolState, PoolType, PumpSwapAdapter,
    ValidatedPool,
};

use super::emergency::{EmergencyReason, EmergencySellHandler};
use super::pnl_tracker::{PnlTracker, TradeRecord};
use super::session::SessionState;
use super::status_printer::{self, StatusPrinter};

/// Bot status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotStatus {
    Starting,
    Running,
    Paused,
    Stopping,
    Stopped,
    EmergencySell,
}

/// Bot controller - main orchestrator
pub struct BotController {
    // Core components
    rpc: Arc<RpcClient>,
    wallet: Keypair,
    settings: Settings,
    
    // Adapters
    pumpswap: PumpSwapAdapter,
    dlmm: DlmmAdapter,
    damm_v2: DammV2Adapter,
    cpmm: CpmmAdapter,
    whirlpool: crate::pools::WhirlpoolAdapter,
    
    // Arbitrage components
    detector: ArbitrageDetector,
    executor: ArbExecutor,
    
    // State tracking
    session: Arc<RwLock<SessionState>>,
    pnl_tracker: Arc<RwLock<PnlTracker>>,
    emergency_handler: EmergencySellHandler,
    
    // Pool state
    validated_pools: Vec<ValidatedPool>,
    target_token: Pubkey,
    baseline_prices: Vec<(Pubkey, f64)>,
    baseline_updated_at: Instant,
    
    // Control flags
    status: Arc<RwLock<BotStatus>>,
    pub should_stop: Arc<AtomicBool>,
    
    // gRPC components (optional)
    grpc_subscriber: Option<GrpcSubscriber>,
    vault_subscriber: Option<VaultSubscriber>,
    pool_cache: PoolStateCache,
    vault_cache: VaultBalanceCache,
    last_grpc_reconnect_attempt: Instant,
    
    // WebSocket subscriber (alternative to gRPC)
    ws_subscriber: Option<WsSubscriber>,
    
    // Trade cooldown tracking
    last_trade_time: Instant,
}

impl BotController {
    /// Create new bot controller
    pub async fn new(
        settings: Settings,
        validated_pools: Vec<ValidatedPool>,
        fresh_session: bool,
    ) -> Result<Self> {
        // Load wallet from private_key (base58) or keypair_path (JSON file)
        let wallet = if !settings.wallet.private_key.is_empty() {
            // Load from base58 private key
            let secret_bytes = bs58::decode(&settings.wallet.private_key)
                .into_vec()
                .map_err(|e| anyhow!("Invalid base58 private key: {}", e))?;
            Keypair::from_bytes(&secret_bytes)
                .map_err(|e| anyhow!("Invalid private key bytes: {}", e))?
        } else {
            // Load from keypair JSON file
            let keypair_path = shellexpand::tilde(&settings.wallet.keypair_path).to_string();
            Keypair::read_from_file(&keypair_path)
                .map_err(|e| anyhow!("Failed to load wallet from file: {}", e))?
        };
        
        tracing::info!("💰 Wallet loaded: {}", wallet.pubkey());
        
        // Create RPC client (use dedicated RPC endpoint, NOT Astralane)
        let rpc = Arc::new(RpcClient::new_with_timeout(
            settings.rpc.endpoint.clone(),
            Duration::from_secs(settings.rpc.timeout_secs),
        ));
        
        // Check wallet balance
        let balance = rpc.get_balance(&wallet.pubkey())?;
        tracing::info!("💰 Wallet balance: {:.4} SOL", balance as f64 / 1e9);
        
        // Parse target token
        let target_token = Pubkey::from_str(&settings.pools.target_token)?;
        
        // Create adapters
        let pumpswap = PumpSwapAdapter::new(rpc.clone());
        let dlmm = DlmmAdapter::new(rpc.clone());
        let damm_v2 = DammV2Adapter::new(rpc.clone());
        let cpmm = CpmmAdapter::new(rpc.clone());
        let whirlpool = crate::pools::WhirlpoolAdapter::new(rpc.clone());
        
        // Create arbitrage components
        let detector = ArbitrageDetector::new(&settings);
        let executor = ArbExecutor::new(rpc.clone(), &settings)?;
        
        // Load or create session
        let session = if fresh_session {
            SessionState::new()
        } else {
            SessionState::load_or_create(Some(&settings.logging.session_path))
        };
        
        // Create P&L tracker
        let mut pnl_tracker = PnlTracker::new(
            settings.limits.max_loss_sol,
            &settings.logging.trade_log_path,
        );
        
        // Restore P&L state from session
        if !fresh_session {
            pnl_tracker.restore_from_session(
                session.total_pnl_sol,
                session.total_fees_sol,
                session.trade_count,
            );
        }
        
        // Create emergency handler
        let emergency_handler = EmergencySellHandler::new(settings.slippage.emergency_slippage_bps);
        
        Ok(Self {
            rpc,
            wallet,
            settings,
            pumpswap,
            dlmm,
            damm_v2,
            cpmm,
            whirlpool,
            detector,
            executor,
            session: Arc::new(RwLock::new(session)),
            pnl_tracker: Arc::new(RwLock::new(pnl_tracker)),
            emergency_handler,
            validated_pools,
            target_token,
            baseline_prices: Vec::new(),
            baseline_updated_at: Instant::now(),
            status: Arc::new(RwLock::new(BotStatus::Starting)),
            should_stop: Arc::new(AtomicBool::new(false)),
            grpc_subscriber: None,
            vault_subscriber: None,
            pool_cache: PoolStateCache::new(),
            vault_cache: VaultBalanceCache::new(),
            last_grpc_reconnect_attempt: Instant::now(),
            ws_subscriber: None,
            last_trade_time: Instant::now() - Duration::from_secs(60), // Allow immediate first trade
        })
    }
    
    /// Initialize gRPC subscriber for real-time updates
    pub async fn init_grpc(&mut self) -> Result<()> {
        // Try gRPC first
        if !self.settings.grpc.endpoint.is_empty() {
            return self.init_grpc_internal().await;
        }
        
        // Fall back to WebSocket if configured
        if !self.settings.grpc.ws_endpoint.is_empty() {
            return self.init_websocket().await;
        }
        
        tracing::info!("📡 No gRPC or WebSocket configured, using RPC polling");
        Ok(())
    }
    
    /// Initialize gRPC subscriber (internal)
    async fn init_grpc_internal(&mut self) -> Result<()> {
        if self.settings.grpc.endpoint.is_empty() {
            return Ok(());
        }
        
        let pool_addresses: Vec<Pubkey> = self.validated_pools
            .iter()
            .map(|p| p.address)
            .collect();
        
        let x_token = if self.settings.grpc.x_token.is_empty() {
            None
        } else {
            Some(self.settings.grpc.x_token.as_str())
        };
        
        // Initialize pool subscriber
        let subscriber = GrpcSubscriber::new(
            &self.settings.grpc.endpoint,
            x_token,
            pool_addresses,
        );
        
        // Connect with automatic reconnection
        subscriber.connect_with_reconnect().await?;
        self.grpc_subscriber = Some(subscriber);
        
        tracing::info!("📡 gRPC pool subscriber initialized");
        
        // Initialize vault subscriber for pools with separate reserve accounts
        // (DLMM, DAMM V2, CPMM store reserves in vault accounts, not in pool account)
        self.init_vault_subscriber().await?;
        
        Ok(())
    }
    
    /// Initialize WebSocket subscriber (alternative to gRPC)
    async fn init_websocket(&mut self) -> Result<()> {
        let pool_addresses: Vec<Pubkey> = self.validated_pools
            .iter()
            .map(|p| p.address)
            .collect();
        
        let subscriber = WsSubscriber::new(
            &self.settings.grpc.ws_endpoint,
            pool_addresses,
        );
        
        subscriber.connect_with_reconnect().await?;
        self.ws_subscriber = Some(subscriber);
        
        tracing::info!("📡 WebSocket subscriber initialized");
        Ok(())
    }
    
    /// Initialize vault subscriber for real-time reserve updates
    async fn init_vault_subscriber(&mut self) -> Result<()> {
        let x_token = if self.settings.grpc.x_token.is_empty() {
            None
        } else {
            Some(self.settings.grpc.x_token.as_str())
        };
        
        let vault_subscriber = VaultSubscriber::new(&self.settings.grpc.endpoint, x_token);
        
        // Collect vault addresses from pools that need them
        let mut vault_mappings: Vec<(Pubkey, Pubkey)> = Vec::new();
        
        for pool in &self.validated_pools {
            match pool.pool_type {
                PoolType::MeteoraDLMM | PoolType::MeteoraDammV2 | PoolType::RaydiumCPMM | PoolType::OrcaWhirlpool => {
                    // Fetch pool state to get vault addresses
                    if let Some(adapter) = self.get_adapter(pool.pool_type) {
                        if let Ok(_state) = adapter.get_pool_state(&pool.address).await {
                            // For these pool types, we need to subscribe to the vault accounts
                            // The vault addresses are stored in the pool state
                            // We'll extract them from the raw pool data
                            if let Some(vaults) = self.extract_vault_addresses(&pool.address, pool.pool_type).await {
                                for vault in vaults {
                                    vault_mappings.push((vault, pool.address));
                                }
                            }
                        }
                    }
                }
                _ => {
                    // PumpSwap stores reserves directly in pool account, no vault subscription needed
                }
            }
        }
        
        if vault_mappings.is_empty() {
            tracing::info!("📡 No vault accounts to subscribe to");
            return Ok(());
        }
        
        vault_subscriber.add_vaults(vault_mappings.clone()).await;
        
        if let Err(e) = vault_subscriber.connect().await {
            tracing::warn!("⚠️ Failed to connect vault subscriber: {}", e);
            return Ok(()); // Non-fatal, will fall back to RPC
        }
        
        self.vault_subscriber = Some(vault_subscriber);
        tracing::info!("📡 Vault subscriber initialized for {} vault accounts", vault_mappings.len());
        
        Ok(())
    }
    
    /// Extract vault addresses from pool account
    async fn extract_vault_addresses(&self, pool_address: &Pubkey, pool_type: PoolType) -> Option<Vec<Pubkey>> {
        let account = self.rpc.get_account(pool_address).ok()?;
        let data = &account.data;
        
        match pool_type {
            PoolType::MeteoraDLMM => {
                // DLMM LbPairState layout (after 8-byte discriminator):
                // parameters: 32 bytes (offset 0)
                // v_parameters: 32 bytes (offset 32)
                // bump_seed + bin_step_seed + pair_type + active_id + bin_step + status + etc: 16 bytes (offset 64)
                // token_x_mint: 32 bytes (offset 80)
                // token_y_mint: 32 bytes (offset 112)
                // reserve_x: 32 bytes (offset 144)
                // reserve_y: 32 bytes (offset 176)
                if data.len() >= 8 + 176 + 32 {
                    let reserve_x = Pubkey::try_from(&data[8 + 144..8 + 144 + 32]).ok()?;
                    let reserve_y = Pubkey::try_from(&data[8 + 176..8 + 176 + 32]).ok()?;
                    Some(vec![reserve_x, reserve_y])
                } else {
                    None
                }
            }
            PoolType::MeteoraDammV2 => {
                // DAMM V2 layout (after 8-byte discriminator):
                // pool_fees: 160 bytes (offset 0)
                // token_a_mint: 32 bytes (offset 160)
                // token_b_mint: 32 bytes (offset 192)
                // token_a_vault: 32 bytes (offset 224)
                // token_b_vault: 32 bytes (offset 256)
                if data.len() >= 8 + 256 + 32 {
                    let vault_a = Pubkey::try_from(&data[8 + 224..8 + 224 + 32]).ok()?;
                    let vault_b = Pubkey::try_from(&data[8 + 256..8 + 256 + 32]).ok()?;
                    Some(vec![vault_a, vault_b])
                } else {
                    None
                }
            }
            PoolType::RaydiumCPMM => {
                // CPMM CpmmPoolState layout (after 8-byte discriminator):
                // amm_config: 32 bytes (offset 0)
                // pool_creator: 32 bytes (offset 32)
                // token_0_vault: 32 bytes (offset 64)
                // token_1_vault: 32 bytes (offset 96)
                if data.len() >= 8 + 96 + 32 {
                    let vault_0 = Pubkey::try_from(&data[8 + 64..8 + 64 + 32]).ok()?;
                    let vault_1 = Pubkey::try_from(&data[8 + 96..8 + 96 + 32]).ok()?;
                    Some(vec![vault_0, vault_1])
                } else {
                    None
                }
            }
            PoolType::OrcaWhirlpool => {
                // Whirlpool layout (after 8-byte discriminator):
                // token_vault_a at offset 133 (8 + 32 + 1 + 2 + 2 + 2 + 2 + 16 + 16 + 4 + 8 + 8 + 32 = 133)
                // token_vault_b at offset 213 (133 + 32 + 16 + 32 = 213)
                if data.len() >= 653 {
                    let vault_a = Pubkey::try_from(&data[133..133 + 32]).ok()?;
                    let vault_b = Pubkey::try_from(&data[213..213 + 32]).ok()?;
                    Some(vec![vault_a, vault_b])
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    
    /// Check gRPC/WebSocket connection health and attempt reconnection if needed
    async fn check_grpc_health(&mut self) -> bool {
        // Check gRPC first
        if let Some(ref subscriber) = self.grpc_subscriber {
            match subscriber.status().await {
                ConnectionStatus::Connected => return true,
                ConnectionStatus::Reconnecting => {
                    tracing::warn!("⚠️ gRPC reconnecting...");
                    return false;
                }
                ConnectionStatus::Disconnected | ConnectionStatus::Failed => {
                    // Attempt reconnection if enough time has passed (30 seconds cooldown)
                    if self.last_grpc_reconnect_attempt.elapsed().as_secs() >= 30 {
                        tracing::info!("🔄 Attempting gRPC reconnection...");
                        self.last_grpc_reconnect_attempt = Instant::now();
                        
                        if let Err(e) = subscriber.connect_with_reconnect().await {
                            tracing::error!("❌ gRPC reconnection failed: {}", e);
                        } else {
                            tracing::info!("✅ gRPC reconnected successfully");
                            return true;
                        }
                    }
                    return false;
                }
                status => {
                    tracing::warn!("⚠️ gRPC status: {:?}", status);
                    return false;
                }
            }
        }
        
        // Check WebSocket if no gRPC
        if let Some(ref ws_sub) = self.ws_subscriber {
            match ws_sub.status().await {
                ConnectionStatus::Connected => return true,
                ConnectionStatus::Reconnecting => {
                    tracing::warn!("⚠️ WebSocket reconnecting...");
                    return false;
                }
                ConnectionStatus::Disconnected | ConnectionStatus::Failed => {
                    // Attempt reconnection if enough time has passed (30 seconds cooldown)
                    if self.last_grpc_reconnect_attempt.elapsed().as_secs() >= 30 {
                        tracing::info!("🔄 Attempting WebSocket reconnection...");
                        self.last_grpc_reconnect_attempt = Instant::now();
                        
                        if let Err(e) = ws_sub.connect_with_reconnect().await {
                            tracing::error!("❌ WebSocket reconnection failed: {}", e);
                        } else {
                            tracing::info!("✅ WebSocket reconnected successfully");
                            return true;
                        }
                    }
                    return false;
                }
                status => {
                    tracing::warn!("⚠️ WebSocket status: {:?}", status);
                    return false;
                }
            }
        }
        
        true // No gRPC/WebSocket configured, always "healthy" (using RPC polling)
    }
    
    /// Get current slot from gRPC (faster than RPC)
    async fn get_current_slot(&self) -> Option<u64> {
        if let Some(ref subscriber) = self.grpc_subscriber {
            let slot = subscriber.current_slot().await;
            if slot > 0 {
                return Some(slot);
            }
        }
        None
    }
    
    /// Get adapter for pool type
    fn get_adapter(&self, pool_type: PoolType) -> Option<&dyn PoolAdapter> {
        match pool_type {
            PoolType::PumpSwap => Some(&self.pumpswap),
            PoolType::MeteoraDLMM => Some(&self.dlmm),
            PoolType::MeteoraDammV2 => Some(&self.damm_v2),
            PoolType::RaydiumCPMM => Some(&self.cpmm),
            PoolType::OrcaWhirlpool => Some(&self.whirlpool),
            PoolType::Unsupported => None,
        }
    }
    
    /// Fetch current pool states (uses gRPC/WebSocket cache if available, falls back to RPC)
    async fn fetch_pool_states(&self) -> Result<Vec<PoolState>> {
        // Process any pending gRPC pool updates
        if let Some(ref subscriber) = self.grpc_subscriber {
            let updates = subscriber.drain_updates().await;
            for update in updates {
                self.pool_cache.update(update).await;
            }
        }
        
        // Process any pending WebSocket pool updates
        if let Some(ref ws_sub) = self.ws_subscriber {
            let updates = ws_sub.drain_updates().await;
            for update in updates {
                self.pool_cache.update(update).await;
            }
        }
        
        // Process any pending vault updates
        if let Some(ref vault_sub) = self.vault_subscriber {
            let vault_updates = vault_sub.drain_updates().await;
            for update in vault_updates {
                self.vault_cache.update(update).await;
            }
        }
        
        let mut states = Vec::new();
        let max_cache_age = Duration::from_millis(500); // 500ms cache for fast updates
        
        for pool in &self.validated_pools {
            let mut state_fetched = false;
            
            // Try gRPC cache first (much faster than RPC)
            if self.grpc_subscriber.is_some() {
                if !self.pool_cache.is_stale(&pool.address, max_cache_age).await {
                    if let Some(cached) = self.pool_cache.get(&pool.address).await {
                        // Try to parse cached data using appropriate adapter
                        if let Some(adapter) = self.get_adapter(pool.pool_type) {
                            if let Some(mut state) = adapter.parse_pool_state_from_data(&pool.address, &cached.data) {
                                // For pools with separate vault accounts, try to get reserves from vault cache
                                if state.token_a_reserve == 0 || state.token_b_reserve == 0 {
                                    if let Some(vaults) = self.extract_vault_addresses(&pool.address, pool.pool_type).await {
                                        let balances = self.vault_cache.get_pool_balances(&vaults).await;
                                        if balances.len() >= 2 && balances[0] > 0 && balances[1] > 0 {
                                            state.token_a_reserve = balances[0];
                                            state.token_b_reserve = balances[1];
                                            // Recalculate price and liquidity
                                            if state.token_a_reserve > 0 {
                                                state.price = state.token_b_reserve as f64 / state.token_a_reserve as f64;
                                            }
                                            state.liquidity_sol = self.calculate_liquidity_sol(&state);
                                            tracing::trace!("📡 Used vault cache for pool {} reserves", pool.address);
                                        } else {
                                            // Vault cache miss, fetch via RPC
                                            if let Ok(full_state) = adapter.get_pool_state(&pool.address).await {
                                                state = full_state;
                                            }
                                        }
                                    } else {
                                        // Couldn't extract vaults, fetch full state
                                        if let Ok(full_state) = adapter.get_pool_state(&pool.address).await {
                                            state = full_state;
                                        }
                                    }
                                }
                                states.push(state);
                                state_fetched = true;
                                tracing::trace!("📡 Used gRPC cache for pool {}", pool.address);
                            }
                        }
                    }
                }
            }
            
            // Fall back to RPC if gRPC cache miss or parse failed
            if !state_fetched {
                if let Some(adapter) = self.get_adapter(pool.pool_type) {
                    match adapter.get_pool_state(&pool.address).await {
                        Ok(state) => {
                            states.push(state);
                            tracing::trace!("📡 Used RPC for pool {}", pool.address);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to fetch pool state for {}: {}", pool.address, e);
                        }
                    }
                }
            }
        }
        
        Ok(states)
    }
    
    /// Calculate SOL liquidity from pool state
    fn calculate_liquidity_sol(&self, state: &PoolState) -> f64 {
        let sol_mint_str = "So11111111111111111111111111111111111111112";
        let sol_mint: Pubkey = sol_mint_str.parse().unwrap();
        
        if state.token_b_mint == sol_mint {
            state.token_b_reserve as f64 / 1e9
        } else if state.token_a_mint == sol_mint {
            state.token_a_reserve as f64 / 1e9
        } else {
            0.0
        }
    }

    /// Run the main bot loop
    pub async fn run(&mut self) -> Result<()> {
        // Print startup banner
        let balance = self.rpc.get_balance(&self.wallet.pubkey()).unwrap_or(0);
        status_printer::print_startup_banner(
            &self.target_token.to_string(),
            self.validated_pools.len(),
            &self.wallet.pubkey().to_string(),
            balance as f64 / 1e9,
        );
        
        // Initialize gRPC if configured
        if let Err(e) = self.init_grpc().await {
            tracing::warn!("⚠️ Failed to initialize gRPC, using RPC polling: {}", e);
        }
        
        // Set status to running
        *self.status.write().await = BotStatus::Running;
        
        // Fetch initial pool states and prices
        let initial_states = self.fetch_pool_states().await?;
        self.baseline_prices = initial_states
            .iter()
            .map(|s| (s.address, s.price))
            .collect();
        self.baseline_updated_at = Instant::now();
        
        tracing::info!("📊 Baseline prices captured for {} pools", self.baseline_prices.len());
        
        let check_interval = Duration::from_millis(self.settings.safety.check_interval_ms);
        let max_trades = self.settings.limits.max_trade_count;
        let mut status_printer = StatusPrinter::new(30); // Print full status every 30 seconds
        
        let mut grpc_health_check_counter = 0u32;
        
        loop {
            // Check stop flag
            if self.should_stop.load(Ordering::Relaxed) {
                tracing::info!("🛑 Stop signal received");
                break;
            }
            
            // Periodic gRPC health check and reconnection (every 100 iterations)
            grpc_health_check_counter += 1;
            if grpc_health_check_counter % 100 == 0 {
                if !self.check_grpc_health().await {
                    tracing::debug!("📡 gRPC unavailable, using RPC fallback");
                }
            }
            
            // Check trade count limit
            {
                let session = self.session.read().await;
                if session.trade_count >= max_trades {
                    tracing::info!("📊 Max trade count ({}) reached, stopping", max_trades);
                    break;
                }
            }
            
            // Check max loss
            {
                let pnl = self.pnl_tracker.read().await;
                if pnl.is_max_loss_reached() {
                    tracing::warn!("🚨 Max loss reached! Current P&L: {:.6} SOL", pnl.current_pnl());
                    *self.status.write().await = BotStatus::EmergencySell;
                    self.trigger_emergency_sell(EmergencyReason::MaxLossReached {
                        current_loss_sol: -pnl.current_pnl(),
                    })
                    .await?;
                    break;
                }
            }
            
            // Fetch current pool states
            let pool_states = match self.fetch_pool_states().await {
                Ok(states) => states,
                Err(e) => {
                    tracing::error!("Failed to fetch pool states: {}", e);
                    tokio::time::sleep(check_interval).await;
                    continue;
                }
            };
            
            // Check for price drops against baseline
            let current_prices: Vec<(Pubkey, f64)> = pool_states
                .iter()
                .map(|s| (s.address, s.price))
                .collect();
            
            if let Some((pool, drop_percent)) = self.emergency_handler.check_price_drop(
                &current_prices,
                &self.baseline_prices,
                self.settings.safety.emergency_drop_percent,
            ) {
                tracing::warn!(
                    "🚨 Price drop detected! Pool {} dropped {:.2}%",
                    pool,
                    drop_percent
                );
                *self.status.write().await = BotStatus::EmergencySell;
                self.trigger_emergency_sell(EmergencyReason::PriceDrop { pool, drop_percent })
                    .await?;
                break;
            }
            
            // Periodically update baseline prices (every 3 minutes)
            // This prevents false emergencies from natural price drift over long sessions
            if self.baseline_updated_at.elapsed().as_secs() >= BASELINE_UPDATE_INTERVAL_SECS {
                self.baseline_prices = current_prices.clone();
                self.baseline_updated_at = Instant::now();
                tracing::info!("📊 Baseline prices updated (3-minute refresh)");
            }
            
            // Find arbitrage opportunity
            if let Some(opportunity) = self.detector.find_best_opportunity(&pool_states, &self.target_token) {
                // Check trade cooldown - prevent double-spending by waiting for previous tx to land
                let cooldown = Duration::from_millis(self.settings.safety.trade_cooldown_ms);
                let time_since_last_trade = self.last_trade_time.elapsed();
                
                if time_since_last_trade < cooldown {
                    let remaining = cooldown - time_since_last_trade;
                    tracing::debug!(
                        "⏳ Trade cooldown: {:.1}s remaining (prevents double-spend)",
                        remaining.as_secs_f64()
                    );
                    // Sleep for the remaining cooldown time, then re-check opportunity
                    // (opportunity may be stale after waiting)
                    tokio::time::sleep(remaining).await;
                    continue;
                }
                
                status_printer::print_opportunity_found(
                    opportunity.spread_percent,
                    opportunity.profit_result.net_profit_sol,
                    &opportunity.buy_pool.address.to_string(),
                    &opportunity.sell_pool.address.to_string(),
                );
                
                // Check wallet balance before executing
                let required_sol = opportunity.recommended_sol_amount + 0.01; // Add buffer for fees
                let wallet_balance = self.get_wallet_balance_sol();
                
                if wallet_balance < required_sol {
                    tracing::warn!(
                        "⚠️ Insufficient balance: need {:.4} SOL, have {:.4} SOL - skipping trade",
                        required_sol,
                        wallet_balance
                    );
                    tokio::time::sleep(check_interval).await;
                    continue;
                }
                
                // Execute arbitrage
                match self.execute_arbitrage(&opportunity).await {
                    Ok(result) => {
                        // Print trade execution
                        status_printer::print_trade_execution(
                            &format!("{:?}", opportunity.buy_pool.pool_type),
                            &format!("{:?}", opportunity.sell_pool.pool_type),
                            opportunity.recommended_sol_amount,
                            opportunity.profit_result.tokens_bought as u64,
                            opportunity.profit_result.sol_received,
                            opportunity.profit_result.net_profit_sol,
                            opportunity.profit_result.total_fees_sol,
                            &result.signature,
                        );
                        
                        // Wait for transaction confirmation with processed commitment (fastest)
                        // This prevents double-spending by ensuring tx lands before next trade
                        let confirmation_timeout = self.settings.safety.trade_cooldown_ms;
                        tracing::info!(
                            "⏳ Waiting for confirmation (processed commitment, {}ms timeout)...",
                            confirmation_timeout
                        );
                        
                        let confirmed = self.executor
                            .wait_for_confirmation(&result.signature, confirmation_timeout)
                            .await;
                        
                        if confirmed {
                            // Record successful trade
                            self.record_trade(&opportunity, &result.signature, true).await;
                            tracing::info!("✅ Trade confirmed and recorded");
                        } else {
                            // Transaction may have failed or timed out
                            tracing::warn!("⚠️ Transaction not confirmed within timeout - may have failed");
                            self.record_trade(&opportunity, &result.signature, false).await;
                        }
                        
                        // Update last trade time after confirmation attempt
                        self.last_trade_time = Instant::now();
                    }
                    Err(e) => {
                        tracing::error!("❌ Arbitrage execution failed: {}", e);
                        // Record failed trade with zero profit
                        self.record_trade(&opportunity, "failed", false).await;
                        
                        // Also apply cooldown on failed trades to prevent spam
                        self.last_trade_time = Instant::now();
                    }
                }
            } else {
                // Print activity dot when no opportunity
                status_printer.print_activity_dot();
            }
            
            // Periodic full status update
            if status_printer.should_print_full_status() {
                let session = self.session.read().await;
                let pnl = self.pnl_tracker.read().await;
                
                // Calculate spread
                let spread = if pool_states.len() >= 2 {
                    let prices: Vec<f64> = pool_states.iter().map(|p| p.price).collect();
                    let max_price = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                    let min_price = prices.iter().cloned().fold(f64::INFINITY, f64::min);
                    if min_price > 0.0 { ((max_price - min_price) / min_price) * 100.0 } else { 0.0 }
                } else {
                    0.0
                };
                
                status_printer::print_pool_prices(&pool_states, spread);
                status_printer::print_session_stats(
                    session.trade_count,
                    max_trades,
                    pnl.current_pnl(),
                    session.win_rate(),
                    pnl.total_fees(),
                    self.settings.limits.max_loss_sol + pnl.current_pnl(),
                );
            }
            
            // Wait before next check
            tokio::time::sleep(check_interval).await;
        }
        
        *self.status.write().await = BotStatus::Stopped;
        status_printer::print_bot_stopped("Normal shutdown");
        
        // Print final stats
        self.print_final_stats().await;
        
        Ok(())
    }
    
    /// Execute an arbitrage opportunity
    async fn execute_arbitrage(
        &self,
        opportunity: &Opportunity,
    ) -> Result<crate::arbitrage::executor::ArbResult> {
        self.executor
            .execute(
                &self.wallet,
                opportunity,
                self.settings.slippage.buy_slippage_bps,
                self.settings.slippage.sell_slippage_bps,
            )
            .await
    }
    
    /// Record a trade in session and P&L tracker atomically
    async fn record_trade(&self, opportunity: &Opportunity, signature: &str, success: bool) {
        let profit = if success {
            opportunity.profit_result.net_profit_sol
        } else {
            0.0
        };
        // Only count fees if trade was successful (fees weren't paid if tx failed)
        let fees = if success {
            opportunity.profit_result.total_fees_sol
        } else {
            0.0
        };
        let timestamp = chrono::Utc::now();
        
        // Acquire both locks together to ensure atomic update
        // This prevents inconsistency if bot crashes between updates
        let (mut session, mut pnl) = tokio::join!(
            self.session.write(),
            self.pnl_tracker.write()
        );
        
        // Update session
        session.record_trade(profit, fees, Some(&self.settings.logging.session_path));
        
        // Update P&L tracker
        pnl.record_trade(TradeRecord {
            timestamp,
            trade_type: "arb".to_string(),
            buy_pool: opportunity.buy_pool.address.to_string(),
            sell_pool: opportunity.sell_pool.address.to_string(),
            buy_pool_type: format!("{:?}", opportunity.buy_pool.pool_type),
            sell_pool_type: format!("{:?}", opportunity.sell_pool.pool_type),
            token: self.target_token.to_string(),
            amount_in_sol: opportunity.recommended_sol_amount,
            tokens_traded: opportunity.profit_result.tokens_bought as u64,
            amount_out_sol: opportunity.profit_result.sol_received,
            profit_sol: profit,
            fees_sol: fees,
            tx_sig: signature.to_string(),
            status: if success { "confirmed" } else { "failed" }.to_string(),
        });
    }
    
    /// Get wallet SOL balance
    fn get_wallet_balance_sol(&self) -> f64 {
        match self.rpc.get_balance(&self.wallet.pubkey()) {
            Ok(lamports) => lamports as f64 / 1e9,
            Err(_) => 0.0,
        }
    }
    
    /// Get token balance for the target token
    fn get_token_balance(&self) -> u64 {
        use spl_associated_token_account::get_associated_token_address;
        
        let wallet_pubkey = self.wallet.pubkey();
        let ata = get_associated_token_address(&wallet_pubkey, &self.target_token);
        
        match self.rpc.get_token_account_balance(&ata) {
            Ok(balance) => balance.amount.parse::<u64>().unwrap_or(0),
            Err(_) => 0,
        }
    }
    
    /// Trigger emergency sell
    async fn trigger_emergency_sell(&self, reason: EmergencyReason) -> Result<()> {
        tracing::warn!("🚨 Triggering emergency sell: {:?}", reason);
        
        // Get current pool states
        let pool_states = self.fetch_pool_states().await?;
        
        // Get actual token balance
        let token_balance = self.get_token_balance();
        tracing::info!("🚨 Current token balance: {}", token_balance);
        
        if token_balance > 0 {
            match self
                .emergency_handler
                .execute(
                    &self.executor,
                    &self.wallet,
                    &pool_states,
                    token_balance,
                    reason,
                    &self.target_token,
                )
                .await
            {
                Ok(sig) => {
                    tracing::info!("🚨 Emergency sell completed: {}", sig);
                }
                Err(e) => {
                    tracing::error!("🚨 Emergency sell failed: {}", e);
                }
            }
        } else {
            tracing::info!("🚨 No tokens to sell in emergency");
        }
        
        Ok(())
    }
    
    /// Signal the bot to stop
    pub fn stop(&self) {
        self.should_stop.store(true, Ordering::Relaxed);
    }
    
    /// Get current status
    pub async fn status(&self) -> BotStatus {
        *self.status.read().await
    }
    
    /// Print final statistics
    async fn print_final_stats(&self) {
        let session = self.session.read().await;
        let pnl = self.pnl_tracker.read().await;
        
        println!("\n╔══════════════════════════════════════════════════════════════════╗");
        println!("║                      FINAL SESSION STATS                         ║");
        println!("╠══════════════════════════════════════════════════════════════════╣");
        println!("║  Session ID: {}                    ║", &session.session_id[..8]);
        println!("║  Duration: {:?}                                          ║", session.duration());
        println!("║  Total Trades: {}                                              ║", session.trade_count);
        println!("║  Wins: {} | Losses: {}                                         ║", session.wins, session.losses);
        println!("║  Win Rate: {:.1}%                                              ║", session.win_rate());
        println!("║  Total P&L: {:.6} SOL                                      ║", pnl.current_pnl());
        println!("║  Total Fees: {:.6} SOL                                     ║", pnl.total_fees());
        println!("╚══════════════════════════════════════════════════════════════════╝\n");
    }
}
