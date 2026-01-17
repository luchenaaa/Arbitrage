//! Meteora DAMM V2 Pool Adapter
//! Based on: Meteora/meteora-rust-backend/src/services/meteora_swap.rs

use anyhow::{anyhow, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
};
use spl_associated_token_account::{
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use std::sync::Arc;

use super::adapter::{PoolAdapter, PoolState, SwapInstructions};
use super::types::*;
use crate::token::get_or_detect_token_program;

/// Meteora DAMM V2 (CPAMM) Program ID
pub const DAMM_V2_PROGRAM: &str = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG";

/// Pool state structure for DAMM V2 (cp_amm)
/// Based on: Meteora/damm-v2/programs/cp-amm/src/state/pool.rs
/// Total size: 1104 bytes (after 8-byte discriminator)
/// 
/// IMPORTANT: The volume bot uses `cp_amm::state::Pool` directly via bytemuck.
/// We replicate the exact layout here for parsing.
/// 
/// PoolFeesStruct = 160 bytes (BaseFeeStruct 40 + 8 + DynamicFeeStruct 96 + 16 padding)
/// const_assert_eq!(PoolFeesStruct::INIT_SPACE, 160);
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DammV2PoolState {
    // pool_fees: PoolFeesStruct (comes FIRST!)
    // PoolFeesStruct is 160 bytes based on the program (verified from fee.rs)
    pub pool_fees: [u8; 160],
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
    pub token_a_vault: Pubkey,
    pub token_b_vault: Pubkey,
    pub whitelisted_vault: Pubkey,
    pub partner: Pubkey,
    pub liquidity: u128,
    pub _padding: u128,
    pub protocol_a_fee: u64,
    pub protocol_b_fee: u64,
    pub partner_a_fee: u64,
    pub partner_b_fee: u64,
    pub sqrt_min_price: u128,
    pub sqrt_max_price: u128,
    pub sqrt_price: u128,
    pub activation_point: u64,
    pub activation_type: u8,
    pub pool_status: u8,
    pub token_a_flag: u8,
    pub token_b_flag: u8,
    pub collect_fee_mode: u8,
    pub pool_type: u8,
    pub version: u8,
    pub _padding_0: u8,
    pub fee_a_per_liquidity: [u8; 32],
    pub fee_b_per_liquidity: [u8; 32],
    pub permanent_lock_liquidity: u128,
    pub metrics: [u8; 80],  // PoolMetrics
    pub creator: Pubkey,
    pub _padding_1: [u64; 6],
    pub reward_infos: [u8; 384],  // [RewardInfo; 2], each 192 bytes
}

unsafe impl bytemuck::Pod for DammV2PoolState {}
unsafe impl bytemuck::Zeroable for DammV2PoolState {}

lazy_static::lazy_static! {
    /// Pool authority PDA (derived once)
    static ref POOL_AUTHORITY: Pubkey = {
        let program_id: Pubkey = DAMM_V2_PROGRAM.parse().unwrap();
        let (pda, _) = Pubkey::find_program_address(
            &[b"vault_and_lp_mint_auth_seed"],
            &program_id,
        );
        pda
    };
    
    /// Event authority PDA
    static ref EVENT_AUTHORITY: Pubkey = {
        let program_id: Pubkey = DAMM_V2_PROGRAM.parse().unwrap();
        let (pda, _) = Pubkey::find_program_address(
            &[b"__event_authority"],
            &program_id,
        );
        pda
    };
}

pub struct DammV2Adapter {
    rpc: Arc<RpcClient>,
}

impl DammV2Adapter {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self { rpc }
    }
    
    /// Parse pool state from account data
    /// The volume bot uses: `let pool: &Pool = bytemuck::from_bytes(&data_without_discriminator);`
    fn parse_pool_state(&self, data: &[u8]) -> Result<DammV2PoolState> {
        // Pool size is 1104 bytes + 8 byte discriminator = 1112 bytes minimum
        if data.len() < 8 + 1104 {
            return Err(anyhow!("DAMM V2 pool data too small: {} bytes (need 1112)", data.len()));
        }
        let state: &DammV2PoolState = bytemuck::from_bytes(
            &data[8..8 + std::mem::size_of::<DammV2PoolState>()]
        );
        Ok(*state)
    }
    
    /// Build swap instruction data
    fn build_swap_data(&self, amount_in: u64, minimum_amount_out: u64) -> Vec<u8> {
        // Swap instruction discriminator for cp_amm
        let discriminator: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
        
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&discriminator);
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&minimum_amount_out.to_le_bytes());
        data
    }
}

#[async_trait::async_trait]
impl PoolAdapter for DammV2Adapter {
    fn pool_type(&self) -> PoolType {
        PoolType::MeteoraDammV2
    }
    
