//! Meteora DLMM Pool Adapter
//! Based on: Meteora/meteora-rust-backend/src/services/dlmm_swap.rs

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
use std::str::FromStr;
use std::sync::Arc;

use super::adapter::{PoolAdapter, PoolState, SwapInstructions};
use super::types::*;
use crate::token::get_or_detect_token_program;

/// DLMM Program ID
pub const DLMM_PROGRAM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const MAX_BIN_PER_ARRAY: i32 = 70;

/// LbPair account structure (simplified for swap operations)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LbPairState {
    pub parameters: [u8; 32],
    pub v_parameters: [u8; 32],
    pub bump_seed: [u8; 1],
    pub bin_step_seed: [u8; 2],
    pub pair_type: u8,
    pub active_id: i32,
    pub bin_step: u16,
    pub status: u8,
    pub require_base_factor_seed: u8,
    pub base_factor_seed: [u8; 2],
    pub _padding1: [u8; 2],
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub protocol_fee: [u8; 32],
    pub _padding2: [u8; 32],
    pub reward_infos: [u8; 256],
    pub oracle: Pubkey,
    pub bin_array_bitmap: [u64; 16],
    pub last_updated_at: i64,
    pub whitelisted_wallet: Pubkey,
    pub pre_activation_swap_address: Pubkey,
    pub base_key: Pubkey,
    pub activation_point: u64,
    pub pre_activation_duration: u64,
    pub _padding3: [u8; 8],
    pub lock_duration: u64,
    pub creator: Pubkey,
    pub _reserved: [u8; 24],
}

unsafe impl bytemuck::Pod for LbPairState {}
unsafe impl bytemuck::Zeroable for LbPairState {}

pub struct DlmmAdapter {
    rpc: Arc<RpcClient>,
}

impl DlmmAdapter {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self { rpc }
    }
    
    /// Derive bin array PDA
    fn derive_bin_array_pda(lb_pair: &Pubkey, bin_array_index: i64) -> Pubkey {
        let program_id: Pubkey = DLMM_PROGRAM.parse().unwrap();
        let (pda, _) = Pubkey::find_program_address(
            &[b"bin_array", lb_pair.as_ref(), &bin_array_index.to_le_bytes()],
            &program_id,
        );
        pda
    }
    
    /// Derive bitmap extension PDA
    fn derive_bitmap_extension_pda(lb_pair: &Pubkey) -> Pubkey {
        let program_id: Pubkey = DLMM_PROGRAM.parse().unwrap();
        let (pda, _) = Pubkey::find_program_address(
            &[b"bitmap", lb_pair.as_ref()],
            &program_id,
        );
        pda
    }
    
    /// Derive oracle PDA (MUST be derived, not read from state)
    fn derive_oracle_pda(lb_pair: &Pubkey) -> Pubkey {
        let program_id: Pubkey = DLMM_PROGRAM.parse().unwrap();
        let (pda, _) = Pubkey::find_program_address(
            &[b"oracle", lb_pair.as_ref()],
            &program_id,
        );
        pda
    }
    
    /// Derive event authority PDA
    fn derive_event_authority_pda() -> Pubkey {
        let program_id: Pubkey = DLMM_PROGRAM.parse().unwrap();
        let (pda, _) = Pubkey::find_program_address(
            &[b"__event_authority"],
            &program_id,
        );
        pda
    }
    
    /// Convert bin ID to bin array index
    fn bin_id_to_bin_array_index(bin_id: i32) -> i32 {
        if bin_id >= 0 {
            bin_id / MAX_BIN_PER_ARRAY
        } else {
            (bin_id - MAX_BIN_PER_ARRAY + 1) / MAX_BIN_PER_ARRAY
        }
    }
    
    /// Get bin array pubkeys needed for swap
    fn get_bin_arrays_for_swap(
        lb_pair: &Pubkey,
        active_id: i32,
        swap_for_y: bool,
        count: usize,
    ) -> Vec<Pubkey> {
        let start_idx = Self::bin_id_to_bin_array_index(active_id);
        let increment: i32 = if swap_for_y { -1 } else { 1 };
        
        (0..count)
            .map(|i| Self::derive_bin_array_pda(lb_pair, (start_idx + i as i32 * increment) as i64))
            .collect()
    }
    
    /// Parse LbPair state from account data
    fn parse_lb_pair_state(&self, data: &[u8]) -> Result<LbPairState> {
        if data.len() < 8 + std::mem::size_of::<LbPairState>() {
            return Err(anyhow!("LbPair account data too small"));
        }
        let state: &LbPairState = bytemuck::from_bytes(&data[8..8 + std::mem::size_of::<LbPairState>()]);
        Ok(*state)
    }
    
    /// Calculate price from active bin
    /// DLMM price formula: price = 1.0001^(active_id)
    fn calculate_bin_price(active_id: i32, bin_step: u16) -> f64 {
        let base = 1.0 + (bin_step as f64 / 10000.0);
        base.powi(active_id)
    }
}

