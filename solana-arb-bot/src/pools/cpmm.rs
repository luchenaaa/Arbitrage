//! Raydium CPMM Pool Adapter
//! Based on: Raydium/volume-bot/src/services/raydium_swap.rs

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

/// Raydium CPMM Program ID
pub const CPMM_PROGRAM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

/// Raydium CPMM Pool State
/// CRITICAL: Contains token_0_program and token_1_program for Token-2022 support
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CpmmPoolState {
    pub amm_config: Pubkey,
    pub pool_creator: Pubkey,
    pub token_0_vault: Pubkey,
    pub token_1_vault: Pubkey,
    pub lp_mint: Pubkey,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,
    pub token_0_program: Pubkey,  // Token program for token 0 (SPL or Token-2022)
    pub token_1_program: Pubkey,  // Token program for token 1 (SPL or Token-2022)
    pub observation_key: Pubkey,
    pub auth_bump: u8,
    pub status: u8,
    pub lp_mint_decimals: u8,
    pub mint_0_decimals: u8,
    pub mint_1_decimals: u8,
    pub _padding: [u8; 3],
    pub lp_supply: u64,
    pub protocol_fees_token_0: u64,
    pub protocol_fees_token_1: u64,
    pub fund_fees_token_0: u64,
    pub fund_fees_token_1: u64,
    pub open_time: u64,
    pub _reserved: [u8; 32],
}

unsafe impl bytemuck::Pod for CpmmPoolState {}
unsafe impl bytemuck::Zeroable for CpmmPoolState {}

lazy_static::lazy_static! {
    /// Authority PDA
    static ref CPMM_AUTHORITY: Pubkey = {
        let program_id: Pubkey = CPMM_PROGRAM.parse().unwrap();
        let (pda, _) = Pubkey::find_program_address(
            &[b"vault_and_lp_mint_auth_seed"],
            &program_id,
        );
        pda
    };
}

pub struct CpmmAdapter {
    rpc: Arc<RpcClient>,
}

impl CpmmAdapter {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self { rpc }
    }
    
    /// Parse pool state from account data
    fn parse_pool_state(&self, data: &[u8]) -> Result<CpmmPoolState> {
        if data.len() < 8 + std::mem::size_of::<CpmmPoolState>() {
            return Err(anyhow!("CPMM pool data too small"));
        }
        let state: &CpmmPoolState = bytemuck::from_bytes(
            &data[8..8 + std::mem::size_of::<CpmmPoolState>()]
        );
        Ok(*state)
    }
    
    /// Build swap instruction data
    fn build_swap_data(&self, amount_in: u64, minimum_amount_out: u64) -> Vec<u8> {
        // SwapBaseInput discriminator
        let discriminator: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];
        
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&discriminator);
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&minimum_amount_out.to_le_bytes());
        data
    }
}

#[async_trait::async_trait]
impl PoolAdapter for CpmmAdapter {
    fn pool_type(&self) -> PoolType {
        PoolType::RaydiumCPMM
    }
    
    async fn get_pool_state(&self, pool_address: &Pubkey) -> Result<PoolState> {
        let account = self.rpc.get_account(pool_address)
            .map_err(|e| anyhow!("Failed to fetch CPMM pool: {}", e))?;
        
        let cpmm_state = self.parse_pool_state(&account.data)?;
        
        // CPMM stores token programs in pool state - use them directly!
        let token_0_program = cpmm_state.token_0_program;
        let token_1_program = cpmm_state.token_1_program;
        
        // Fetch vault balances
        let vault_0_balance = self.rpc
            .get_token_account_balance(&cpmm_state.token_0_vault)
            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
            .unwrap_or(0);
        let vault_1_balance = self.rpc
            .get_token_account_balance(&cpmm_state.token_1_vault)
            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
            .unwrap_or(0);
        
        // Calculate price
        let price = if vault_0_balance > 0 {
            vault_1_balance as f64 / vault_0_balance as f64
        } else {
            0.0
        };
        
        // Determine SOL liquidity
        let sol_mint = *NATIVE_MINT;
        let liquidity_sol = if cpmm_state.token_1_mint == sol_mint {
            vault_1_balance as f64 / 1e9
        } else if cpmm_state.token_0_mint == sol_mint {
            vault_0_balance as f64 / 1e9
        } else {
            0.0
        };
        
        Ok(PoolState {
            pool_type: PoolType::RaydiumCPMM,
            address: *pool_address,
            token_a_mint: cpmm_state.token_0_mint,
            token_b_mint: cpmm_state.token_1_mint,
            token_a_reserve: vault_0_balance,
            token_b_reserve: vault_1_balance,
            token_a_program: token_0_program,
            token_b_program: token_1_program,
            price,
            liquidity_sol,
        })
    }
    