    async fn get_pool_state(&self, pool_address: &Pubkey) -> Result<PoolState> {
        let account = self.rpc.get_account(pool_address)
            .map_err(|e| anyhow!("Failed to fetch DAMM V2 pool: {}", e))?;
        
        let damm_state = self.parse_pool_state(&account.data)?;
        
        // Detect token programs
        let token_a_program = get_or_detect_token_program(&self.rpc, &damm_state.token_a_mint);
        let token_b_program = get_or_detect_token_program(&self.rpc, &damm_state.token_b_mint);
        
        // Fetch vault balances
        let vault_a_balance = self.rpc
            .get_token_account_balance(&damm_state.token_a_vault)
            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
            .unwrap_or(0);
        let vault_b_balance = self.rpc
            .get_token_account_balance(&damm_state.token_b_vault)
            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
            .unwrap_or(0);
        
        // Calculate price: token_b / token_a
        let price = if vault_a_balance > 0 {
            vault_b_balance as f64 / vault_a_balance as f64
        } else {
            0.0
        };
        
        // Determine SOL liquidity
        let sol_mint = *NATIVE_MINT;
        let liquidity_sol = if damm_state.token_b_mint == sol_mint {
            vault_b_balance as f64 / 1e9
        } else if damm_state.token_a_mint == sol_mint {
            vault_a_balance as f64 / 1e9
        } else {
            0.0
        };
        
        Ok(PoolState {
            pool_type: PoolType::MeteoraDammV2,
            address: *pool_address,
            token_a_mint: damm_state.token_a_mint,
            token_b_mint: damm_state.token_b_mint,
            token_a_reserve: vault_a_balance,
            token_b_reserve: vault_b_balance,
            token_a_program,
            token_b_program,
            price,
            liquidity_sol,
        })
    }
    
