//! Emergency Sell Handler
//! Handles emergency sell when price drops or max loss is reached

use anyhow::{anyhow, Result};
use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer};

use crate::arbitrage::ArbExecutor;
use crate::pools::{PoolState, PoolType};

/// Reason for emergency sell
#[derive(Debug, Clone)]
pub enum EmergencyReason {
    PriceDrop {
        pool: Pubkey,
        drop_percent: f64,
    },
    MaxLossReached {
        current_loss_sol: f64,
    },
    ManualTrigger,
}

/// Emergency sell handler
pub struct EmergencySellHandler {
    emergency_slippage_bps: u64,
}

impl EmergencySellHandler {
    pub fn new(emergency_slippage_bps: u64) -> Self {
        Self {
            emergency_slippage_bps,
        }
    }
    
    /// Select the pool with highest liquidity for emergency sell
    pub fn select_best_pool<'a>(
        &self,
        pools: &'a [PoolState],
        exclude_pool: Option<&Pubkey>,
    ) -> Option<&'a PoolState> {
        pools
            .iter()
            .filter(|p| exclude_pool.map_or(true, |ex| &p.address != ex))
            .filter(|p| p.pool_type != PoolType::Unsupported)
            .max_by(|a, b| {
                a.liquidity_sol
                    .partial_cmp(&b.liquidity_sol)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }
    
    /// Execute emergency sell using the executor
    pub async fn execute(
        &self,
        executor: &ArbExecutor,
        wallet: &Keypair,
        pools: &[PoolState],
        token_balance: u64,
        reason: EmergencyReason,
        target_token: &Pubkey,
    ) -> Result<String> {
        let exclude_pool = match &reason {
            EmergencyReason::PriceDrop { pool, .. } => Some(pool),
            _ => None,
        };
        
        let best_pool = self
            .select_best_pool(pools, exclude_pool)
            .ok_or_else(|| anyhow!("No valid pool for emergency sell"))?;
        
        tracing::warn!("🚨 EMERGENCY SELL triggered: {:?}", reason);
        tracing::warn!(
            "🚨 Selling {} tokens on pool {} ({:?})",
            token_balance,
            best_pool.address,
            best_pool.pool_type
        );
        
        // Use high tip for emergency (want fast inclusion)
        let emergency_tip = 50_000u64; // 0.00005 SOL
        
        // Execute emergency sell via executor
        let signature = executor
            .emergency_sell(
                wallet,
                &best_pool.address,
                best_pool.pool_type,
                token_balance,
                self.emergency_slippage_bps,
                emergency_tip,
            )
            .await?;
        
        tracing::info!("🚨 Emergency sell transaction sent: {}", signature);
        
        Ok(signature)
    }
    
    /// Check if any pool has dropped significantly
    pub fn check_price_drop(
        &self,
        current_prices: &[(Pubkey, f64)],
        initial_prices: &[(Pubkey, f64)],
        drop_threshold_percent: f64,
    ) -> Option<(Pubkey, f64)> {
        for (pool, current_price) in current_prices {
            if let Some((_, initial_price)) = initial_prices.iter().find(|(p, _)| p == pool) {
                if *initial_price > 0.0 {
                    let drop_percent = ((initial_price - current_price) / initial_price) * 100.0;
                    if drop_percent >= drop_threshold_percent {
                        return Some((*pool, drop_percent));
                    }
                }
            }
        }
        None
    }
}
