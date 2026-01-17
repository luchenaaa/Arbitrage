//! Orca Whirlpool Pool Adapter
//! Based on: Orca/rust-volume-bot/src/services/orca_whirlpool_swap.rs
//! Uses the official Orca Whirlpools SDK for swap instructions

use anyhow::{anyhow, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
};
use std::sync::Arc;

use super::adapter::{PoolAdapter, PoolState, SwapInstructions};
use super::types::*;
use crate::token::get_or_detect_token_program;

/// Orca Whirlpool Program ID
pub const WHIRLPOOL_PROGRAM: Pubkey = solana_sdk::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// Whirlpool account discriminator
pub const WHIRLPOOL_DISCRIMINATOR: [u8; 8] = [63, 149, 209, 12, 225, 128, 99, 9];

/// Whirlpool pool state structure
/// Based on orca_whirlpools_client::Whirlpool
/// Size: 653 bytes total (including 8-byte discriminator)
#[derive(Clone, Debug)]
pub struct WhirlpoolState {
    pub whirlpools_config: Pubkey,
    pub whirlpool_bump: u8,
    pub tick_spacing: u16,
    pub fee_tier_index_seed: [u8; 2],
    pub fee_rate: u16,
    pub protocol_fee_rate: u16,
    pub liquidity: u128,
    pub sqrt_price: u128,
    pub tick_current_index: i32,
    pub protocol_fee_owed_a: u64,
    pub protocol_fee_owed_b: u64,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub fee_growth_global_a: u128,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
    pub fee_growth_global_b: u128,
    pub reward_last_updated_timestamp: u64,
    // reward_infos: [WhirlpoolRewardInfo; 3] - skipped for simplicity
}

pub struct WhirlpoolAdapter {
    rpc: Arc<RpcClient>,
}

