//! Arbitrage Opportunity Detector
//! Finds the best arbitrage opportunity across all pool pairs

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;

use crate::config::Settings;
use crate::pools::{PoolAdapter, PoolState, PoolType};

use super::calculator::{ProfitCalculator, ProfitResult};

/// Represents an arbitrage opportunity
#[derive(Debug, Clone)]
pub struct Opportunity {
    pub buy_pool: PoolInfo,
    pub sell_pool: PoolInfo,
    pub buy_price: f64,
    pub sell_price: f64,
    pub spread_percent: f64,
    pub recommended_sol_amount: f64,
    pub profit_result: ProfitResult,
}

/// Pool information for an opportunity
#[derive(Debug, Clone)]
pub struct PoolInfo {
    pub address: Pubkey,
    pub pool_type: PoolType,
    pub liquidity_sol: f64,
    pub state: PoolState,
}

/// Estimated account counts per pool type (for transaction size estimation)
/// These are approximate and include all accounts needed for a swap
const PUMPSWAP_ACCOUNTS: usize = 23;  // PumpSwap has many fee/creator accounts
const DLMM_ACCOUNTS: usize = 19;      // DLMM base + 3 bin arrays
const DAMM_V2_ACCOUNTS: usize = 12;
const CPMM_ACCOUNTS: usize = 14;
const WHIRLPOOL_ACCOUNTS: usize = 11;

/// Maximum transaction size in bytes (Solana limit)
const MAX_TX_SIZE: usize = 1232;

/// Arbitrage detector - finds opportunities across pools
pub struct ArbitrageDetector {
    calculator: ProfitCalculator,
    max_buy_sol: f64,
    max_pool_liquidity_percent: f64,
    buy_slippage: f64,
    sell_slippage: f64,
}

impl ArbitrageDetector {
    pub fn new(settings: &Settings) -> Self {
        Self {
            calculator: ProfitCalculator::new(settings),
            max_buy_sol: settings.limits.max_buy_amount_sol,
            max_pool_liquidity_percent: settings.sizing.max_pool_liquidity_percent,
            buy_slippage: settings.slippage.buy_slippage_bps as f64 / 10000.0,
            sell_slippage: settings.slippage.sell_slippage_bps as f64 / 10000.0,
        }
    }
    
    /// Find the best arbitrage opportunity from pool states
    pub fn find_best_opportunity(
        &self,
        pool_states: &[PoolState],
        target_token: &Pubkey,
    ) -> Option<Opportunity> {
        if pool_states.len() < 2 {
            return None;
        }
        
        let mut best_opportunity: Option<Opportunity> = None;
        let mut best_profit = 0.0f64;
        
        // Generate all possible buy/sell pairs
        for (i, buy_state) in pool_states.iter().enumerate() {
            for (j, sell_state) in pool_states.iter().enumerate() {
                if i == j {
                    continue; // Can't arb same pool
                }
                
                // Determine if target token is token_a or token_b
                let buy_target_is_a = buy_state.token_a_mint == *target_token;
                let sell_target_is_a = sell_state.token_a_mint == *target_token;
                
                // Skip if target token not in both pools
                if !buy_target_is_a && buy_state.token_b_mint != *target_token {
                    continue;
                }
                if !sell_target_is_a && sell_state.token_b_mint != *target_token {
                    continue;
                }
                
                // Get prices (SOL per token)
                let buy_price = self.get_price_sol_per_token(buy_state, buy_target_is_a);
                let sell_price = self.get_price_sol_per_token(sell_state, sell_target_is_a);
                
                // Quick check if spread is worth it
                if !self.calculator.is_spread_profitable(buy_price, sell_price) {
                    continue;
                }
                
                // Calculate optimal buy amount
                let sol_amount = self.calculate_optimal_amount(buy_state, sell_state);
                
                // Calculate profit
                let profit_result = self.calculator.calculate(
                    buy_price,
                    sell_price,
                    sol_amount,
                    self.buy_slippage,
                    self.sell_slippage,
                );
                
                if profit_result.is_profitable && profit_result.net_profit_sol > best_profit {
                    best_profit = profit_result.net_profit_sol;
                    
                    let spread_percent = if buy_price > 0.0 {
                        ((sell_price - buy_price) / buy_price) * 100.0
                    } else {
                        0.0
                    };
                    
                    best_opportunity = Some(Opportunity {
                        buy_pool: PoolInfo {
                            address: buy_state.address,
                            pool_type: buy_state.pool_type,
                            liquidity_sol: buy_state.liquidity_sol,
                            state: buy_state.clone(),
                        },
                        sell_pool: PoolInfo {
                            address: sell_state.address,
                            pool_type: sell_state.pool_type,
                            liquidity_sol: sell_state.liquidity_sol,
                            state: sell_state.clone(),
                        },
                        buy_price,
                        sell_price,
                        spread_percent,
                        recommended_sol_amount: sol_amount,
                        profit_result,
                    });
                }
            }
        }
        
        best_opportunity
    }
    
