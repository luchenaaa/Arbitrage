//! PumpSwap Pool Adapter
//! Based on: Pumpswap/volume-bot/src/services/pumpswap_swap.rs and sol-trade-sdk

use anyhow::{anyhow, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_program,
};
use spl_associated_token_account::get_associated_token_address_with_program_id;
use std::sync::Arc;

use super::adapter::{PoolAdapter, PoolState, SwapInstructions};
use super::types::*;
use crate::token::get_or_detect_token_program;

/// PumpSwap AMM program ID (correct one)
pub const PUMPSWAP_PROGRAM: Pubkey = solana_sdk::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");

/// PumpSwap fee program
pub const FEE_PROGRAM: Pubkey = solana_sdk::pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

/// Global account PDA
pub const GLOBAL_ACCOUNT: Pubkey = solana_sdk::pubkey!("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");

/// Event authority
pub const EVENT_AUTHORITY: Pubkey = solana_sdk::pubkey!("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR");

/// Fee recipient
pub const FEE_RECIPIENT: Pubkey = solana_sdk::pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");

/// Global volume accumulator
pub const GLOBAL_VOLUME_ACCUMULATOR: Pubkey = solana_sdk::pubkey!("C2aFPdENg4A2HQsmrd5rTw5TaYBX5Ku887cWjbFKtZpw");

/// Fee config
pub const FEE_CONFIG: Pubkey = solana_sdk::pubkey!("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx");

/// Default coin creator vault authority
pub const DEFAULT_COIN_CREATOR_VAULT_AUTHORITY: Pubkey = solana_sdk::pubkey!("8N3GDaZ2iwN65oxVatKTLPNooAVUJTbfiVJ1ahyqwjSk");

/// Associated token program
pub const ASSOCIATED_TOKEN_PROGRAM: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// WSOL mint
pub const WSOL_MINT: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");

/// SPL Token program
pub const TOKEN_PROGRAM: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Buy instruction discriminator
pub const BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

/// Sell instruction discriminator  
pub const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

/// Fee basis points
pub const LP_FEE_BASIS_POINTS: u64 = 25;
pub const PROTOCOL_FEE_BASIS_POINTS: u64 = 5;
pub const COIN_CREATOR_FEE_BASIS_POINTS: u64 = 5;


/// PumpSwap pool state structure (from on-chain data)
/// Size: 1 + 2 + 32*6 + 8 + 32 = 235 bytes (after 8-byte discriminator)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PumpSwapPoolState {
    pub pool_bump: u8,
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub lp_supply: u64,
    pub coin_creator: Pubkey,
}

pub struct PumpSwapAdapter {
    rpc: Arc<RpcClient>,
}