#[async_trait::async_trait]
impl PoolAdapter for DlmmAdapter {
    fn pool_type(&self) -> PoolType {
        PoolType::MeteoraDLMM
    }
    
    async fn get_pool_state(&self, pool_address: &Pubkey) -> Result<PoolState> {
        let account = self.rpc.get_account(pool_address)
            .map_err(|e| anyhow!("Failed to fetch DLMM pool: {}", e))?;
        
        let lb_pair = self.parse_lb_pair_state(&account.data)?;
        
        // Detect token programs
        let token_x_program = get_or_detect_token_program(&self.rpc, &lb_pair.token_x_mint);
        let token_y_program = get_or_detect_token_program(&self.rpc, &lb_pair.token_y_mint);
        
        // Fetch reserve balances
        let reserve_x_balance = self.rpc
            .get_token_account_balance(&lb_pair.reserve_x)
            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
            .unwrap_or(0);
        let reserve_y_balance = self.rpc
            .get_token_account_balance(&lb_pair.reserve_y)
            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
            .unwrap_or(0);
        
        // Calculate price from active bin
        let price = Self::calculate_bin_price(lb_pair.active_id, lb_pair.bin_step);
        
        // Determine which is SOL for liquidity calculation
        let sol_mint = *NATIVE_MINT;
        let liquidity_sol = if lb_pair.token_y_mint == sol_mint {
            reserve_y_balance as f64 / 1e9
        } else if lb_pair.token_x_mint == sol_mint {
            reserve_x_balance as f64 / 1e9
        } else {
            0.0
        };
        
        Ok(PoolState {
            pool_type: PoolType::MeteoraDLMM,
            address: *pool_address,
            token_a_mint: lb_pair.token_x_mint,
            token_b_mint: lb_pair.token_y_mint,
            token_a_reserve: reserve_x_balance,
            token_b_reserve: reserve_y_balance,
            token_a_program: token_x_program,
            token_b_program: token_y_program,
            price,
            liquidity_sol,
        })
    }
    
    fn parse_pool_state_from_data(&self, pool_address: &Pubkey, data: &[u8]) -> Option<PoolState> {
        let lb_pair = self.parse_lb_pair_state(data).ok()?;
        
        // Use cached token programs
        let token_x_program = get_or_detect_token_program(&self.rpc, &lb_pair.token_x_mint);
        let token_y_program = get_or_detect_token_program(&self.rpc, &lb_pair.token_y_mint);
        
        // DLMM price comes from active_id (bin-based pricing) - no reserves needed for price!
        let price = Self::calculate_bin_price(lb_pair.active_id, lb_pair.bin_step);
        
        // DLMM stores reserves in SEPARATE vault accounts (reserve_x, reserve_y)
        // Unlike PumpSwap, we need VaultSubscriber for real-time reserve updates
        // Return 0 reserves here - bot controller will fetch via RPC or VaultSubscriber
        Some(PoolState {
            pool_type: PoolType::MeteoraDLMM,
            address: *pool_address,
            token_a_mint: lb_pair.token_x_mint,
            token_b_mint: lb_pair.token_y_mint,
            token_a_reserve: 0, // Needs VaultSubscriber for lb_pair.reserve_x
            token_b_reserve: 0, // Needs VaultSubscriber for lb_pair.reserve_y
            token_a_program: token_x_program,
            token_b_program: token_y_program,
            price,
            liquidity_sol: 0.0,
        })
    }
    
    fn calculate_price(&self, state: &PoolState, target_is_token_a: bool) -> f64 {
        // For DLMM, price is already calculated from active bin
        if target_is_token_a {
            state.price
        } else {
            if state.price > 0.0 { 1.0 / state.price } else { 0.0 }
        }
    }
    
    fn calculate_amount_out(
        &self,
        state: &PoolState,
        amount_in: u64,
        is_buy: bool,
    ) -> Result<u64> {
        // DLMM uses bin-based pricing
        // Price represents token_y per token_x (e.g., SOL per Token if SOL is Y)
        let price = state.price;

        if price <= 0.0 {
            return Err(anyhow!("Invalid price"));
        }

        // Determine which token is SOL
        let sol_mint = *NATIVE_MINT;
        let sol_is_token_y = state.token_b_mint == sol_mint;

        // DLMM fee is typically 0.1% to 2% depending on bin_step
        // Using 0.3% (30 bps) as a conservative estimate
        let fee_bps = 30u64;
        let amount_in_after_fee = amount_in * (10000 - fee_bps) / 10000;

        let amount_out = if is_buy {
            // SOL -> Token
            if sol_is_token_y {
                // SOL is Y, Token is X: amount_out_x = amount_in_y / price
                (amount_in_after_fee as f64 / price) as u64
            } else {
                // SOL is X, Token is Y: amount_out_y = amount_in_x * price
                (amount_in_after_fee as f64 * price) as u64
            }
        } else {
            // Token -> SOL
            if sol_is_token_y {
                // Token is X, SOL is Y: amount_out_y = amount_in_x * price
                (amount_in_after_fee as f64 * price) as u64
            } else {
                // Token is Y, SOL is X: amount_out_x = amount_in_y / price
                (amount_in_after_fee as f64 / price) as u64
            }
        };

        Ok(amount_out)
    }
    