impl WhirlpoolAdapter {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self { rpc }
    }

    /// Parse whirlpool state from account data
    /// Uses borsh deserialization similar to the Orca SDK
    fn parse_pool_state(&self, data: &[u8]) -> Result<WhirlpoolState> {
        // Whirlpool account is 653 bytes
        if data.len() < 653 {
            return Err(anyhow!("Whirlpool data too small: {} bytes", data.len()));
        }
        
        // Verify discriminator
        let discriminator = &data[0..8];
        if discriminator != WHIRLPOOL_DISCRIMINATOR {
            return Err(anyhow!("Invalid Whirlpool discriminator"));
        }
        
        let mut offset = 8; // Skip discriminator
        
        // whirlpools_config: Pubkey (32 bytes)
        let whirlpools_config = Pubkey::try_from(&data[offset..offset + 32])
            .map_err(|_| anyhow!("Invalid whirlpools_config"))?;
        offset += 32;
        
        // whirlpool_bump: [u8; 1]
        let whirlpool_bump = data[offset];
        offset += 1;
        
        // tick_spacing: u16
        let tick_spacing = u16::from_le_bytes([data[offset], data[offset + 1]]);
        offset += 2;
        
        // fee_tier_index_seed: [u8; 2]
        let fee_tier_index_seed = [data[offset], data[offset + 1]];
        offset += 2;
        
        // fee_rate: u16
        let fee_rate = u16::from_le_bytes([data[offset], data[offset + 1]]);
        offset += 2;
        
        // protocol_fee_rate: u16
        let protocol_fee_rate = u16::from_le_bytes([data[offset], data[offset + 1]]);
        offset += 2;
        
        // liquidity: u128
        let liquidity = u128::from_le_bytes(data[offset..offset + 16].try_into().unwrap());
        offset += 16;
        
        // sqrt_price: u128
        let sqrt_price = u128::from_le_bytes(data[offset..offset + 16].try_into().unwrap());
        offset += 16;
        
        // tick_current_index: i32
        let tick_current_index = i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        offset += 4;
        
        // protocol_fee_owed_a: u64
        let protocol_fee_owed_a = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
        offset += 8;
        
        // protocol_fee_owed_b: u64
        let protocol_fee_owed_b = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
        offset += 8;
        
        // token_mint_a: Pubkey
        let token_mint_a = Pubkey::try_from(&data[offset..offset + 32])
            .map_err(|_| anyhow!("Invalid token_mint_a"))?;
        offset += 32;
        
        // token_vault_a: Pubkey
        let token_vault_a = Pubkey::try_from(&data[offset..offset + 32])
            .map_err(|_| anyhow!("Invalid token_vault_a"))?;
        offset += 32;
        
        // fee_growth_global_a: u128
        let fee_growth_global_a = u128::from_le_bytes(data[offset..offset + 16].try_into().unwrap());
        offset += 16;
        
        // token_mint_b: Pubkey
        let token_mint_b = Pubkey::try_from(&data[offset..offset + 32])
            .map_err(|_| anyhow!("Invalid token_mint_b"))?;
        offset += 32;
        
        // token_vault_b: Pubkey
        let token_vault_b = Pubkey::try_from(&data[offset..offset + 32])
            .map_err(|_| anyhow!("Invalid token_vault_b"))?;
        offset += 32;
        
        // fee_growth_global_b: u128
        let fee_growth_global_b = u128::from_le_bytes(data[offset..offset + 16].try_into().unwrap());
        offset += 16;
        
        // reward_last_updated_timestamp: u64
        let reward_last_updated_timestamp = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
        
        Ok(WhirlpoolState {
            whirlpools_config,
            whirlpool_bump,
            tick_spacing,
            fee_tier_index_seed,
            fee_rate,
            protocol_fee_rate,
            liquidity,
            sqrt_price,
            tick_current_index,
            protocol_fee_owed_a,
            protocol_fee_owed_b,
            token_mint_a,
            token_vault_a,
            fee_growth_global_a,
            token_mint_b,
            token_vault_b,
            fee_growth_global_b,
            reward_last_updated_timestamp,
        })
    }
    
    /// Get vault balances
    async fn get_vault_balances(&self, wp_state: &WhirlpoolState) -> Result<(u64, u64)> {
        let accounts = self.rpc.get_multiple_accounts(&[
            wp_state.token_vault_a,
            wp_state.token_vault_b,
        ])?;
        
        let vault_a_balance = Self::parse_token_balance(&accounts[0])?;
        let vault_b_balance = Self::parse_token_balance(&accounts[1])?;
        
        Ok((vault_a_balance, vault_b_balance))
    }
    
    fn parse_token_balance(account: &Option<solana_sdk::account::Account>) -> Result<u64> {
        let account = account.as_ref().ok_or_else(|| anyhow!("Token account not found"))?;
        if account.data.len() < 72 {
            return Err(anyhow!("Invalid token account data"));
        }
        // Token account balance is at offset 64
        Ok(u64::from_le_bytes(account.data[64..72].try_into().unwrap()))
    }
    
    /// Calculate price from sqrt_price
    /// sqrt_price is in Q64.64 format
    fn sqrt_price_to_price(sqrt_price: u128, decimals_a: u8, decimals_b: u8) -> f64 {
        // sqrt_price is Q64.64, so actual sqrt_price = sqrt_price / 2^64
        let sqrt_price_f64 = sqrt_price as f64 / (1u128 << 64) as f64;
        let price = sqrt_price_f64 * sqrt_price_f64;
        // Adjust for decimals
        let decimal_adjustment = 10f64.powi(decimals_a as i32 - decimals_b as i32);
        price * decimal_adjustment
    }
}


#[async_trait::async_trait]
impl PoolAdapter for WhirlpoolAdapter {
    fn pool_type(&self) -> PoolType {
        PoolType::OrcaWhirlpool
    }
    