impl PumpSwapAdapter {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self { rpc }
    }
    
    /// Parse pool state from account data (skip 8-byte discriminator)
    fn parse_pool_state(&self, data: &[u8]) -> Result<PumpSwapPoolState> {
        if data.len() < 8 + 211 {
            return Err(anyhow!("PumpSwap pool data too small: {} bytes", data.len()));
        }
        
        // Skip 8-byte discriminator
        let data = &data[8..];
        
        let pool_bump = data[0];
        let index = u16::from_le_bytes([data[1], data[2]]);
        let creator = Pubkey::try_from(&data[3..35]).map_err(|_| anyhow!("Invalid creator pubkey"))?;
        let base_mint = Pubkey::try_from(&data[35..67]).map_err(|_| anyhow!("Invalid base_mint pubkey"))?;
        let quote_mint = Pubkey::try_from(&data[67..99]).map_err(|_| anyhow!("Invalid quote_mint pubkey"))?;
        let lp_mint = Pubkey::try_from(&data[99..131]).map_err(|_| anyhow!("Invalid lp_mint pubkey"))?;
        let pool_base_token_account = Pubkey::try_from(&data[131..163]).map_err(|_| anyhow!("Invalid pool_base_token_account pubkey"))?;
        let pool_quote_token_account = Pubkey::try_from(&data[163..195]).map_err(|_| anyhow!("Invalid pool_quote_token_account pubkey"))?;
        let lp_supply = u64::from_le_bytes(data[195..203].try_into().unwrap());
        let coin_creator = Pubkey::try_from(&data[203..235]).map_err(|_| anyhow!("Invalid coin_creator pubkey"))?;
        
        Ok(PumpSwapPoolState {
            pool_bump,
            index,
            creator,
            base_mint,
            quote_mint,
            lp_mint,
            pool_base_token_account,
            pool_quote_token_account,
            lp_supply,
            coin_creator,
        })
    }
    
    /// Get token reserves from vault accounts
    async fn get_reserves(&self, ps_state: &PumpSwapPoolState) -> Result<(u64, u64)> {
        let accounts = self.rpc.get_multiple_accounts(&[
            ps_state.pool_base_token_account,
            ps_state.pool_quote_token_account,
        ])?;
        
        let base_reserve = Self::parse_token_balance(&accounts[0])?;
        let quote_reserve = Self::parse_token_balance(&accounts[1])?;
        
        Ok((base_reserve, quote_reserve))
    }
    
    fn parse_token_balance(account: &Option<solana_sdk::account::Account>) -> Result<u64> {
        let account = account.as_ref().ok_or_else(|| anyhow!("Token account not found"))?;
        if account.data.len() < 72 {
            return Err(anyhow!("Invalid token account data"));
        }
        // Token account balance is at offset 64
        Ok(u64::from_le_bytes(account.data[64..72].try_into().unwrap()))
    }
    
    /// Derive coin creator vault authority PDA
    fn coin_creator_vault_authority(coin_creator: &Pubkey) -> Pubkey {
        let (authority, _) = Pubkey::find_program_address(
            &[b"creator_vault", coin_creator.as_ref()],
            &PUMPSWAP_PROGRAM,
        );
        authority
    }
    
    /// Get coin creator vault ATA
    fn coin_creator_vault_ata(coin_creator: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
        let authority = Self::coin_creator_vault_authority(coin_creator);
        get_associated_token_address_with_program_id(&authority, quote_mint, &TOKEN_PROGRAM)
    }
    
    /// Get fee recipient ATA
    fn fee_recipient_ata(quote_mint: &Pubkey) -> Pubkey {
        get_associated_token_address_with_program_id(&FEE_RECIPIENT, quote_mint, &TOKEN_PROGRAM)
    }
    
    /// Get user volume accumulator PDA
    fn get_user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
        let (pda, _) = Pubkey::find_program_address(
            &[b"user_volume_accumulator", user.as_ref()],
            &PUMPSWAP_PROGRAM,
        );
        pda
    }
}


#[async_trait::async_trait]
impl PoolAdapter for PumpSwapAdapter {
    fn pool_type(&self) -> PoolType {
        PoolType::PumpSwap
    }
    
    async fn get_pool_state(&self, pool_address: &Pubkey) -> Result<PoolState> {
        let account = self.rpc.get_account(pool_address)
            .map_err(|e| anyhow!("Failed to fetch PumpSwap pool: {}", e))?;
        
        let ps_state = self.parse_pool_state(&account.data)?;
        
        // Get reserves from vault accounts
        let (base_reserve, quote_reserve) = self.get_reserves(&ps_state).await?;
        
        // Detect token programs
        let base_program = get_or_detect_token_program(&self.rpc, &ps_state.base_mint);
        let quote_program = get_or_detect_token_program(&self.rpc, &ps_state.quote_mint);
        
        // Calculate price: quote_reserves / base_reserves
        let price = if base_reserve > 0 {
            quote_reserve as f64 / base_reserve as f64
        } else {
            0.0
        };
        
        // Liquidity in SOL (quote is usually SOL/WSOL)
        let liquidity_sol = quote_reserve as f64 / 1e9;
        
        Ok(PoolState {
            pool_type: PoolType::PumpSwap,
            address: *pool_address,
            token_a_mint: ps_state.base_mint,
            token_b_mint: ps_state.quote_mint,
            token_a_reserve: base_reserve,
            token_b_reserve: quote_reserve,
            token_a_program: base_program,
            token_b_program: quote_program,
            price,
            liquidity_sol,
        })
    }
    