    async fn build_buy_instructions(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        sol_amount: u64,
        slippage_bps: u64,
    ) -> Result<SwapInstructions> {
        let account = self.rpc.get_account(pool_address)?;
        let lb_pair = self.parse_lb_pair_state(&account.data)?;
        let state = self.get_pool_state(pool_address).await?;
        
        let wallet_pubkey = wallet.pubkey();
        let program_id: Pubkey = DLMM_PROGRAM.parse()?;
        let sol_mint = *NATIVE_MINT;
        
        // Determine swap direction
        let swap_for_y = lb_pair.token_x_mint == sol_mint;
        
        // Get token programs
        let token_x_program = get_or_detect_token_program(&self.rpc, &lb_pair.token_x_mint);
        let token_y_program = get_or_detect_token_program(&self.rpc, &lb_pair.token_y_mint);
        
        // Get user token accounts
        let user_token_x = get_associated_token_address_with_program_id(
            &wallet_pubkey, &lb_pair.token_x_mint, &token_x_program
        );
        let user_token_y = get_associated_token_address_with_program_id(
            &wallet_pubkey, &lb_pair.token_y_mint, &token_y_program
        );
        
        let (user_token_in, user_token_out) = if swap_for_y {
            (user_token_x, user_token_y)
        } else {
            (user_token_y, user_token_x)
        };
        
        // Get bin arrays
        let bin_array_pubkeys = Self::get_bin_arrays_for_swap(
            pool_address, lb_pair.active_id, swap_for_y, 3
        );
        
        // For DLMM, set min_amount_out to 0 (built-in price protection via bins)
        let min_amount_out = 0u64;
        let expected_out = self.calculate_amount_out(&state, sol_amount, true)?;
        
        let bitmap_extension_key = Self::derive_bitmap_extension_pda(pool_address);
        let bitmap_extension_exists = self.rpc.get_account(&bitmap_extension_key).is_ok();
        let event_authority = Self::derive_event_authority_pda();
        let oracle_pda = Self::derive_oracle_pda(pool_address);
        
        // Build Swap2 instruction
        let swap2_discriminator: [u8; 8] = [65, 75, 63, 76, 235, 91, 91, 136];
        let mut ix_data = Vec::with_capacity(32);
        ix_data.extend_from_slice(&swap2_discriminator);
        ix_data.extend_from_slice(&sol_amount.to_le_bytes());
        ix_data.extend_from_slice(&min_amount_out.to_le_bytes());
        ix_data.extend_from_slice(&[0u8; 4]); // RemainingAccountsInfo empty
        
        let mut accounts = vec![
            AccountMeta::new(*pool_address, false),
            AccountMeta::new_readonly(
                if bitmap_extension_exists { bitmap_extension_key } else { program_id },
                false
            ),
            AccountMeta::new(lb_pair.reserve_x, false),
            AccountMeta::new(lb_pair.reserve_y, false),
            AccountMeta::new(user_token_in, false),
            AccountMeta::new(user_token_out, false),
            AccountMeta::new_readonly(lb_pair.token_x_mint, false),
            AccountMeta::new_readonly(lb_pair.token_y_mint, false),
            AccountMeta::new(oracle_pda, false),
            AccountMeta::new_readonly(program_id, false), // host_fee_in (None)
            AccountMeta::new_readonly(wallet_pubkey, true),
            AccountMeta::new_readonly(token_x_program, false),
            AccountMeta::new_readonly(token_y_program, false),
            AccountMeta::new_readonly(
                Pubkey::from_str("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr").unwrap(),
                false
            ),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(program_id, false),
        ];
        
        // Add bin arrays
        for bin_array_key in &bin_array_pubkeys {
            accounts.push(AccountMeta::new(*bin_array_key, false));
        }
        
        let swap_ix = Instruction { program_id, accounts, data: ix_data };
        
        // Check if ATAs exist
        let accounts_to_check = vec![user_token_in, user_token_out];
        let existing = self.rpc.get_multiple_accounts(&accounts_to_check)?;
        
        let (input_mint, output_mint, input_program, output_program) = if swap_for_y {
            (lb_pair.token_x_mint, lb_pair.token_y_mint, token_x_program, token_y_program)
        } else {
            (lb_pair.token_y_mint, lb_pair.token_x_mint, token_y_program, token_x_program)
        };
        
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
        ];
        