    async fn get_pool_state(&self, pool_address: &Pubkey) -> Result<PoolState> {
        let account = self.rpc.get_account(pool_address)
            .map_err(|e| anyhow!("Failed to fetch Whirlpool: {}", e))?;
        
        let wp_state = self.parse_pool_state(&account.data)?;
        
        // Detect token programs for Token-2022 support
        let token_a_program = get_or_detect_token_program(&self.rpc, &wp_state.token_mint_a);
        let token_b_program = get_or_detect_token_program(&self.rpc, &wp_state.token_mint_b);
        
        // Get vault balances
        let (vault_a_balance, vault_b_balance) = self.get_vault_balances(&wp_state).await?;
        
        // Calculate price from sqrt_price (assuming 9 decimals for both, adjust as needed)
        let price = Self::sqrt_price_to_price(wp_state.sqrt_price, 9, 9);
        
        // Determine SOL liquidity
        let sol_mint = *NATIVE_MINT;
        let liquidity_sol = if wp_state.token_mint_b == sol_mint {
            vault_b_balance as f64 / 1e9
        } else if wp_state.token_mint_a == sol_mint {
            vault_a_balance as f64 / 1e9
        } else {
            0.0
        };
        
        Ok(PoolState {
            pool_type: PoolType::OrcaWhirlpool,
            address: *pool_address,
            token_a_mint: wp_state.token_mint_a,
            token_b_mint: wp_state.token_mint_b,
            token_a_reserve: vault_a_balance,
            token_b_reserve: vault_b_balance,
            token_a_program,
            token_b_program,
            price,
            liquidity_sol,
        })
    }
    
    fn parse_pool_state_from_data(&self, pool_address: &Pubkey, data: &[u8]) -> Option<PoolState> {
        let wp_state = self.parse_pool_state(data).ok()?;
        
        // Detect token programs for Token-2022 support
        let token_a_program = get_or_detect_token_program(&self.rpc, &wp_state.token_mint_a);
        let token_b_program = get_or_detect_token_program(&self.rpc, &wp_state.token_mint_b);
        
        // Whirlpool stores reserves in SEPARATE vault accounts
        // Return 0 reserves here - need vault subscription for real-time updates
        Some(PoolState {
            pool_type: PoolType::OrcaWhirlpool,
            address: *pool_address,
            token_a_mint: wp_state.token_mint_a,
            token_b_mint: wp_state.token_mint_b,
            token_a_reserve: 0, // Needs VaultSubscriber for wp_state.token_vault_a
            token_b_reserve: 0, // Needs VaultSubscriber for wp_state.token_vault_b
            token_a_program,
            token_b_program,
            price: 0.0,
            liquidity_sol: 0.0,
        })
    }
    
    fn calculate_price(&self, state: &PoolState, target_is_token_a: bool) -> f64 {
        if target_is_token_a {
            if state.token_a_reserve > 0 {
                state.token_b_reserve as f64 / state.token_a_reserve as f64
            } else {
                0.0
            }
        } else {
            if state.token_b_reserve > 0 {
                state.token_a_reserve as f64 / state.token_b_reserve as f64
            } else {
                0.0
            }
        }
    }
    
