use anyhow::{anyhow, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::types::*;

/// Pool type detector - validates pools at startup
pub struct PoolDetector;

impl PoolDetector {
    pub fn new() -> Self {
        Self
    }

    /// Detect pool type by checking account owner
    pub fn detect_pool_type(&self, owner: &Pubkey) -> PoolType {
        if owner == &*PUMPSWAP_PROGRAM_ID {
            PoolType::PumpSwap
        } else if owner == &*METEORA_DLMM_PROGRAM_ID {
            PoolType::MeteoraDLMM
        } else if owner == &*METEORA_DAMM_V2_PROGRAM_ID {
            PoolType::MeteoraDammV2
        } else if owner == &*RAYDIUM_CPMM_PROGRAM_ID {
            PoolType::RaydiumCPMM
        } else if owner == &*ORCA_WHIRLPOOL_PROGRAM_ID {
            PoolType::OrcaWhirlpool
        } else {
            PoolType::Unsupported
        }
    }

    /// Extract token mints from pool account data based on pool type
    /// Uses the same parsing logic as the adapters for consistency
    fn extract_token_mints(
        &self,
        pool_type: PoolType,
        data: &[u8],
    ) -> Result<(Pubkey, Pubkey)> {
        match pool_type {
            PoolType::PumpSwap => {
                // PumpSwap pool layout (skip 8-byte discriminator):
                // pool_bump: u8           - offset 8 (0 after discriminator)
                // index: u16              - offset 9 (1)
                // creator: Pubkey         - offset 11 (3)
                // base_mint: Pubkey       - offset 43 (35)
                // quote_mint: Pubkey      - offset 75 (67)
                if data.len() < 8 + 99 {
                    return Err(anyhow!("PumpSwap pool data too small: {} bytes", data.len()));
                }
                // Skip 8-byte discriminator
                let data = &data[8..];
                let base_mint = Pubkey::try_from(&data[35..67])
                    .map_err(|_| anyhow!("Invalid base_mint pubkey"))?;
                let quote_mint = Pubkey::try_from(&data[67..99])
                    .map_err(|_| anyhow!("Invalid quote_mint pubkey"))?;
                Ok((base_mint, quote_mint))
            }
            PoolType::MeteoraDLMM => {
                // LbPairState: Skip 8-byte discriminator, then use bytemuck
                if data.len() < 8 + std::mem::size_of::<crate::pools::dlmm::LbPairState>() {
                    return Err(anyhow!("DLMM pool data too small: {} bytes", data.len()));
                }
                let state: &crate::pools::dlmm::LbPairState = bytemuck::from_bytes(
                    &data[8..8 + std::mem::size_of::<crate::pools::dlmm::LbPairState>()],
                );
                Ok((state.token_x_mint, state.token_y_mint))
            }
            PoolType::MeteoraDammV2 => {
                // DammV2PoolState: Skip 8-byte discriminator
                if data.len() < 8 + std::mem::size_of::<crate::pools::damm_v2::DammV2PoolState>() {
                    return Err(anyhow!("DAMM V2 pool data too small: {} bytes", data.len()));
                }
                let state: &crate::pools::damm_v2::DammV2PoolState = bytemuck::from_bytes(
                    &data[8..8 + std::mem::size_of::<crate::pools::damm_v2::DammV2PoolState>()],
                );
                Ok((state.token_a_mint, state.token_b_mint))
            }
            PoolType::RaydiumCPMM => {
                // CpmmPoolState: Skip 8-byte discriminator
                if data.len() < 8 + std::mem::size_of::<crate::pools::cpmm::CpmmPoolState>() {
                    return Err(anyhow!("CPMM pool data too small: {} bytes", data.len()));
                }
                let state: &crate::pools::cpmm::CpmmPoolState = bytemuck::from_bytes(
                    &data[8..8 + std::mem::size_of::<crate::pools::cpmm::CpmmPoolState>()],
                );
                Ok((state.token_0_mint, state.token_1_mint))
            }
            PoolType::OrcaWhirlpool => {
                // Whirlpool: 653 bytes total
                // token_mint_a at offset 8 + 32 + 1 + 2 + 2 + 2 + 2 + 16 + 16 + 4 + 8 + 8 = 8 + 93 = 101
                // token_mint_b at offset 101 + 32 + 32 + 16 = 181
                if data.len() < 653 {
                    return Err(anyhow!("Whirlpool data too small: {} bytes", data.len()));
                }
                // Offsets based on WhirlpoolState structure:
                // discriminator: 8, whirlpools_config: 32, whirlpool_bump: 1, tick_spacing: 2,
                // fee_tier_index_seed: 2, fee_rate: 2, protocol_fee_rate: 2, liquidity: 16,
                // sqrt_price: 16, tick_current_index: 4, protocol_fee_owed_a: 8, protocol_fee_owed_b: 8
                // = 8 + 32 + 1 + 2 + 2 + 2 + 2 + 16 + 16 + 4 + 8 + 8 = 101
                let token_mint_a_offset = 101;
                let token_vault_a_offset = token_mint_a_offset + 32; // 133
                let fee_growth_global_a_offset = token_vault_a_offset + 32; // 165
                let token_mint_b_offset = fee_growth_global_a_offset + 16; // 181
                
                let token_mint_a = Pubkey::try_from(&data[token_mint_a_offset..token_mint_a_offset + 32])
                    .map_err(|_| anyhow!("Invalid token_mint_a pubkey"))?;
                let token_mint_b = Pubkey::try_from(&data[token_mint_b_offset..token_mint_b_offset + 32])
                    .map_err(|_| anyhow!("Invalid token_mint_b pubkey"))?;
                Ok((token_mint_a, token_mint_b))
            }
            PoolType::Unsupported => Err(anyhow!("Cannot extract mints from unsupported pool type")),
        }
    }

    /// Validate a single pool address
    pub async fn validate_pool(&self, rpc: &RpcClient, address: &str) -> Result<ValidatedPool> {
        let pubkey = Pubkey::from_str(address)
            .map_err(|_| anyhow!("Invalid pool address format: {}", address))?;

        // Fetch account to check owner
        let account = rpc
            .get_account(&pubkey)
            .map_err(|e| anyhow!("Failed to fetch pool account {}: {}", address, e))?;

        let pool_type = self.detect_pool_type(&account.owner);

        if !pool_type.is_supported() {
            return Err(anyhow!(
                "Unsupported pool type for {}: owner is {}",
                address,
                account.owner
            ));
        }

        // Extract token mints from pool data
        let (token_a_mint, token_b_mint) = self.extract_token_mints(pool_type, &account.data)?;

        tracing::info!(
            "✅ Pool {} validated as {:?}",
            address,
            pool_type
        );
        tracing::info!(
            "   Token A: {}, Token B: {}",
            token_a_mint,
            token_b_mint
        );

        Ok(ValidatedPool {
            address: pubkey,
            pool_type,
            token_a_mint: Some(token_a_mint),
            token_b_mint: Some(token_b_mint),
        })
    }

    /// Validate multiple pool addresses
    pub async fn validate_pools(
        &self,
        rpc: &RpcClient,
        addresses: &[String],
    ) -> Result<Vec<ValidatedPool>> {
        let mut validated = Vec::new();

        for address in addresses {
            match self.validate_pool(rpc, address).await {
                Ok(pool) => {
                    validated.push(pool);
                }
                Err(e) => {
                    tracing::warn!("⚠️ Pool validation failed for {}: {}", address, e);
                }
            }
        }

        if validated.len() < 2 {
            return Err(anyhow!(
                "At least 2 valid pools required for arbitrage, found {}",
                validated.len()
            ));
        }

        Ok(validated)
    }

    /// Validate that all pools share a common token (the target token)
    pub fn validate_common_token(
        &self,
        pools: &[ValidatedPool],
        target_token: &Pubkey,
    ) -> Result<()> {
        for pool in pools {
            let has_target = pool
                .token_a_mint
                .map(|m| m == *target_token)
                .unwrap_or(false)
                || pool
                    .token_b_mint
                    .map(|m| m == *target_token)
                    .unwrap_or(false);

            if !has_target {
                return Err(anyhow!(
                    "Pool {} does not contain target token {}",
                    pool.address,
                    target_token
                ));
            }
        }

        tracing::info!(
            "✅ All {} pools contain target token {}",
            pools.len(),
            target_token
        );
        Ok(())
    }
}

impl Default for PoolDetector {
    fn default() -> Self {
        Self::new()
    }
}
