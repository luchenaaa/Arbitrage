//! Arbitrage Executor
//! Builds and sends atomic arbitrage transactions via Astralane or Jito
//! Uses Versioned Transactions with Address Lookup Tables for size optimization

use anyhow::{anyhow, Result};
use base64::Engine;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    system_instruction,
    transaction::{Transaction, VersionedTransaction},
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Settings, TransactionMode};
use crate::pools::{
    CpmmAdapter, DammV2Adapter, DlmmAdapter, PoolAdapter, PoolState, PoolType, PumpSwapAdapter,
    WhirlpoolAdapter,
};

use super::detector::Opportunity;
use super::jito_sender::JitoSender;

/// Result of an arbitrage execution
#[derive(Debug)]
pub struct ArbResult {
    pub signature: String,
    pub buy_pool: Pubkey,
    pub sell_pool: Pubkey,
    pub sol_in: u64,
    pub expected_tokens: u64,
    pub expected_sol_out: u64,
}

/// Default token decimals (most SPL tokens use 6 or 9)
const DEFAULT_TOKEN_DECIMALS: u8 = 6;

/// Retry configuration
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub base_tip_lamports: u64,
    pub tip_multiplier: f64,
    pub max_tip_lamports: u64,
    pub retry_delay_ms: u64,
}

impl RetryConfig {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            max_retries: settings.retry.max_retries,
            base_tip_lamports: settings.fees.jito_tip_lamports,
            tip_multiplier: settings.retry.tip_multiplier,
            max_tip_lamports: settings.retry.max_tip_lamports,
            retry_delay_ms: settings.retry.retry_delay_ms,
        }
    }
}

/// Arbitrage executor - builds and sends atomic transactions
pub struct ArbExecutor {
    rpc: Arc<RpcClient>,
    http_client: reqwest::Client,
    
    /// Transaction submission mode
    transaction_mode: TransactionMode,
    
    // Astralane config
    astralane_endpoint: String,
    api_key: String,
    tip_wallets: Vec<Pubkey>,
    
    // Jito sender (for Jito mode)
    jito_sender: Option<JitoSender>,
    jito_tip_account: Option<Pubkey>,
    
    priority_fee: u64,
    retry_config: RetryConfig,
    
    /// Address Lookup Table for transaction size optimization
    lookup_table: Option<AddressLookupTableAccount>,
    
    // Pool adapters
    pumpswap: PumpSwapAdapter,
    dlmm: DlmmAdapter,
    damm_v2: DammV2Adapter,
    cpmm: CpmmAdapter,
    whirlpool: WhirlpoolAdapter,
}