    fn calculate_amount_out(
        &self,
        state: &PoolState,
        amount_in: u64,
        is_buy: bool,
    ) -> Result<u64> {
        // Determine which token is SOL
        let sol_mint = *NATIVE_MINT;
        let sol_is_token_a = state.token_a_mint == sol_mint;
        
        let (input_reserve, output_reserve) = if is_buy {
            // SOL -> Token
            if sol_is_token_a {
                (state.token_a_reserve, state.token_b_reserve)
            } else {
                (state.token_b_reserve, state.token_a_reserve)
            }
        } else {
            // Token -> SOL
            if sol_is_token_a {
                (state.token_b_reserve, state.token_a_reserve)
            } else {
                (state.token_a_reserve, state.token_b_reserve)
            }
        };
        
        if input_reserve == 0 || output_reserve == 0 {
            return Err(anyhow!("Pool has zero liquidity"));
        }
        
        // Whirlpool fee is stored in fee_rate (basis points, typically 30 = 0.3%)
        // For estimation, use a conservative 0.3% fee
        let fee_bps = 30u64;
        let amount_in_after_fee = amount_in * (10000 - fee_bps) / 10000;
        
        // Constant product formula (simplified - actual Whirlpool uses concentrated liquidity)
        let numerator = (amount_in_after_fee as u128) * (output_reserve as u128);
        let denominator = (input_reserve as u128) + (amount_in_after_fee as u128);
        
        Ok((numerator / denominator) as u64)
    }

    
    async fn build_buy_instructions(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        sol_amount: u64,
        slippage_bps: u64,
    ) -> Result<SwapInstructions> {
        let account = self.rpc.get_account(pool_address)?;
        let wp_state = self.parse_pool_state(&account.data)?;
        let state = self.get_pool_state(pool_address).await?;
        
        let wallet_pubkey = wallet.pubkey();
        let sol_mint = *NATIVE_MINT;
        
        // Determine swap direction: SOL -> Token
        // In Whirlpool, token_a < token_b by pubkey ordering
        let a_to_b = wp_state.token_mint_a == sol_mint;
        
        // Detect token programs for Token-2022 support
        let token_a_program = get_or_detect_token_program(&self.rpc, &wp_state.token_mint_a);
        let token_b_program = get_or_detect_token_program(&self.rpc, &wp_state.token_mint_b);
        
        // Get user ATAs with correct token programs
        let user_ata_a = spl_associated_token_account::get_associated_token_address_with_program_id(
            &wallet_pubkey,
            &wp_state.token_mint_a,
            &token_a_program,
        );
        let user_ata_b = spl_associated_token_account::get_associated_token_address_with_program_id(
            &wallet_pubkey,
            &wp_state.token_mint_b,
            &token_b_program,
        );
        
        // Calculate expected output
        let expected_out = self.calculate_amount_out(&state, sol_amount, true)?;
        let min_amount_out = expected_out * (10000 - slippage_bps) / 10000;
        
        // Derive tick array addresses based on swap direction
        // For a_to_b (price going down): need current, -1, -2 tick arrays
        // For b_to_a (price going up): need current, +1, +2 tick arrays
        let (tick_array_0, tick_array_1, tick_array_2) = if a_to_b {
            (
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, 0)?,
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, -1)?,
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, -2)?,
            )
        } else {
            (
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, 0)?,
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, 1)?,
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, 2)?,
            )
        };
        
        // Derive oracle address
        let oracle = self.derive_oracle_address(pool_address)?;
        
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ];
        
        // Create ATAs if needed
        let (input_mint, input_program, output_mint, output_program, user_input_ata, user_output_ata) = if a_to_b {
            (wp_state.token_mint_a, token_a_program, wp_state.token_mint_b, token_b_program, user_ata_a, user_ata_b)
        } else {
            (wp_state.token_mint_b, token_b_program, wp_state.token_mint_a, token_a_program, user_ata_b, user_ata_a)
        };
        
        // Create input ATA (WSOL) and wrap SOL
        instructions.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &wallet_pubkey,
                &wallet_pubkey,
                &input_mint,
                &input_program,
            )
        );
        instructions.push(solana_sdk::system_instruction::transfer(
            &wallet_pubkey,
            &user_input_ata,
            sol_amount,
        ));
        instructions.push(
            spl_token::instruction::sync_native(&input_program, &user_input_ata)?
        );
        
        // Create output ATA
        instructions.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &wallet_pubkey,
                &wallet_pubkey,
                &output_mint,
                &output_program,
            )
        );
        
        // Build SwapV2 instruction
        let swap_ix = self.build_swap_v2_instruction(
            pool_address,
            &wp_state,
            &wallet_pubkey,
            &user_ata_a,
            &user_ata_b,
            &tick_array_0,
            &tick_array_1,
            &tick_array_2,
            &oracle,
            sol_amount,
            min_amount_out,
            a_to_b,
            token_a_program,
            token_b_program,
        )?;
        instructions.push(swap_ix);
        
        // Close WSOL account after swap
        instructions.push(
            spl_token::instruction::close_account(
                &input_program,
                &user_input_ata,
                &wallet_pubkey,
                &wallet_pubkey,
                &[],
            )?
        );
        
        Ok(SwapInstructions {
            instructions,
            expected_amount_out: expected_out,
        })
    }
    
    async fn build_sell_instructions(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        token_amount: u64,
        slippage_bps: u64,
    ) -> Result<SwapInstructions> {
        let account = self.rpc.get_account(pool_address)?;
        let wp_state = self.parse_pool_state(&account.data)?;
        let state = self.get_pool_state(pool_address).await?;
        
        let wallet_pubkey = wallet.pubkey();
        let sol_mint = *NATIVE_MINT;
        
        // Determine swap direction: Token -> SOL
        let a_to_b = wp_state.token_mint_a != sol_mint; // If SOL is token_b, we go a_to_b
        
        // Detect token programs for Token-2022 support
        let token_a_program = get_or_detect_token_program(&self.rpc, &wp_state.token_mint_a);
        let token_b_program = get_or_detect_token_program(&self.rpc, &wp_state.token_mint_b);
        
        // Get user ATAs with correct token programs
        let user_ata_a = spl_associated_token_account::get_associated_token_address_with_program_id(
            &wallet_pubkey,
            &wp_state.token_mint_a,
            &token_a_program,
        );
        let user_ata_b = spl_associated_token_account::get_associated_token_address_with_program_id(
            &wallet_pubkey,
            &wp_state.token_mint_b,
            &token_b_program,
        );
        
        // Calculate expected output
        let expected_out = self.calculate_amount_out(&state, token_amount, false)?;
        let min_amount_out = expected_out * (10000 - slippage_bps) / 10000;
        
        // Derive tick array addresses based on swap direction
        // For a_to_b (price going down): need current, -1, -2 tick arrays
        // For b_to_a (price going up): need current, +1, +2 tick arrays
        let (tick_array_0, tick_array_1, tick_array_2) = if a_to_b {
            (
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, 0)?,
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, -1)?,
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, -2)?,
            )
        } else {
            (
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, 0)?,
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, 1)?,
                self.derive_tick_array_address(pool_address, wp_state.tick_current_index, wp_state.tick_spacing, 2)?,
            )
        };
        
        // Derive oracle address
        let oracle = self.derive_oracle_address(pool_address)?;
        
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ];
        
        // Determine output (SOL) ATA
        let (output_mint, output_program, user_output_ata) = if a_to_b {
            (wp_state.token_mint_b, token_b_program, user_ata_b)
        } else {
            (wp_state.token_mint_a, token_a_program, user_ata_a)
        };
        
        // Create WSOL ATA for receiving SOL
        instructions.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &wallet_pubkey,
                &wallet_pubkey,
                &output_mint,
                &output_program,
            )
        );
        
        // Build SwapV2 instruction
        let swap_ix = self.build_swap_v2_instruction(
            pool_address,
            &wp_state,
            &wallet_pubkey,
            &user_ata_a,
            &user_ata_b,
            &tick_array_0,
            &tick_array_1,
            &tick_array_2,
            &oracle,
            token_amount,
            min_amount_out,
            a_to_b,
            token_a_program,
            token_b_program,
        )?;
        instructions.push(swap_ix);
        
        // Close WSOL account to unwrap SOL
        instructions.push(
            spl_token::instruction::close_account(
                &output_program,
                &user_output_ata,
                &wallet_pubkey,
                &wallet_pubkey,
                &[],
            )?
        );
        
        Ok(SwapInstructions {
            instructions,
            expected_amount_out: expected_out,
        })
    }
}


