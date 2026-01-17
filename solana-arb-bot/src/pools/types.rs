use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// Supported pool types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolType {
    PumpSwap,
    MeteoraDLMM,
    MeteoraDammV2,
    RaydiumCPMM,
    OrcaWhirlpool,
    Unsupported,
}

impl PoolType {
    /// Get the program ID for this pool type
    pub fn program_id(&self) -> Option<Pubkey> {
        match self {
            PoolType::PumpSwap => Some(*PUMPSWAP_PROGRAM_ID),
            PoolType::MeteoraDLMM => Some(*METEORA_DLMM_PROGRAM_ID),
            PoolType::MeteoraDammV2 => Some(*METEORA_DAMM_V2_PROGRAM_ID),
            PoolType::RaydiumCPMM => Some(*RAYDIUM_CPMM_PROGRAM_ID),
            PoolType::OrcaWhirlpool => Some(*ORCA_WHIRLPOOL_PROGRAM_ID),
            PoolType::Unsupported => None,
        }
    }
    
    /// Check if this pool type is supported for trading
    pub fn is_supported(&self) -> bool {
        !matches!(self, PoolType::Unsupported)
    }
}

/// Validated pool information
#[derive(Debug, Clone)]
pub struct ValidatedPool {
    pub address: Pubkey,
    pub pool_type: PoolType,
    pub token_a_mint: Option<Pubkey>,
    pub token_b_mint: Option<Pubkey>,
}

// Program IDs
lazy_static::lazy_static! {
    pub static ref PUMPSWAP_PROGRAM_ID: Pubkey = 
        Pubkey::from_str("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA").unwrap();
    
    pub static ref METEORA_DLMM_PROGRAM_ID: Pubkey = 
        Pubkey::from_str("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo").unwrap();
    
    pub static ref METEORA_DAMM_V2_PROGRAM_ID: Pubkey = 
        Pubkey::from_str("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG").unwrap();
    
    pub static ref RAYDIUM_CPMM_PROGRAM_ID: Pubkey = 
        Pubkey::from_str("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C").unwrap();
    
    pub static ref ORCA_WHIRLPOOL_PROGRAM_ID: Pubkey = 
        Pubkey::from_str("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc").unwrap();
    
    // Token Programs
    pub static ref SPL_TOKEN_PROGRAM_ID: Pubkey = 
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    
    pub static ref TOKEN_2022_PROGRAM_ID: Pubkey = 
        Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap();
    
    pub static ref NATIVE_MINT: Pubkey = 
        Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
}