        // Create input ATA if needed (idempotent - safe if already exists)
        instructions.push(create_associated_token_account_idempotent(
            &wallet_pubkey, &wallet_pubkey, &input_mint, &input_program
        ));
        
        // Wrap SOL
        if input_mint == sol_mint {
            instructions.push(system_instruction::transfer(&wallet_pubkey, &user_token_in, sol_amount));
            instructions.push(spl_token::instruction::sync_native(&spl_token::id(), &user_token_in)?);
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
        let lb_pair = self.parse_lb_pair_state(&account.data)?;
        let state = self.get_pool_state(pool_address).await?;
        
        let wallet_pubkey = wallet.pubkey();
        let program_id: Pubkey = DLMM_PROGRAM.parse()?;
        let sol_mint = *NATIVE_MINT;
        
        // For sell (Token -> SOL), swap_for_y = true if SOL is token Y
        let swap_for_y = lb_pair.token_y_mint == sol_mint;
        
        let token_x_program = get_or_detect_token_program(&self.rpc, &lb_pair.token_x_mint);
        let token_y_program = get_or_detect_token_program(&self.rpc, &lb_pair.token_y_mint);
        
        let user_token_x = get_associated_token_address_with_program_id(
            &wallet_pubkey, &lb_pair.token_x_mint, &token_x_program
        );
        let user_token_y = get_associated_token_address_with_program_id(
            &wallet_pubkey, &lb_pair.token_y_mint, &token_y_program
        );
        
        let (user_token_in, user_token_out) = if swap_for_y {
            (user_token_x, user_token_y)
        } else {
            (user_token_y, user_token_x)
        };
        
        let bin_array_pubkeys = Self::get_bin_arrays_for_swap(
            pool_address, lb_pair.active_id, swap_for_y, 3
        );
        
        let min_amount_out = 0u64;
        let expected_out = self.calculate_amount_out(&state, token_amount, false)?;
        
        let bitmap_extension_key = Self::derive_bitmap_extension_pda(pool_address);
        let bitmap_extension_exists = self.rpc.get_account(&bitmap_extension_key).is_ok();
        let event_authority = Self::derive_event_authority_pda();
        let oracle_pda = Self::derive_oracle_pda(pool_address);
        
        let swap2_discriminator: [u8; 8] = [65, 75, 63, 76, 235, 91, 91, 136];
        let mut ix_data = Vec::with_capacity(32);
        ix_data.extend_from_slice(&swap2_discriminator);
        ix_data.extend_from_slice(&token_amount.to_le_bytes());
        ix_data.extend_from_slice(&min_amount_out.to_le_bytes());
        ix_data.extend_from_slice(&[0u8; 4]);
        
        let mut accounts = vec![
            AccountMeta::new(*pool_address, false),
            AccountMeta::new_readonly(
                if bitmap_extension_exists { bitmap_extension_key } else { program_id },
                false
            ),
            AccountMeta::new(lb_pair.reserve_x, false),
            AccountMeta::new(lb_pair.reserve_y, false),
            AccountMeta::new(user_token_in, false),
            AccountMeta::new(user_token_out, false),
            AccountMeta::new_readonly(lb_pair.token_x_mint, false),
            AccountMeta::new_readonly(lb_pair.token_y_mint, false),
            AccountMeta::new(oracle_pda, false),
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(wallet_pubkey, true),
            AccountMeta::new_readonly(token_x_program, false),
            AccountMeta::new_readonly(token_y_program, false),
            AccountMeta::new_readonly(
                Pubkey::from_str("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr").unwrap(),
                false
            ),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(program_id, false),
        ];
        
        for bin_array_key in &bin_array_pubkeys {
            accounts.push(AccountMeta::new(*bin_array_key, false));
        }
        
        let swap_ix = Instruction { program_id, accounts, data: ix_data };
        
        let accounts_to_check = vec![user_token_in, user_token_out];
        let existing = self.rpc.get_multiple_accounts(&accounts_to_check)?;
        
        let (input_mint, output_mint, input_program, output_program) = if swap_for_y {
            (lb_pair.token_x_mint, lb_pair.token_y_mint, token_x_program, token_y_program)
        } else {
            (lb_pair.token_y_mint, lb_pair.token_x_mint, token_y_program, token_x_program)
        };
        
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
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
                &spl_token::id(), &user_token_out, &wallet_pubkey, &wallet_pubkey, &[]
            )?);
        }
        
        Ok(SwapInstructions { instructions, expected_amount_out: expected_out })
    }
}