impl WhirlpoolAdapter {
    /// Derive tick array PDA address
    fn derive_tick_array_address(
        &self,
        pool_address: &Pubkey,
        tick_current_index: i32,
        tick_spacing: u16,
        offset: i32,
    ) -> Result<Pubkey> {
        const TICK_ARRAY_SIZE: i32 = 88;
        let ticks_in_array = TICK_ARRAY_SIZE * tick_spacing as i32;
        
        // Calculate start tick index
        let mut start_tick_index = (tick_current_index / ticks_in_array) * ticks_in_array;
        if tick_current_index < 0 && tick_current_index % ticks_in_array != 0 {
            start_tick_index -= ticks_in_array;
        }
        
        // Apply offset
        start_tick_index += offset * ticks_in_array;
        
        let (pda, _) = Pubkey::find_program_address(
            &[
                b"tick_array",
                pool_address.as_ref(),
                &start_tick_index.to_le_bytes(),
            ],
            &WHIRLPOOL_PROGRAM,
        );
        
        Ok(pda)
    }
    
    /// Derive oracle PDA address
    fn derive_oracle_address(&self, pool_address: &Pubkey) -> Result<Pubkey> {
        let (pda, _) = Pubkey::find_program_address(
            &[b"oracle", pool_address.as_ref()],
            &WHIRLPOOL_PROGRAM,
        );
        Ok(pda)
    }
    
