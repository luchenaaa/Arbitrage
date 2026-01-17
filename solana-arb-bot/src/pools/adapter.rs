use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{instruction::Instruction, pubkey::Pubkey, signature::Keypair};
use std::sync::Arc;

use super::types::PoolType;

/// Common pool state information
#[derive(Debug, Clone)]
pub struct PoolState {
    pub pool_type: PoolType,
    pub address: Pubkey,
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
    pub token_a_reserve: u64,
    pub token_b_reserve: u64,
    pub token_a_program: Pubkey,
    pub token_b_program: Pubkey,
    pub price: f64,              // token_b per token_a
    pub liquidity_sol: f64,     // Estimated SOL liquidity
}

/// Result of building swap instructions
#[derive(Debug)]
pub struct SwapInstructions {
    pub instructions: Vec<Instruction>,
    pub expected_amount_out: u64,
}

/// Trait for pool adapters - each DEX implements this
#[async_trait::async_trait]
pub trait PoolAdapter: Send + Sync {
    /// Get the pool type
    fn pool_type(&self) -> PoolType;
    
    /// Fetch and parse pool state
    async fn get_pool_state(&self, pool_address: &Pubkey) -> Result<PoolState>;
    
    /// Parse pool state from raw account data (for gRPC updates)
    /// Returns None if parsing fails or data is invalid
    fn parse_pool_state_from_data(&self, pool_address: &Pubkey, data: &[u8]) -> Option<PoolState>;
    
    /// Calculate price (SOL per token for the target token)
    fn calculate_price(&self, state: &PoolState, target_is_token_a: bool) -> f64;
    
    /// Calculate expected output amount
    fn calculate_amount_out(
        &self,
        state: &PoolState,
        amount_in: u64,
        is_buy: bool,  // true = SOL -> Token, false = Token -> SOL
    ) -> Result<u64>;
    
    /// Build buy instructions (SOL -> Token)
    async fn build_buy_instructions(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        sol_amount: u64,
        slippage_bps: u64,
    ) -> Result<SwapInstructions>;
    
    /// Build sell instructions (Token -> SOL)
    async fn build_sell_instructions(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        token_amount: u64,
        slippage_bps: u64,
    ) -> Result<SwapInstructions>;
}

/// Adapter manager - holds all pool adapters
pub struct AdapterManager {
    pub pumpswap: Arc<dyn PoolAdapter>,
    pub dlmm: Arc<dyn PoolAdapter>,
    pub damm_v2: Arc<dyn PoolAdapter>,
    pub cpmm: Arc<dyn PoolAdapter>,
    pub whirlpool: Arc<dyn PoolAdapter>,
}

impl AdapterManager {
    /// Get adapter for a specific pool type
    pub fn get_adapter(&self, pool_type: PoolType) -> Option<Arc<dyn PoolAdapter>> {
        match pool_type {
            PoolType::PumpSwap => Some(self.pumpswap.clone()),
            PoolType::MeteoraDLMM => Some(self.dlmm.clone()),
            PoolType::MeteoraDammV2 => Some(self.damm_v2.clone()),
            PoolType::RaydiumCPMM => Some(self.cpmm.clone()),
            PoolType::OrcaWhirlpool => Some(self.whirlpool.clone()),
            PoolType::Unsupported => None,
        }
    }
}