    fn parse_pool_state_from_data(&self, pool_address: &Pubkey, data: &[u8]) -> Option<PoolState> {
        let ps_state = self.parse_pool_state(data).ok()?;
        
        // Use cached token programs
        let base_program = get_or_detect_token_program(&self.rpc, &ps_state.base_mint);
        let quote_program = get_or_detect_token_program(&self.rpc, &ps_state.quote_mint);
        
        // Note: For real-time updates, we need to fetch vault balances separately
        // This is a limitation - PumpSwap doesn't store reserves in pool account directly
        // For gRPC updates, we should subscribe to vault accounts too
        
        Some(PoolState {
            pool_type: PoolType::PumpSwap,
            address: *pool_address,
            token_a_mint: ps_state.base_mint,
            token_b_mint: ps_state.quote_mint,
            token_a_reserve: 0, // Need vault subscription for real-time
            token_b_reserve: 0,
            token_a_program: base_program,
            token_b_program: quote_program,
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
        let (input_reserve, output_reserve) = if is_buy {
            (state.token_b_reserve, state.token_a_reserve)
        } else {
            (state.token_a_reserve, state.token_b_reserve)
        };
        
        if input_reserve == 0 || output_reserve == 0 {
            return Err(anyhow!("Pool has zero liquidity"));
        }
        
        // Total fee: LP 25bps + Protocol 5bps + Creator 5bps = 35bps
        let total_fee_bps = LP_FEE_BASIS_POINTS + PROTOCOL_FEE_BASIS_POINTS + COIN_CREATOR_FEE_BASIS_POINTS;
        
        if is_buy {
            // Buy: quote -> base
            // Effective quote after fees
            let effective_quote = (amount_in as u128 * 10_000) / (10_000 + total_fee_bps) as u128;
            let numerator = (output_reserve as u128) * effective_quote;
            let denominator = (input_reserve as u128) + effective_quote;
            Ok((numerator / denominator) as u64)
        } else {
            // Sell: base -> quote
            let quote_out_raw = (output_reserve as u128) * (amount_in as u128) 
                / ((input_reserve as u128) + (amount_in as u128));
            // Deduct fees from output
            let fees = quote_out_raw * (total_fee_bps as u128) / 10_000;
            Ok((quote_out_raw - fees) as u64)
        }
    }

    
    async fn build_buy_instructions(
        &self,
        wallet: &Keypair,
        pool_address: &Pubkey,
        sol_amount: u64,
        slippage_bps: u64,
    ) -> Result<SwapInstructions> {
        let account = self.rpc.get_account(pool_address)?;
        let ps_state = self.parse_pool_state(&account.data)?;
        let (base_reserve, quote_reserve) = self.get_reserves(&ps_state).await?;
        
        let wallet_pubkey = wallet.pubkey();
        
        // Detect token programs
        let base_token_program = get_or_detect_token_program(&self.rpc, &ps_state.base_mint);
        let quote_token_program = get_or_detect_token_program(&self.rpc, &ps_state.quote_mint);
        
        // Calculate expected output and min amount with slippage
        let total_fee_bps = LP_FEE_BASIS_POINTS + PROTOCOL_FEE_BASIS_POINTS + COIN_CREATOR_FEE_BASIS_POINTS;
        let effective_quote = (sol_amount as u128 * 10_000) / (10_000 + total_fee_bps) as u128;
        let expected_out = ((base_reserve as u128) * effective_quote 
            / ((quote_reserve as u128) + effective_quote)) as u64;
        let min_amount_out = expected_out * (10000 - slippage_bps) / 10000;
        let max_quote_in = sol_amount * (10000 + slippage_bps) / 10000;
        
        // Get user ATAs
        let user_base_ata = get_associated_token_address_with_program_id(
            &wallet_pubkey,
            &ps_state.base_mint,
            &base_token_program,
        );
        let user_quote_ata = get_associated_token_address_with_program_id(
            &wallet_pubkey,
            &ps_state.quote_mint,
            &quote_token_program,
        );
        
        // Get fee and creator accounts
        let fee_recipient_ata = Self::fee_recipient_ata(&ps_state.quote_mint);
        let coin_creator_vault_authority = Self::coin_creator_vault_authority(&ps_state.coin_creator);
        let coin_creator_vault_ata = Self::coin_creator_vault_ata(&ps_state.coin_creator, &ps_state.quote_mint);
        let user_volume_accumulator = Self::get_user_volume_accumulator_pda(&wallet_pubkey);
        
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ];
        
        // Create WSOL ATA and wrap SOL
        instructions.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &wallet_pubkey,
                &wallet_pubkey,
                &ps_state.quote_mint,
                &quote_token_program,
            )
        );
        instructions.push(solana_sdk::system_instruction::transfer(
            &wallet_pubkey,
            &user_quote_ata,
            sol_amount,
        ));
        instructions.push(
            spl_token::instruction::sync_native(&quote_token_program, &user_quote_ata)?
        );
        