    /// Build SwapV2 instruction
    /// This is the main swap instruction for Orca Whirlpools
    fn build_swap_v2_instruction(
        &self,
        pool_address: &Pubkey,
        wp_state: &WhirlpoolState,
        wallet: &Pubkey,
        user_ata_a: &Pubkey,
        user_ata_b: &Pubkey,
        tick_array_0: &Pubkey,
        tick_array_1: &Pubkey,
        tick_array_2: &Pubkey,
        oracle: &Pubkey,
        amount: u64,
        other_amount_threshold: u64,
        a_to_b: bool,
        token_program_a: Pubkey,
        token_program_b: Pubkey,
    ) -> Result<Instruction> {
        use solana_sdk::instruction::AccountMeta;
        
        // SwapV2 discriminator (from Orca SDK)
        let discriminator: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];
        
        // Build instruction data
        let mut data = Vec::with_capacity(8 + 8 + 8 + 16 + 1 + 1 + 1);
        data.extend_from_slice(&discriminator);
        data.extend_from_slice(&amount.to_le_bytes());
        data.extend_from_slice(&other_amount_threshold.to_le_bytes());
        // sqrt_price_limit: 0 means no limit
        data.extend_from_slice(&0u128.to_le_bytes());
        // amount_specified_is_input: true (ExactIn)
        data.push(1u8);
        // a_to_b
        data.push(if a_to_b { 1u8 } else { 0u8 });
        // remaining_accounts_info: None (simplified)
        data.push(0u8);
        
        // Memo program ID
        let memo_program = solana_sdk::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
        
        let accounts = vec![
            AccountMeta::new_readonly(token_program_a, false),           // token_program_a
            AccountMeta::new_readonly(token_program_b, false),           // token_program_b
            AccountMeta::new_readonly(memo_program, false),              // memo_program
            AccountMeta::new_readonly(*wallet, true),                    // token_authority (signer)
            AccountMeta::new(*pool_address, false),                      // whirlpool
            AccountMeta::new_readonly(wp_state.token_mint_a, false),     // token_mint_a
            AccountMeta::new_readonly(wp_state.token_mint_b, false),     // token_mint_b
            AccountMeta::new(*user_ata_a, false),                        // token_owner_account_a
            AccountMeta::new(wp_state.token_vault_a, false),             // token_vault_a
            AccountMeta::new(*user_ata_b, false),                        // token_owner_account_b
            AccountMeta::new(wp_state.token_vault_b, false),             // token_vault_b
            AccountMeta::new(*tick_array_0, false),                      // tick_array_0
            AccountMeta::new(*tick_array_1, false),                      // tick_array_1
            AccountMeta::new(*tick_array_2, false),                      // tick_array_2
            AccountMeta::new(*oracle, false),                            // oracle
        ];
        
        Ok(Instruction {
            program_id: WHIRLPOOL_PROGRAM,
            accounts,
            data,
        })
    }
}