    fn parse_pool_state_from_data(&self, pool_address: &Pubkey, data: &[u8]) -> Option<PoolState> {
        let cpmm_state = self.parse_pool_state(data).ok()?;
        
        // CPMM stores token programs in pool state - use them directly! (critical for Token-2022)
        let token_0_program = cpmm_state.token_0_program;
        let token_1_program = cpmm_state.token_1_program;
        
        // CPMM stores reserves in SEPARATE vault accounts (token_0_vault, token_1_vault)
        // Unlike PumpSwap, we need VaultSubscriber for real-time reserve updates
        // Return 0 reserves here - bot controller will fetch via RPC or VaultSubscriber
        Some(PoolState {
            pool_type: PoolType::RaydiumCPMM,
            address: *pool_address,
            token_a_mint: cpmm_state.token_0_mint,
            token_b_mint: cpmm_state.token_1_mint,
            token_a_reserve: 0, // Needs VaultSubscriber for cpmm_state.token_0_vault
            token_b_reserve: 0, // Needs VaultSubscriber for cpmm_state.token_1_vault
            token_a_program: token_0_program,
            token_b_program: token_1_program,
            price: 0.0, // Can't calculate without reserves
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

        // Raydium CPMM has 0.25% fee (25 bps)
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
        let cpmm_state = self.parse_pool_state(&account.data)?;
        let state = self.get_pool_state(pool_address).await?;
        
        let wallet_pubkey = wallet.pubkey();
        let program_id: Pubkey = CPMM_PROGRAM.parse()?;
        let sol_mint = *NATIVE_MINT;
        
        // Determine if token_0 is SOL (input)
        let input_is_token_0 = cpmm_state.token_0_mint == sol_mint;
        
        // Get token programs from pool state (critical for Token-2022!)
        let (input_vault, output_vault, input_mint, output_mint, input_program, output_program) = 
            if input_is_token_0 {
                (
                    cpmm_state.token_0_vault,
                    cpmm_state.token_1_vault,
                    cpmm_state.token_0_mint,
                    cpmm_state.token_1_mint,
                    cpmm_state.token_0_program,
                    cpmm_state.token_1_program,
                )
            } else {
                (
                    cpmm_state.token_1_vault,
                    cpmm_state.token_0_vault,
                    cpmm_state.token_1_mint,
                    cpmm_state.token_0_mint,
                    cpmm_state.token_1_program,
                    cpmm_state.token_0_program,
                )
            };
        
        let input_token_account = get_associated_token_address_with_program_id(
            &wallet_pubkey, &input_mint, &input_program
        );
        let output_token_account = get_associated_token_address_with_program_id(
            &wallet_pubkey, &output_mint, &output_program
        );
        
        let expected_out = self.calculate_amount_out(&state, sol_amount, true)?;
        let minimum_amount_out = expected_out * (10000 - slippage_bps) / 10000;
        
        let ix_data = self.build_swap_data(sol_amount, minimum_amount_out);
        
        let accounts = vec![
            AccountMeta::new_readonly(wallet_pubkey, true),           // payer
            AccountMeta::new_readonly(*CPMM_AUTHORITY, false),        // authority
            AccountMeta::new_readonly(cpmm_state.amm_config, false),  // amm_config
            AccountMeta::new(*pool_address, false),                   // pool_state
            AccountMeta::new(input_token_account, false),             // input_token_account
            AccountMeta::new(output_token_account, false),            // output_token_account
            AccountMeta::new(input_vault, false),                     // input_vault
            AccountMeta::new(output_vault, false),                    // output_vault
            AccountMeta::new_readonly(input_program, false),          // input_token_program
            AccountMeta::new_readonly(output_program, false),         // output_token_program
            AccountMeta::new_readonly(input_mint, false),             // input_token_mint
            AccountMeta::new_readonly(output_mint, false),            // output_token_mint
            AccountMeta::new(cpmm_state.observation_key, false),      // observation_state
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
        let cpmm_state = self.parse_pool_state(&account.data)?;
        let state = self.get_pool_state(pool_address).await?;
        
        let wallet_pubkey = wallet.pubkey();
        let program_id: Pubkey = CPMM_PROGRAM.parse()?;
        let sol_mint = *NATIVE_MINT;
        
        // For sell, token is input, SOL is output
        let input_is_token_0 = cpmm_state.token_0_mint != sol_mint;
        
        let (input_vault, output_vault, input_mint, output_mint, input_program, output_program) = 
            if input_is_token_0 {
                (
                    cpmm_state.token_0_vault,
                    cpmm_state.token_1_vault,
                    cpmm_state.token_0_mint,
                    cpmm_state.token_1_mint,
                    cpmm_state.token_0_program,
                    cpmm_state.token_1_program,
                )
            } else {
                (
                    cpmm_state.token_1_vault,
                    cpmm_state.token_0_vault,
                    cpmm_state.token_1_mint,
                    cpmm_state.token_0_mint,
                    cpmm_state.token_1_program,
                    cpmm_state.token_0_program,
                )
            };
        
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
            AccountMeta::new_readonly(wallet_pubkey, true),
            AccountMeta::new_readonly(*CPMM_AUTHORITY, false),
            AccountMeta::new_readonly(cpmm_state.amm_config, false),
            AccountMeta::new(*pool_address, false),
            AccountMeta::new(input_token_account, false),
            AccountMeta::new(output_token_account, false),
            AccountMeta::new(input_vault, false),
            AccountMeta::new(output_vault, false),
            AccountMeta::new_readonly(input_program, false),
            AccountMeta::new_readonly(output_program, false),
            AccountMeta::new_readonly(input_mint, false),
            AccountMeta::new_readonly(output_mint, false),
            AccountMeta::new(cpmm_state.observation_key, false),
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