        // Create base token ATA if needed
        instructions.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &wallet_pubkey,
                &wallet_pubkey,
                &ps_state.base_mint,
                &base_token_program,
            )
        );
        
        // Build buy instruction with all required accounts (matching sol-trade-sdk)
        let mut ix_data = vec![0u8; 24];
        ix_data[..8].copy_from_slice(&BUY_DISCRIMINATOR);
        ix_data[8..16].copy_from_slice(&expected_out.to_le_bytes());  // base_amount_out
        ix_data[16..24].copy_from_slice(&max_quote_in.to_le_bytes()); // max_quote_amount_in
        
        let swap_accounts = vec![
            AccountMeta::new(*pool_address, false),                          // pool
            AccountMeta::new(wallet_pubkey, true),                           // user (signer)
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),                // global
            AccountMeta::new_readonly(ps_state.base_mint, false),            // base_mint
            AccountMeta::new_readonly(ps_state.quote_mint, false),           // quote_mint
            AccountMeta::new(user_base_ata, false),                          // user_base_token_account
            AccountMeta::new(user_quote_ata, false),                         // user_quote_token_account
            AccountMeta::new(ps_state.pool_base_token_account, false),       // pool_base_token_account
            AccountMeta::new(ps_state.pool_quote_token_account, false),      // pool_quote_token_account
            AccountMeta::new_readonly(FEE_RECIPIENT, false),                 // fee_recipient
            AccountMeta::new(fee_recipient_ata, false),                      // fee_recipient_ata
            AccountMeta::new_readonly(base_token_program, false),            // base_token_program
            AccountMeta::new_readonly(quote_token_program, false),           // quote_token_program
            AccountMeta::new_readonly(system_program::id(), false),          // system_program
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM, false),      // associated_token_program
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),               // event_authority
            AccountMeta::new_readonly(PUMPSWAP_PROGRAM, false),              // amm_program
            AccountMeta::new(coin_creator_vault_ata, false),                 // coin_creator_vault_ata
            AccountMeta::new_readonly(coin_creator_vault_authority, false),  // coin_creator_vault_authority
            AccountMeta::new(GLOBAL_VOLUME_ACCUMULATOR, false),              // global_volume_accumulator
            AccountMeta::new(user_volume_accumulator, false),                // user_volume_accumulator
            AccountMeta::new_readonly(FEE_CONFIG, false),                    // fee_config
            AccountMeta::new_readonly(FEE_PROGRAM, false),                   // fee_program
        ];
        
        instructions.push(Instruction {
            program_id: PUMPSWAP_PROGRAM,
            accounts: swap_accounts,
            data: ix_data,
        });
        
        // Close WSOL account after swap
        instructions.push(
            spl_token::instruction::close_account(
                &quote_token_program,
                &user_quote_ata,
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
        let ps_state = self.parse_pool_state(&account.data)?;
        let (base_reserve, quote_reserve) = self.get_reserves(&ps_state).await?;
        
        let wallet_pubkey = wallet.pubkey();
        
        // Detect token programs
        let base_token_program = get_or_detect_token_program(&self.rpc, &ps_state.base_mint);
        let quote_token_program = get_or_detect_token_program(&self.rpc, &ps_state.quote_mint);
        
        // Calculate expected output and min amount with slippage
        let total_fee_bps = LP_FEE_BASIS_POINTS + PROTOCOL_FEE_BASIS_POINTS + COIN_CREATOR_FEE_BASIS_POINTS;
        let quote_out_raw = (quote_reserve as u128) * (token_amount as u128) 
            / ((base_reserve as u128) + (token_amount as u128));
        let fees = quote_out_raw * (total_fee_bps as u128) / 10_000;
        let expected_out = (quote_out_raw - fees) as u64;
        let min_amount_out = expected_out * (10000 - slippage_bps) / 10000;
        
        // Get user ATAs
        let user_base_ata = get_associated_token_address_with_program_id(
            &wallet_pubkey,
            &ps_state.base_mint,
            &base_token_program,
        );
        let user_quote_ata = get_associated_token_address_with_program_id(
            &wallet_pubkey,
            &ps_state.quote_mint,
            &quote_token_program,
        );
        
        // Get fee and creator accounts
        let fee_recipient_ata = Self::fee_recipient_ata(&ps_state.quote_mint);
        let coin_creator_vault_authority = Self::coin_creator_vault_authority(&ps_state.coin_creator);
        let coin_creator_vault_ata = Self::coin_creator_vault_ata(&ps_state.coin_creator, &ps_state.quote_mint);
        
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        ];
        
        // Create WSOL ATA for receiving SOL
        instructions.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &wallet_pubkey,
                &wallet_pubkey,
                &ps_state.quote_mint,
                &quote_token_program,
            )
        );
        
        // Build sell instruction with all required accounts (matching sol-trade-sdk)
        // For sell: quote is WSOL, so we DON'T add volume accumulator accounts
        // (volume accumulators are only for buy when quote is WSOL, or sell when quote is NOT WSOL)
        let mut ix_data = vec![0u8; 24];
        ix_data[..8].copy_from_slice(&SELL_DISCRIMINATOR);
        ix_data[8..16].copy_from_slice(&token_amount.to_le_bytes());    // base_amount_in
        ix_data[16..24].copy_from_slice(&min_amount_out.to_le_bytes()); // min_quote_amount_out
        
        let swap_accounts = vec![
            AccountMeta::new(*pool_address, false),                          // pool
            AccountMeta::new(wallet_pubkey, true),                           // user (signer)
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),                // global
            AccountMeta::new_readonly(ps_state.base_mint, false),            // base_mint
            AccountMeta::new_readonly(ps_state.quote_mint, false),           // quote_mint
            AccountMeta::new(user_base_ata, false),                          // user_base_token_account
            AccountMeta::new(user_quote_ata, false),                         // user_quote_token_account
            AccountMeta::new(ps_state.pool_base_token_account, false),       // pool_base_token_account
            AccountMeta::new(ps_state.pool_quote_token_account, false),      // pool_quote_token_account
            AccountMeta::new_readonly(FEE_RECIPIENT, false),                 // fee_recipient
            AccountMeta::new(fee_recipient_ata, false),                      // fee_recipient_ata
            AccountMeta::new_readonly(base_token_program, false),            // base_token_program
            AccountMeta::new_readonly(quote_token_program, false),           // quote_token_program
            AccountMeta::new_readonly(system_program::id(), false),          // system_program
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM, false),      // associated_token_program
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),               // event_authority
            AccountMeta::new_readonly(PUMPSWAP_PROGRAM, false),              // amm_program
            AccountMeta::new(coin_creator_vault_ata, false),                 // coin_creator_vault_ata
            AccountMeta::new_readonly(coin_creator_vault_authority, false),  // coin_creator_vault_authority
            // Note: No volume accumulator accounts for sell when quote is WSOL (standard case)
            AccountMeta::new_readonly(FEE_CONFIG, false),                    // fee_config
            AccountMeta::new_readonly(FEE_PROGRAM, false),                   // fee_program
        ];
        
        instructions.push(Instruction {
            program_id: PUMPSWAP_PROGRAM,
            accounts: swap_accounts,
            data: ix_data,
        });
        
        // Close WSOL account to unwrap SOL
        instructions.push(
            spl_token::instruction::close_account(
                &quote_token_program,
                &user_quote_ata,
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