impl ArbExecutor {
    pub fn new(rpc: Arc<RpcClient>, settings: &Settings) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(85))
            .build()?;
        
        let transaction_mode = settings.transaction_mode;
        
        // Parse Astralane tip wallets (may be empty if using Jito mode)
        let tip_wallets: Vec<Pubkey> = if !settings.astralane.tip_wallets.is_empty() {
            settings.astralane.tip_wallets
                .iter()
                .filter_map(|s| Pubkey::from_str(s).ok())
                .collect()
        } else {
            vec![]
        };
        
        // Create Jito sender if in Jito mode
        let (jito_sender, jito_tip_account) = if transaction_mode == TransactionMode::Jito {
            // Load auth keypair if configured for gRPC
            let auth_keypair = if !settings.jito.auth_keypair_path.is_empty() {
                let path = shellexpand::tilde(&settings.jito.auth_keypair_path).to_string();
                match std::fs::read_to_string(&path) {
                    Ok(json_str) => {
                        match serde_json::from_str::<Vec<u8>>(&json_str) {
                            Ok(bytes) => {
                                match solana_sdk::signature::Keypair::from_bytes(&bytes) {
                                    Ok(kp) => {
                                        tracing::info!("🔑 Loaded Jito auth keypair: {}", kp.pubkey());
                                        Some(Arc::new(kp))
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to parse auth keypair: {}", e);
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to parse auth keypair JSON: {}", e);
                                None
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to read auth keypair file: {}", e);
                        None
                    }
                }
            } else {
                None
            };
            
            let sender = JitoSender::new(rpc.clone(), settings, auth_keypair);
            let tip_account = Pubkey::from_str(&settings.jito.get_random_tip_account())
                .map_err(|e| anyhow!("Invalid Jito tip account: {}", e))?;
            tracing::info!("🚀 Using Jito bundle mode ({:?}) with {} endpoints", 
                settings.jito.protocol, settings.jito.endpoints.len());
            if settings.jito.parallel_send {
                tracing::info!("   Parallel send enabled (all endpoints)");
            }
            if !settings.jito.uuid.is_empty() {
                tracing::info!("   UUID configured (authenticated REST access)");
            }
            (Some(sender), Some(tip_account))
        } else {
            if tip_wallets.is_empty() {
                return Err(anyhow!("At least one Astralane tip wallet is required"));
            }
            tracing::info!("📍 Using Astralane mode with {} tip wallets", tip_wallets.len());
            (None, None)
        };
        
        // Load Address Lookup Table if configured
        let lookup_table = if !settings.astralane.lookup_table.is_empty() {
            match Self::load_lookup_table(&rpc, &settings.astralane.lookup_table) {
                Ok(alt) => {
                    tracing::info!("📋 Loaded ALT with {} addresses: {}", 
                        alt.addresses.len(), settings.astralane.lookup_table);
                    Some(alt)
                }
                Err(e) => {
                    tracing::warn!("⚠️ Failed to load ALT: {}. Transactions may fail if too large.", e);
                    None
                }
            }
        } else {
            tracing::info!("📋 No ALT configured. Large transactions (PumpSwap+DLMM) may fail.");
            None
        };
        
        Ok(Self {
            rpc: rpc.clone(),
            http_client,
            transaction_mode,
            astralane_endpoint: settings.astralane.endpoint.clone(),
            api_key: settings.astralane.api_key.clone(),
            tip_wallets,
            jito_sender,
            jito_tip_account,
            priority_fee: settings.fees.base_priority_lamports,
            retry_config: RetryConfig::from_settings(settings),
            lookup_table,
            pumpswap: PumpSwapAdapter::new(rpc.clone()),
            dlmm: DlmmAdapter::new(rpc.clone()),
            damm_v2: DammV2Adapter::new(rpc.clone()),
            cpmm: CpmmAdapter::new(rpc.clone()),
            whirlpool: WhirlpoolAdapter::new(rpc),
        })
    }
    
    /// Load an Address Lookup Table from chain
    fn load_lookup_table(rpc: &RpcClient, address_str: &str) -> Result<AddressLookupTableAccount> {
        let address = Pubkey::from_str(address_str)
            .map_err(|e| anyhow!("Invalid ALT address: {}", e))?;
        
        let account = rpc.get_account(&address)
            .map_err(|e| anyhow!("Failed to fetch ALT account: {}", e))?;
        
        let lookup_table = AddressLookupTableAccount {
            key: address,
            addresses: Self::parse_lookup_table_addresses(&account.data)?,
        };
        
        Ok(lookup_table)
    }
    
    /// Parse addresses from ALT account data
    fn parse_lookup_table_addresses(data: &[u8]) -> Result<Vec<Pubkey>> {
        // ALT account layout:
        // - 1 byte: type discriminator (1 for lookup table)
        // - 8 bytes: deactivation slot (u64)
        // - 8 bytes: last extended slot (u64)
        // - 1 byte: last extended slot start index
        // - 1 byte: has authority (bool)
        // - 32 bytes: authority (if has_authority)
        // - padding to 56 bytes total header
        // - remaining: addresses (32 bytes each)
        
        const HEADER_SIZE: usize = 56;
        
        if data.len() < HEADER_SIZE {
            return Err(anyhow!("ALT account data too small"));
        }
        
        let addresses_data = &data[HEADER_SIZE..];
        let num_addresses = addresses_data.len() / 32;
        
        let mut addresses = Vec::with_capacity(num_addresses);
        for i in 0..num_addresses {
            let start = i * 32;
            let end = start + 32;
            if end > addresses_data.len() {
                break;
            }
            let pubkey = Pubkey::try_from(&addresses_data[start..end])
                .map_err(|_| anyhow!("Invalid pubkey in ALT at index {}", i))?;
            addresses.push(pubkey);
        }
        
        Ok(addresses)
    }
    
    /// Get a random tip wallet from the configured list
    fn get_random_tip_wallet(&self) -> Pubkey {
        use std::time::{SystemTime, UNIX_EPOCH};
        
        // For Jito mode, use Jito tip account
        if self.transaction_mode == TransactionMode::Jito {
            return self.jito_tip_account.unwrap_or_else(|| {
                // Fallback to first Jito tip account
                Pubkey::from_str("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5").unwrap()
            });
        }
        
        // For Astralane mode, use Astralane tip wallets
        // Simple random selection using current time
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as usize;
        
        let index = nanos % self.tip_wallets.len();
        let selected = self.tip_wallets[index];
        
        tracing::debug!("🎲 Selected tip wallet {}/{}: {}", index + 1, self.tip_wallets.len(), selected);
        selected
    }
    
    /// Execute an arbitrage opportunity atomically
    pub async fn execute(
        &self,
        wallet: &Keypair,
        opportunity: &Opportunity,
        buy_slippage_bps: u64,
        sell_slippage_bps: u64,
    ) -> Result<ArbResult> {
        let sol_amount = (opportunity.recommended_sol_amount * 1e9) as u64;
        
        // Build buy instructions - returns expected_amount_out in RAW token units
        let buy_result = self.build_buy_instructions_with_output(
            wallet,
            &opportunity.buy_pool.address,
            opportunity.buy_pool.pool_type,
            sol_amount,
            buy_slippage_bps,
        ).await?;
        
        let buy_instructions = buy_result.0;
        let expected_tokens = buy_result.1; // Raw token amount from buy instruction
        
        tracing::info!("   📊 Expected tokens from buy: {} (raw units)", expected_tokens);
        
        // Build sell instructions using the ACTUAL expected output from buy
        let sell_instructions = self.build_sell_instructions(
            wallet,
            &opportunity.sell_pool.address,
            opportunity.sell_pool.pool_type,
            expected_tokens,
            sell_slippage_bps,
        ).await?;
        
        // Execute with retry
        let signature = self.execute_with_retry(
            wallet,
            buy_instructions,
            sell_instructions,
        ).await?;
        
        Ok(ArbResult {
            signature,
            buy_pool: opportunity.buy_pool.address,
            sell_pool: opportunity.sell_pool.address,
            sol_in: sol_amount,
            expected_tokens,
            expected_sol_out: (opportunity.profit_result.sol_received * 1e9) as u64,
        })
    }
    
    /// Build buy instructions and return expected token output (raw units)
    async fn build_buy_instructions_with_output(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        pool_type: PoolType,
        sol_amount: u64,
        slippage_bps: u64,
    ) -> Result<(Vec<Instruction>, u64)> {
        tracing::info!("🔨 Building BUY instructions:");
        tracing::info!("   - Pool: {} ({:?})", pool_address, pool_type);
        tracing::info!("   - SOL amount: {} lamports ({:.6} SOL)", sol_amount, sol_amount as f64 / 1e9);
        tracing::info!("   - Slippage: {} bps", slippage_bps);
        
        let result = match pool_type {
            PoolType::PumpSwap => {
                tracing::info!("   - Using PumpSwap adapter");
                self.pumpswap.build_buy_instructions(wallet, pool_address, sol_amount, slippage_bps).await?
            }
            PoolType::MeteoraDLMM => {
                tracing::info!("   - Using DLMM adapter");
                self.dlmm.build_buy_instructions(wallet, pool_address, sol_amount, slippage_bps).await?
            }
            PoolType::MeteoraDammV2 => {
                tracing::info!("   - Using DAMM V2 adapter");
                self.damm_v2.build_buy_instructions(wallet, pool_address, sol_amount, slippage_bps).await?
            }
            PoolType::RaydiumCPMM => {
                tracing::info!("   - Using CPMM adapter");
                self.cpmm.build_buy_instructions(wallet, pool_address, sol_amount, slippage_bps).await?
            }
            PoolType::OrcaWhirlpool => {
                tracing::info!("   - Using Whirlpool adapter");
                self.whirlpool.build_buy_instructions(wallet, pool_address, sol_amount, slippage_bps).await?
            }
            PoolType::Unsupported => {
                return Err(anyhow!("Unsupported pool type for buy"));
            }
        };
        
        tracing::info!("   ✅ Built {} buy instructions, expected out: {} (raw tokens)", 
            result.instructions.len(), result.expected_amount_out);
        
        // Log each instruction's program ID
        for (i, ix) in result.instructions.iter().enumerate() {
            tracing::debug!("   - Instruction {}: program={}, accounts={}, data_len={}", 
                i, ix.program_id, ix.accounts.len(), ix.data.len());
        }
        
        Ok((result.instructions, result.expected_amount_out))
    }
    
    /// Build buy instructions based on pool type (returns only instructions)
    #[allow(dead_code)]
    async fn build_buy_instructions(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        pool_type: PoolType,
        sol_amount: u64,
        slippage_bps: u64,
    ) -> Result<Vec<Instruction>> {
        let (instructions, _) = self.build_buy_instructions_with_output(
            wallet, pool_address, pool_type, sol_amount, slippage_bps
        ).await?;
        Ok(instructions)
    }
    
    /// Build sell instructions based on pool type
    async fn build_sell_instructions(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        pool_type: PoolType,
        token_amount: u64,
        slippage_bps: u64,
    ) -> Result<Vec<Instruction>> {
        tracing::info!("🔨 Building SELL instructions:");
        tracing::info!("   - Pool: {} ({:?})", pool_address, pool_type);
        tracing::info!("   - Token amount: {}", token_amount);
        tracing::info!("   - Slippage: {} bps", slippage_bps);
        
        let result = match pool_type {
            PoolType::PumpSwap => {
                tracing::info!("   - Using PumpSwap adapter");
                self.pumpswap.build_sell_instructions(wallet, pool_address, token_amount, slippage_bps).await?
            }
            PoolType::MeteoraDLMM => {
                tracing::info!("   - Using DLMM adapter");
                self.dlmm.build_sell_instructions(wallet, pool_address, token_amount, slippage_bps).await?
            }
            PoolType::MeteoraDammV2 => {
                tracing::info!("   - Using DAMM V2 adapter");
                self.damm_v2.build_sell_instructions(wallet, pool_address, token_amount, slippage_bps).await?
            }
            PoolType::RaydiumCPMM => {
                tracing::info!("   - Using CPMM adapter");
                self.cpmm.build_sell_instructions(wallet, pool_address, token_amount, slippage_bps).await?
            }
            PoolType::OrcaWhirlpool => {
                tracing::info!("   - Using Whirlpool adapter");
                self.whirlpool.build_sell_instructions(wallet, pool_address, token_amount, slippage_bps).await?
            }
            PoolType::Unsupported => {
                return Err(anyhow!("Unsupported pool type for sell"));
            }
        };
        
        tracing::info!("   ✅ Built {} sell instructions, expected out: {} lamports ({:.6} SOL)", 
            result.instructions.len(), result.expected_amount_out, result.expected_amount_out as f64 / 1e9);
        
        // Log each instruction's program ID
        for (i, ix) in result.instructions.iter().enumerate() {
            tracing::debug!("   - Instruction {}: program={}, accounts={}, data_len={}", 
                i, ix.program_id, ix.accounts.len(), ix.data.len());
        }
        
        Ok(result.instructions)
    }

    /// Execute with retry and escalating tips
    async fn execute_with_retry(
        &self,
        wallet: &Keypair,
        buy_instructions: Vec<Instruction>,
        sell_instructions: Vec<Instruction>,
    ) -> Result<String> {
        let mut current_tip = self.retry_config.base_tip_lamports;
        
        for attempt in 0..=self.retry_config.max_retries {
            match self.send_atomic_bundle(
                wallet,
                &buy_instructions,
                &sell_instructions,
                current_tip,
            ).await {
                Ok(sig) => {
                    tracing::info!(
                        "✅ Arb landed on attempt {} with tip {} lamports",
                        attempt,
                        current_tip
                    );
                    return Ok(sig);
                }
                Err(e) if Self::is_retryable(&e) => {
                    tracing::warn!(
                        "⚠️ Attempt {} failed, retrying with higher tip: {}",
                        attempt,
                        e
                    );
                    
                    // Escalate tip
                    current_tip = ((current_tip as f64) * self.retry_config.tip_multiplier) as u64;
                    current_tip = current_tip.min(self.retry_config.max_tip_lamports);
                    
                    tokio::time::sleep(Duration::from_millis(self.retry_config.retry_delay_ms)).await;
                }
                Err(e) => {
                    tracing::error!("❌ Non-retryable error: {}", e);
                    return Err(e);
                }
            }
        }
        
        Err(anyhow!("Max retries ({}) exceeded", self.retry_config.max_retries))
    }
    
    /// Check if error is retryable
    fn is_retryable(e: &anyhow::Error) -> bool {
        let msg = e.to_string().to_lowercase();
        msg.contains("timeout")
            || msg.contains("blockhash")
            || msg.contains("not confirmed")
            || msg.contains("connection")
    }
    
    /// Send atomic bundle via Astralane or Jito (based on transaction_mode)
    /// Uses Versioned Transactions (V0) with Address Lookup Tables for size optimization
    async fn send_atomic_bundle(
        &self,
        wallet: &Keypair,
        buy_instructions: &[Instruction],
        sell_instructions: &[Instruction],
        tip_lamports: u64,
    ) -> Result<String> {
        let wallet_pubkey = wallet.pubkey();
        
        // Combine all instructions into single transaction
        let mut all_instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            ComputeBudgetInstruction::set_compute_unit_price(self.priority_fee),
        ];
        
        // Add buy instructions (skip compute budget if already present)
        for ix in buy_instructions {
            if !Self::is_compute_budget_ix(ix) {
                all_instructions.push(ix.clone());
            }
        }
        
        // Add sell instructions (skip compute budget if already present)
        for ix in sell_instructions {
            if !Self::is_compute_budget_ix(ix) {
                all_instructions.push(ix.clone());
            }
        }
        
        // Add tip (Astralane or Jito tip account)
        // Min 100,000 lamports (0.0001 SOL) for Paladin support
        let tip_wallet = self.get_random_tip_wallet();
        all_instructions.push(system_instruction::transfer(
            &wallet_pubkey,
            &tip_wallet,
            tip_lamports.max(100_000),
        ));
        
        // Get blockhash with Processed commitment for speed
        let blockhash = self.rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
            .map_err(|e| anyhow!("Failed to get blockhash: {}", e))?
            .0;
        
        // Build versioned transaction
        let versioned_tx = self.build_versioned_transaction(
            wallet,
            &all_instructions,
            blockhash,
        )?;
        
        // Get serialized size for logging
        let serialized = bincode::serialize(&versioned_tx)
            .map_err(|e| anyhow!("Failed to serialize transaction: {}", e))?;
        
        // Debug: Log transaction details
        tracing::info!("📤 Transaction details:");
        tracing::info!("   - Serialized size: {} bytes", serialized.len());
        tracing::info!("   - Num instructions: {}", all_instructions.len());
        tracing::info!("   - Mode: {:?}", self.transaction_mode);
        
        // Check if transaction exceeds Solana's limit
        if serialized.len() > 1232 {
            tracing::error!("❌ Transaction too large: {} bytes (max 1232)", serialized.len());
            tracing::error!("   Consider using pools with fewer accounts or creating an ALT");
            return Err(anyhow!(
                "Transaction too large: {} bytes. PumpSwap + DLMM combo exceeds Solana's 1232 byte limit. \
                Try using simpler pool pairs (e.g., two Raydium pools) or create an Address Lookup Table.",
                serialized.len()
            ));
        }
        
        // Send based on transaction mode
        match self.transaction_mode {
            TransactionMode::Jito => {
                tracing::info!("📤 Sending via Jito bundle");
                if let Some(ref jito_sender) = self.jito_sender {
                    jito_sender.send_bundle(versioned_tx, tip_lamports).await
                } else {
                    Err(anyhow!("Jito sender not initialized"))
                }
            }
            TransactionMode::Astralane => {
                tracing::info!("📤 Sending via Astralane JSON-RPC endpoint");
                let encoded_tx = base64::prelude::BASE64_STANDARD.encode(&serialized);
                self.send_transaction_jsonrpc(&encoded_tx, true).await
            }
        }
    }
    
    /// Build a versioned transaction (V0 with ALT if available)
    fn build_versioned_transaction(
        &self,
        wallet: &Keypair,
        instructions: &[Instruction],
        blockhash: Hash,
    ) -> Result<VersionedTransaction> {
        let wallet_pubkey = wallet.pubkey();
        
        // Try V0 with ALT first
        if let Some(ref alt) = self.lookup_table {
            if let Ok(v0_message) = v0::Message::try_compile(
                &wallet_pubkey,
                instructions,
                &[alt.clone()],
                blockhash,
            ) {
                let versioned_message = VersionedMessage::V0(v0_message);
                if let Ok(tx) = VersionedTransaction::try_new(versioned_message, &[wallet]) {
                    let serialized = bincode::serialize(&tx).unwrap_or_default();
                    if serialized.len() <= 1232 {
                        tracing::info!("✅ Using V0 transaction with ALT ({} bytes)", serialized.len());
                        return Ok(tx);
                    }
                }
            }
        }
        
        // Try V0 without ALT
        if let Ok(v0_message) = v0::Message::try_compile(
            &wallet_pubkey,
            instructions,
            &[],
            blockhash,
        ) {
            let versioned_message = VersionedMessage::V0(v0_message);
            if let Ok(tx) = VersionedTransaction::try_new(versioned_message, &[wallet]) {
                tracing::debug!("Using V0 transaction without ALT");
                return Ok(tx);
            }
        }
        
        // Fall back to legacy (wrapped in VersionedTransaction)
        let legacy_tx = Transaction::new_signed_with_payer(
            instructions,
            Some(&wallet_pubkey),
            &[wallet],
            blockhash,
        );
        
        tracing::debug!("Using legacy transaction format");
        Ok(VersionedTransaction::from(legacy_tx))
    }
    
    /// Build an optimized transaction, trying V0 format with ALT first
    fn build_optimized_transaction(
        &self,
        wallet: &Keypair,
        instructions: &[Instruction],
        blockhash: Hash,
    ) -> Result<Vec<u8>> {
        let wallet_pubkey = wallet.pubkey();
        
        // If we have an ALT, try V0 with ALT first (most compact)
        if let Some(ref alt) = self.lookup_table {
            match self.try_build_v0_transaction(wallet, instructions, blockhash, &[alt.clone()]) {
                Ok(serialized) if serialized.len() <= 1232 => {
                    tracing::info!("✅ Using V0 transaction with ALT ({} bytes, {} addresses compressed)", 
                        serialized.len(), alt.addresses.len());
                    return Ok(serialized);
                }
                Ok(serialized) => {
                    tracing::warn!("V0+ALT transaction still too large: {} bytes", serialized.len());
                }
                Err(e) => {
                    tracing::debug!("V0+ALT transaction failed: {}", e);
                }
            }
        }
        
        // Try V0 without ALT
        match self.try_build_v0_transaction(wallet, instructions, blockhash, &[]) {
            Ok(serialized) if serialized.len() <= 1232 => {
                tracing::debug!("Using V0 transaction without ALT ({} bytes)", serialized.len());
                return Ok(serialized);
            }
            Ok(serialized) => {
                tracing::warn!("V0 transaction too large: {} bytes", serialized.len());
            }
            Err(e) => {
                tracing::debug!("V0 transaction failed: {}, trying legacy", e);
            }
        }
        
        // Fall back to legacy transaction
        let tx = Transaction::new_signed_with_payer(
            instructions,
            Some(&wallet_pubkey),
            &[wallet],
            blockhash,
        );
        
        // Serialize legacy transaction
        let message_data = tx.message_data();
        let mut serialized = Vec::with_capacity(1 + 64 * tx.signatures.len() + message_data.len());
        serialized.push(tx.signatures.len() as u8);
        for sig in &tx.signatures {
            serialized.extend_from_slice(sig.as_ref());
        }
        serialized.extend_from_slice(&message_data);
        
        tracing::debug!("Using legacy transaction format ({} bytes)", serialized.len());
        Ok(serialized)
    }
    
    /// Try to build a V0 versioned transaction
    fn try_build_v0_transaction(
        &self,
        wallet: &Keypair,
        instructions: &[Instruction],
        blockhash: Hash,
        lookup_tables: &[AddressLookupTableAccount],
    ) -> Result<Vec<u8>> {
        let wallet_pubkey = wallet.pubkey();
        
        // Build V0 message
        let v0_message = v0::Message::try_compile(
            &wallet_pubkey,
            instructions,
            lookup_tables,
            blockhash,
        ).map_err(|e| anyhow!("Failed to compile V0 message: {}", e))?;
        
        let versioned_message = VersionedMessage::V0(v0_message);
        
        // Sign the versioned transaction
        let versioned_tx = VersionedTransaction::try_new(
            versioned_message,
            &[wallet],
        ).map_err(|e| anyhow!("Failed to sign V0 transaction: {}", e))?;
        
        // Serialize using bincode (standard for versioned transactions)
        let serialized = bincode::serialize(&versioned_tx)
            .map_err(|e| anyhow!("Failed to serialize V0 transaction: {}", e))?;
        
        Ok(serialized)
    }
    
    /// Check if instruction is a compute budget instruction
    fn is_compute_budget_ix(ix: &Instruction) -> bool {
        ix.program_id == solana_sdk::compute_budget::id()
    }
    
    /// Get target token decimals from pool state
    /// Returns decimals for the non-SOL token in the pair
    fn get_target_token_decimals(&self, state: &PoolState) -> u8 {
        let sol_mint_str = "So11111111111111111111111111111111111111112";
        let sol_mint: Pubkey = sol_mint_str.parse().unwrap();
        
        // Determine which token is the target (non-SOL)
        let target_mint = if state.token_a_mint == sol_mint {
            state.token_b_mint
        } else {
            state.token_a_mint
        };
        
        // Try to fetch mint info for decimals
        match self.rpc.get_account(&target_mint) {
            Ok(account) => {
                // SPL Token mint has decimals at offset 44 (after 36 bytes of other data + 4 byte supply + 1 byte is_initialized + 1 byte freeze_authority_option + 32 byte freeze_authority = 44, then decimals at 44)
                // Actually: mint_authority (36) + supply (8) + decimals (1) = decimals at offset 44
                // Simpler: use spl_token to parse
                if account.data.len() >= 45 {
                    account.data[44]
                } else {
                    DEFAULT_TOKEN_DECIMALS
                }
            }
            Err(_) => DEFAULT_TOKEN_DECIMALS,
        }
    }
    
    /// Execute emergency sell on a specific pool
    pub async fn emergency_sell(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        pool_type: PoolType,
        token_amount: u64,
        slippage_bps: u64,
        tip_lamports: u64,
    ) -> Result<String> {
        tracing::info!(
            "🚨 Building emergency sell: {} tokens on {:?} pool {}",
            token_amount,
            pool_type,
            pool_address
        );

        // Build sell instructions
        let sell_instructions = self
            .build_sell_instructions(wallet, pool_address, pool_type, token_amount, slippage_bps)
            .await?;

        // Send as single transaction with high priority
        self.send_single_transaction(wallet, sell_instructions, tip_lamports)
            .await
    }

    /// Send single transaction (for emergency sell)
    pub async fn send_single_transaction(
        &self,
        wallet: &Keypair,
        instructions: Vec<Instruction>,
        tip_lamports: u64,
    ) -> Result<String> {
        let wallet_pubkey = wallet.pubkey();
        
        let mut all_instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
            ComputeBudgetInstruction::set_compute_unit_price(self.priority_fee),
        ];
        
        for ix in instructions {
            if !Self::is_compute_budget_ix(&ix) {
                all_instructions.push(ix);
            }
        }
        
        // Add tip (min 100,000 lamports for Paladin support)
        let tip_wallet = self.get_random_tip_wallet();
        all_instructions.push(system_instruction::transfer(
            &wallet_pubkey,
            &tip_wallet,
            tip_lamports.max(100_000),
        ));
        
        // Get blockhash with Processed commitment for speed
        let blockhash = self.rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::processed())?
            .0;
        
        // Build versioned transaction
        let versioned_tx = self.build_versioned_transaction(
            wallet,
            &all_instructions,
            blockhash,
        )?;
        
        // Send based on transaction mode
        match self.transaction_mode {
            TransactionMode::Jito => {
                tracing::info!("📤 Sending emergency sell via Jito bundle");
                if let Some(ref jito_sender) = self.jito_sender {
                    jito_sender.send_bundle(versioned_tx, tip_lamports).await
                } else {
                    Err(anyhow!("Jito sender not initialized"))
                }
            }
            TransactionMode::Astralane => {
                let serialized = bincode::serialize(&versioned_tx)
                    .map_err(|e| anyhow!("Failed to serialize transaction: {}", e))?;
                let encoded_tx = base64::prelude::BASE64_STANDARD.encode(&serialized);
                self.send_transaction_jsonrpc(&encoded_tx, true).await
            }
        }
    }
    
    /// Send transaction via Astralane V2 endpoint (lower latency)
    /// Uses /iris2 with plain text body instead of JSON
    /// - No CORS preflight checks
    /// - Reduced body size = reduced bandwidth and network overhead
    #[allow(dead_code)]
    async fn send_transaction_v2(&self, encoded_tx: &str, mev_protect: bool) -> Result<String> {
        // Build V2 endpoint URL: /iris2?api-key=xxx&method=sendTransaction&mev-protect=true
        let v2_endpoint = self.get_v2_endpoint_url(mev_protect);
        
        // Log the endpoint (mask API key)
        let masked_endpoint = v2_endpoint.replace(&self.api_key, "***");
        tracing::info!("📤 Sending to V2: {}", masked_endpoint);
        
        let response = self.http_client
            .post(&v2_endpoint)
            .header("Content-Type", "text/plain")
            .body(encoded_tx.to_string())
            .send()
            .await
            .map_err(|e| anyhow!("Failed to send transaction V2: {}", e))?;
        
        let status = response.status();
        let body = response.text().await
            .map_err(|e| anyhow!("Failed to read V2 response: {}", e))?;
        
        tracing::info!("📥 V2 Response: status={}, body={}", status, body);
        
        // V2 returns signature directly or as JSON
        // Try to parse as JSON first
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(error) = json.get("error") {
                tracing::error!("❌ Astralane V2 error: {:?}", error);
                return Err(anyhow!("Astralane V2 error: {:?}", error));
            }
            if let Some(result) = json.get("result") {
                if let Some(sig) = result.as_str() {
                    tracing::info!("✅ Got signature: {}", sig);
                    return Ok(sig.to_string());
                }
            }
        }
        
        // If not JSON, body might be the signature directly (for success)
        if status.is_success() {
            let trimmed = body.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('{') {
                tracing::info!("✅ Got signature (plain): {}", trimmed);
                return Ok(trimmed.to_string());
            }
        }
        
        Err(anyhow!("Astralane V2 error ({}): {}", status, body))
    }
    
    /// Get V2 endpoint URL (/iris2 with query params)
    fn get_v2_endpoint_url(&self, mev_protect: bool) -> String {
        // Convert /iris to /iris2
        let base = if self.astralane_endpoint.contains("/iris?") {
            self.astralane_endpoint.replace("/iris?", "/iris2?")
        } else if self.astralane_endpoint.ends_with("/iris") {
            format!("{}2", self.astralane_endpoint)
        } else {
            // Assume it's a base URL, append /iris2
            format!("{}/iris2", self.astralane_endpoint.trim_end_matches('/'))
        };
        
        // Add required query params
        if base.contains('?') {
            format!("{}&api-key={}&method=sendTransaction&mev-protect={}", 
                base, self.api_key, mev_protect)
        } else {
            format!("{}?api-key={}&method=sendTransaction&mev-protect={}", 
                base, self.api_key, mev_protect)
        }
    }
    
    /// Send transaction via JSON-RPC endpoint (matching Astralane docs)
    /// Uses api-key as query parameter (recommended by Astralane docs)
    async fn send_transaction_jsonrpc(&self, encoded_tx: &str, mev_protect: bool) -> Result<String> {
        // Build endpoint URL with api-key as query parameter
        // Format: https://ams.gateway.astralane.io/iris?api-key=xxxx
        let endpoint_url = if self.astralane_endpoint.contains("?api-key=") {
            // Already has api-key in URL
            self.astralane_endpoint.clone()
        } else if self.astralane_endpoint.contains('?') {
            format!("{}&api-key={}", self.astralane_endpoint, self.api_key)
        } else {
            format!("{}?api-key={}", self.astralane_endpoint, self.api_key)
        };
        
        // Log the endpoint (mask API key)
        let masked_url = endpoint_url.replace(&self.api_key, "***");
        tracing::info!("📤 Sending to: {}", masked_url);
        
        // Try standard Solana RPC format (Astralane claims compatibility)
        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [
                encoded_tx,
                {
                    "encoding": "base64",
                    "skipPreflight": true,
                    "preflightCommitment": "processed"
                }
            ]
        });
        
        tracing::info!("📤 Request: {}", serde_json::to_string_pretty(&request_body).unwrap_or_default());
        
        tracing::debug!("📤 Request body: {}", serde_json::to_string(&request_body).unwrap_or_default());
        
        let response = self.http_client
            .post(&endpoint_url)
            .header("Content-Type", "application/json")
            .header("api_key", &self.api_key)  // Also send as header for redundancy
            .json(&request_body)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to send transaction: {}", e))?;
        
        let status = response.status();
        let body: serde_json::Value = response.json().await
            .map_err(|e| anyhow!("Failed to parse response: {}", e))?;
        
        tracing::info!("📥 Response: status={}, body={}", status, body);
        
        // Check for error in response
        if let Some(error) = body.get("error") {
            tracing::error!("❌ Astralane error: {:?}", error);
            return Err(anyhow!("Astralane error: {:?}", error));
        }
        
        // Extract signature from result
        if let Some(result) = body.get("result") {
            if let Some(sig) = result.as_str() {
                tracing::info!("✅ Got signature: {}", sig);
                return Ok(sig.to_string());
            }
        }
        
        Err(anyhow!("Unexpected response: {:?}", body))
    }
    
    /// Wait for transaction confirmation with processed commitment (fastest)
    /// Returns true if confirmed, false if not confirmed within timeout
    pub async fn wait_for_confirmation(&self, signature: &str, timeout_ms: u64) -> bool {
        let sig = match Signature::from_str(signature) {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!("Invalid signature format: {}", signature);
                return false;
            }
        };
        
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);
        let check_interval = Duration::from_millis(200); // Check every 200ms
        
        while start.elapsed() < timeout {
            match self.rpc.get_signature_status_with_commitment(
                &sig,
                CommitmentConfig::processed(),
            ) {
                Ok(Some(status)) => {
                    match status {
                        Ok(_) => {
                            tracing::info!("✅ Transaction confirmed (processed): {}", signature);
                            return true;
                        }
                        Err(e) => {
                            tracing::warn!("❌ Transaction failed: {} - {}", signature, e);
                            return false; // Transaction failed, no need to wait
                        }
                    }
                }
                Ok(None) => {
                    // Not yet processed, keep waiting
                    tracing::trace!("⏳ Waiting for confirmation: {}", signature);
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
    
    /// Get RPC client reference (for external confirmation checks)
    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }
}