    fn parse_pool_state_from_data(&self, pool_address: &Pubkey, data: &[u8]) -> Option<PoolState> {
        let damm_state = self.parse_pool_state(data).ok()?;
        
        // Use cached token programs
        let token_a_program = get_or_detect_token_program(&self.rpc, &damm_state.token_a_mint);
        let token_b_program = get_or_detect_token_program(&self.rpc, &damm_state.token_b_mint);
        
        // DAMM V2 stores reserves in SEPARATE vault accounts (token_a_vault, token_b_vault)
        // Unlike PumpSwap, we need VaultSubscriber for real-time reserve updates
        // Return 0 reserves here - bot controller will fetch via RPC or VaultSubscriber
        Some(PoolState {
            pool_type: PoolType::MeteoraDammV2,
            address: *pool_address,
            token_a_mint: damm_state.token_a_mint,
            token_b_mint: damm_state.token_b_mint,
            token_a_reserve: 0, // Needs VaultSubscriber for damm_state.token_a_vault
            token_b_reserve: 0, // Needs VaultSubscriber for damm_state.token_b_vault
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
        // Determine which token is SOL to get correct reserves
        let sol_mint = *NATIVE_MINT;
        let sol_is_token_a = state.token_a_mint == sol_mint;

        let (input_reserve, output_reserve) = if is_buy {
            // SOL -> Token: SOL is input
            if sol_is_token_a {
                (state.token_a_reserve, state.token_b_reserve)
            } else {
                (state.token_b_reserve, state.token_a_reserve)
            }
        } else {
            // Token -> SOL: Token is input
            if sol_is_token_a {
                (state.token_b_reserve, state.token_a_reserve)
            } else {
                (state.token_a_reserve, state.token_b_reserve)
            }
        };

        if input_reserve == 0 || output_reserve == 0 {
            return Err(anyhow!("Pool has zero liquidity"));
        }

        // Constant product with 0.25% fee (25 bps)
        let fee_bps = 25u64;
        let amount_in_after_fee = amount_in * (10000 - fee_bps) / 10000;

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
        let damm_state = self.parse_pool_state(&account.data)?;
        let state = self.get_pool_state(pool_address).await?;
        
        let wallet_pubkey = wallet.pubkey();
        let program_id: Pubkey = DAMM_V2_PROGRAM.parse()?;
        let sol_mint = *NATIVE_MINT;
        
        // Determine swap direction
        let is_a_to_b = damm_state.token_a_mint == sol_mint;
        
        let (input_mint, output_mint) = if is_a_to_b {
            (damm_state.token_a_mint, damm_state.token_b_mint)
        } else {
            (damm_state.token_b_mint, damm_state.token_a_mint)
        };
        
        let input_program = get_or_detect_token_program(&self.rpc, &input_mint);
        let output_program = get_or_detect_token_program(&self.rpc, &output_mint);
        
        let input_token_account = get_associated_token_address_with_program_id(
            &wallet_pubkey, &input_mint, &input_program
        );
        let output_token_account = get_associated_token_address_with_program_id(
            &wallet_pubkey, &output_mint, &output_program
        );
        
        // Calculate minimum output
        let expected_out = self.calculate_amount_out(&state, sol_amount, true)?;
        let minimum_amount_out = expected_out * (10000 - slippage_bps) / 10000;
        
        let ix_data = self.build_swap_data(sol_amount, minimum_amount_out);
        
        let accounts = vec![
            AccountMeta::new_readonly(*POOL_AUTHORITY, false),
            AccountMeta::new(*pool_address, false),
            AccountMeta::new(input_token_account, false),
            AccountMeta::new(output_token_account, false),
            AccountMeta::new(damm_state.token_a_vault, false),
            AccountMeta::new(damm_state.token_b_vault, false),
            AccountMeta::new_readonly(damm_state.token_a_mint, false),
            AccountMeta::new_readonly(damm_state.token_b_mint, false),
            AccountMeta::new_readonly(wallet_pubkey, true),
            AccountMeta::new_readonly(input_program, false),
            AccountMeta::new_readonly(output_program, false),
            AccountMeta::new_readonly(program_id, false), // referral (none)
            AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(program_id, false),
        ];
        
        let swap_ix = Instruction { program_id, accounts, data: ix_data };
        
        // Check if ATAs exist
        let accounts_to_check = vec![input_token_account, output_token_account];
        let existing = self.rpc.get_multiple_accounts(&accounts_to_check)?;
        
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ];
        
        // Create input ATA if needed (idempotent - safe if already exists)
        instructions.push(create_associated_token_account_idempotent(
            &wallet_pubkey, &wallet_pubkey, &input_mint, &input_program
        ));
        
        // Wrap SOL
        if input_mint == sol_mint {
            instructions.push(system_instruction::transfer(&wallet_pubkey, &input_token_account, sol_amount));
            instructions.push(spl_token::instruction::sync_native(&spl_token::id(), &input_token_account)?);
        }
        
        // Create output ATA if needed (idempotent - safe if already exists)
        instructions.push(create_associated_token_account_idempotent(
            &wallet_pubkey, &wallet_pubkey, &output_mint, &output_program
        ));
        
        instructions.push(swap_ix);
        
        Ok(SwapInstructions { instructions, expected_amount_out: expected_out })
    }
    
    async fn build_sell_instructions(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        token_amount: u64,
        slippage_bps: u64,
    ) -> Result<SwapInstructions> {
        let account = self.rpc.get_account(pool_address)?;
        let damm_state = self.parse_pool_state(&account.data)?;
        let state = self.get_pool_state(pool_address).await?;
        
        let wallet_pubkey = wallet.pubkey();
        let program_id: Pubkey = DAMM_V2_PROGRAM.parse()?;
        let sol_mint = *NATIVE_MINT;
        
        // For sell (Token -> SOL), determine direction
        let is_a_to_b = damm_state.token_b_mint == sol_mint;
        
        let (input_mint, output_mint) = if is_a_to_b {
            (damm_state.token_a_mint, damm_state.token_b_mint)
        } else {
            (damm_state.token_b_mint, damm_state.token_a_mint)
        };
        
        let input_program = get_or_detect_token_program(&self.rpc, &input_mint);
        let output_program = get_or_detect_token_program(&self.rpc, &output_mint);
        
        let input_token_account = get_associated_token_address_with_program_id(
            &wallet_pubkey, &input_mint, &input_program
        );
        let output_token_account = get_associated_token_address_with_program_id(
            &wallet_pubkey, &output_mint, &output_program
        );
        
        let expected_out = self.calculate_amount_out(&state, token_amount, false)?;
        let minimum_amount_out = expected_out * (10000 - slippage_bps) / 10000;
        
        let ix_data = self.build_swap_data(token_amount, minimum_amount_out);
        
        let accounts = vec![
            AccountMeta::new_readonly(*POOL_AUTHORITY, false),
            AccountMeta::new(*pool_address, false),
            AccountMeta::new(input_token_account, false),
            AccountMeta::new(output_token_account, false),
            AccountMeta::new(damm_state.token_a_vault, false),
            AccountMeta::new(damm_state.token_b_vault, false),
            AccountMeta::new_readonly(damm_state.token_a_mint, false),
            AccountMeta::new_readonly(damm_state.token_b_mint, false),
            AccountMeta::new_readonly(wallet_pubkey, true),
            AccountMeta::new_readonly(input_program, false),
            AccountMeta::new_readonly(output_program, false),
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(program_id, false),
        ];
        
        let swap_ix = Instruction { program_id, accounts, data: ix_data };
        
        let accounts_to_check = vec![input_token_account, output_token_account];
        let existing = self.rpc.get_multiple_accounts(&accounts_to_check)?;
        
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ];
        
        // Create ATAs if needed (idempotent - safe if already exists from buy)
        instructions.push(create_associated_token_account_idempotent(
            &wallet_pubkey, &wallet_pubkey, &input_mint, &input_program
        ));
        
        instructions.push(create_associated_token_account_idempotent(
            &wallet_pubkey, &wallet_pubkey, &output_mint, &output_program
        ));
        
        instructions.push(swap_ix);
        
        // Close WSOL account if output is SOL
        if output_mint == sol_mint {
            instructions.push(spl_token::instruction::close_account(
                &spl_token::id(), &output_token_account, &wallet_pubkey, &wallet_pubkey, &[]
            )?);
        }
        
        Ok(SwapInstructions { instructions, expected_amount_out: expected_out })
    }
}