    /// Get price in SOL per token
    fn get_price_sol_per_token(&self, state: &PoolState, target_is_token_a: bool) -> f64 {
        let sol_mint_str = "So11111111111111111111111111111111111111112";
        let sol_mint: Pubkey = sol_mint_str.parse().unwrap();
        
        if target_is_token_a {
            // Target is token_a, SOL should be token_b
            if state.token_b_mint == sol_mint && state.token_a_reserve > 0 {
                state.token_b_reserve as f64 / state.token_a_reserve as f64
            } else {
                state.price
            }
        } else {
            // Target is token_b, SOL should be token_a
            if state.token_a_mint == sol_mint && state.token_b_reserve > 0 {
                state.token_a_reserve as f64 / state.token_b_reserve as f64
            } else if state.price > 0.0 {
                1.0 / state.price
            } else {
                0.0
            }
        }
    }
    
    /// Calculate optimal buy amount based on liquidity
    fn calculate_optimal_amount(&self, buy_state: &PoolState, sell_state: &PoolState) -> f64 {
        // Get minimum liquidity between pools
        let min_liquidity = buy_state.liquidity_sol.min(sell_state.liquidity_sol);
        
        // Don't take more than X% of pool liquidity
        let liquidity_limit = min_liquidity * (self.max_pool_liquidity_percent / 100.0);
        
        // Calculate spread-based sizing
        // Higher spread = more confident = larger size
        let buy_price = buy_state.price;
        let sell_price = sell_state.price;
        
        let spread = if buy_price > 0.0 {
            (sell_price - buy_price) / buy_price
        } else {
            0.0
        };
        
        // spread 0.5% → 25% of max, spread 2% → 100% of max
        let spread_multiplier = (spread / 0.02).min(1.0).max(0.25);
        let calculated = self.max_buy_sol * spread_multiplier;
        
        // Return minimum of all limits
        calculated.min(liquidity_limit).min(self.max_buy_sol)
    }
    
    /// Estimate if a pool pair will fit in a single transaction
    /// Returns (fits, estimated_bytes)
    pub fn estimate_transaction_size(buy_type: PoolType, sell_type: PoolType) -> (bool, usize) {
        let buy_accounts = Self::get_account_count(buy_type);
        let sell_accounts = Self::get_account_count(sell_type);
        
        // Estimate transaction size:
        // - 1 signature = 64 bytes
        // - Message header = ~3 bytes
        // - Blockhash = 32 bytes
        // - Each unique account = 32 bytes
        // - Each instruction = ~50-100 bytes (program_id index, account indices, data)
        // - Compute budget instructions = ~20 bytes each
        // - Tip instruction = ~50 bytes
        
        // Calculate overlap based on pool types
        // Same pool type = higher overlap (same program, similar accounts)
        let overlap_factor = if buy_type == sell_type {
            0.5 // 50% of sell accounts are duplicates
        } else {
            0.25 // 25% overlap for different pool types (just common accounts like wallet, mints)
        };
        
        // Unique accounts = buy + (sell * (1 - overlap)) + 5 common (wallet, mints, system, tip)
        let unique_accounts = buy_accounts + ((sell_accounts as f64) * (1.0 - overlap_factor)) as usize + 5;
        let estimated_bytes = 64 + 3 + 32 + (unique_accounts * 32) + 200; // 200 for instruction data
        
        (estimated_bytes <= MAX_TX_SIZE, estimated_bytes)
    }
    
    /// Get estimated account count for a pool type
    fn get_account_count(pool_type: PoolType) -> usize {
        match pool_type {
            PoolType::PumpSwap => PUMPSWAP_ACCOUNTS,
            PoolType::MeteoraDLMM => DLMM_ACCOUNTS,
            PoolType::MeteoraDammV2 => DAMM_V2_ACCOUNTS,
            PoolType::RaydiumCPMM => CPMM_ACCOUNTS,
            PoolType::OrcaWhirlpool => WHIRLPOOL_ACCOUNTS,
            PoolType::Unsupported => 10,
        }
    }
    
    /// Check if a pool pair combination is likely to exceed transaction size limits
    pub fn is_pool_pair_too_large(buy_type: PoolType, sell_type: PoolType) -> bool {
        let (fits, estimated) = Self::estimate_transaction_size(buy_type, sell_type);
        if !fits {
            tracing::warn!(
                "⚠️ Pool pair {:?} + {:?} may exceed tx size limit (~{} bytes > 1232)",
                buy_type, sell_type, estimated
            );
        }
        !fits
    }
}
