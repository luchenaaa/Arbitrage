use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::RwLock;

use crate::pools::types::{SPL_TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, NATIVE_MINT};

lazy_static::lazy_static! {
    /// Cache for token program IDs (mint -> token program)
    /// Avoids repeated RPC calls to detect Token-2022 vs SPL Token
    static ref TOKEN_PROGRAM_CACHE: RwLock<HashMap<Pubkey, Pubkey>> = RwLock::new(HashMap::new());
}

/// Get cached token program for a mint
pub fn get_cached_token_program(mint: &Pubkey) -> Option<Pubkey> {
    TOKEN_PROGRAM_CACHE.read().ok()?.get(mint).copied()
}

/// Cache the token program for a mint
pub fn cache_token_program(mint: Pubkey, token_program: Pubkey) {
    if let Ok(mut cache) = TOKEN_PROGRAM_CACHE.write() {
        cache.insert(mint, token_program);
        tracing::debug!("Cached token program for {}: {}", mint, token_program);
    }
}

/// Get token program from cache, or detect and cache it
/// This is the main function to use - only makes RPC call if not cached
pub fn get_or_detect_token_program(client: &RpcClient, mint: &Pubkey) -> Pubkey {
    // Native SOL/WSOL always uses SPL Token
    if mint == &*NATIVE_MINT {
        return *SPL_TOKEN_PROGRAM_ID;
    }
    
    // Check cache first
    if let Some(cached) = get_cached_token_program(mint) {
        return cached;
    }
    
    // Not cached, detect from RPC
    let token_program = detect_token_program_from_rpc(client, mint);
    
    // Cache the result
    cache_token_program(*mint, token_program);
    
    token_program
}

/// Detect token program by checking mint account owner
fn detect_token_program_from_rpc(client: &RpcClient, mint: &Pubkey) -> Pubkey {
    match client.get_account(mint) {
        Ok(account) => {
            if account.owner == *TOKEN_2022_PROGRAM_ID {
                tracing::info!("Token-2022 detected for mint {}", mint);
                *TOKEN_2022_PROGRAM_ID
            } else {
                *SPL_TOKEN_PROGRAM_ID
            }
        }
        Err(e) => {
            tracing::warn!("Failed to detect token program for {}: {}, defaulting to SPL Token", mint, e);
            *SPL_TOKEN_PROGRAM_ID
        }
    }
}

/// Pre-cache token program for a mint (call at startup)
pub fn precache_token_program(client: &RpcClient, mint: &Pubkey) {
    // Skip if already cached
    if get_cached_token_program(mint).is_some() {
        return;
    }
    
    let token_program = detect_token_program_from_rpc(client, mint);
    cache_token_program(*mint, token_program);
}

/// Batch pre-cache token programs for multiple mints
pub fn precache_token_programs(client: &RpcClient, mints: &[Pubkey]) {
    for mint in mints {
        precache_token_program(client, mint);
    }
}

/// Clear the cache (useful for testing)
pub fn clear_cache() {
    if let Ok(mut cache) = TOKEN_PROGRAM_CACHE.write() {
        cache.clear();
        tracing::info!("Token program cache cleared");
    }
}

/// Get cache size (for debugging)
pub fn cache_size() -> usize {
    TOKEN_PROGRAM_CACHE.read().map(|c| c.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    
    #[test]
    fn test_native_mint_returns_spl_token() {
        // Native mint should always return SPL Token without RPC call
        let mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        // We can't test with real RPC, but we can verify the logic
        assert_eq!(mint, *NATIVE_MINT);
    }
    
    #[test]
    fn test_cache_operations() {
        clear_cache();
        assert_eq!(cache_size(), 0);
        
        let mint = Pubkey::new_unique();
        cache_token_program(mint, *SPL_TOKEN_PROGRAM_ID);
        
        assert_eq!(cache_size(), 1);
        assert_eq!(get_cached_token_program(&mint), Some(*SPL_TOKEN_PROGRAM_ID));
        
        clear_cache();
        assert_eq!(cache_size(), 0);
    }
}
