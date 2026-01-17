use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Transaction submission mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TransactionMode {
    #[default]
    Astralane,
    Jito,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub wallet: WalletConfig,
    pub rpc: RpcConfig,
    pub pools: PoolsConfig,
    pub limits: LimitsConfig,
    pub sizing: SizingConfig,
    pub slippage: SlippageConfig,
    pub fees: FeesConfig,
    pub retry: RetryConfig,
    pub safety: SafetyConfig,
    /// Transaction submission mode: "astralane" or "jito"
    #[serde(default)]
    pub transaction_mode: TransactionMode,
    pub astralane: AstralaneConfig,
    /// Jito bundle configuration (optional, uses defaults if not specified)
    #[serde(default)]
    pub jito: JitoConfig,
    pub grpc: GrpcConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletConfig {
    /// Path to keypair JSON file (optional if private_key is set)
    #[serde(default)]
    pub keypair_path: String,
    /// Base58 encoded private key (optional if keypair_path is set)
    #[serde(default)]
    pub private_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolsConfig {
    /// Up to 5 pool addresses
    pub addresses: Vec<String>,
    /// Target token mint address
    pub target_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    pub max_trade_count: u32,
    pub max_buy_amount_sol: f64,
    pub max_loss_sol: f64,
    pub min_profit_sol: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizingConfig {
    pub max_pool_liquidity_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlippageConfig {
    pub buy_slippage_bps: u64,
    pub sell_slippage_bps: u64,
    pub emergency_slippage_bps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeesConfig {
    pub base_priority_lamports: u64,
    pub jito_tip_lamports: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub tip_multiplier: f64,
    pub max_tip_lamports: u64,
    pub retry_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    pub emergency_drop_percent: f64,
    pub check_interval_ms: u64,
    /// Cooldown after each trade in milliseconds
    /// Prevents double-spending by waiting for tx confirmation
    #[serde(default = "default_trade_cooldown_ms")]
    pub trade_cooldown_ms: u64,
}

fn default_trade_cooldown_ms() -> u64 {
    3000 // 3 seconds default
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstralaneConfig {
    pub endpoint: String,
    pub api_key: String,
    /// Tip wallets - can be a single address or multiple (will randomly select one)
    /// If empty, uses default Astralane tip wallets
    #[serde(default = "default_tip_wallets")]
    pub tip_wallets: Vec<String>,
    /// Address Lookup Table (ALT) address for transaction size optimization
    /// Required for PumpSwap + DLMM arbitrage (too many accounts without ALT)
    /// Create with: arb-bot create-alt
    #[serde(default)]
    pub lookup_table: String,
}

fn default_tip_wallets() -> Vec<String> {
    vec![
        "astrazznxsGUhWShqgNtAdfrzP2G83DzcWVJDxwV9bF".to_string(),
        "astra4uejePWneqNaJKuFFA8oonqCE1sqF6b45kDMZm".to_string(),
        "astra9xWY93QyfG6yM8zwsKsRodscjQ2uU2HKNL5prk".to_string(),
        "astraRVUuTHjpwEVvNBeQEgwYx9w9CFyfxjYoobCZhL".to_string(),
        "astraEJ2fEj8Xmy6KLG7B3VfbKfsHXhHrNdCQx7iGJK".to_string(),
        "astraubkDw81n4LuutzSQ8uzHCv4BhPVhfvTcYv8SKC".to_string(),
        "astraZW5GLFefxNPAatceHhYjfA1ciq9gvfEg2S47xk".to_string(),
        "astrawVNP4xDBKT7rAdxrLYiTSTdqtUr63fSMduivXK".to_string(),
    ]
}

/// Jito protocol type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum JitoProtocol {
    /// REST API (simpler, no auth keypair needed)
    #[default]
    Rest,
    /// gRPC (official SDK protocol, supports auth keypair)
    Grpc,
}

/// Jito bundle configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitoConfig {
    /// Protocol to use: "rest" or "grpc"
    /// - rest: Uses REST API (simpler, works with UUID)
    /// - grpc: Uses gRPC (official SDK, supports auth keypair)
    #[serde(default)]
    pub protocol: JitoProtocol,
    /// Jito UUID for authenticated access (optional, avoids rate limits)
    /// Get one from: https://web.jito.wtf/
    /// Only used with REST protocol
    #[serde(default)]
    pub uuid: String,
    /// Path to auth keypair for gRPC authentication (optional)
    /// Only used with gRPC protocol
    #[serde(default)]
    pub auth_keypair_path: String,
    /// Tip account (randomly selected from Jito's 8 tip accounts if empty)
    #[serde(default)]
    pub tip_account: String,
    /// Send to all Jito endpoints in parallel (recommended if no UUID/auth)
    #[serde(default = "default_parallel_send")]
    pub parallel_send: bool,
    /// Jito block engine endpoints (uses all 9 mainnet endpoints by default)
    #[serde(default = "default_jito_endpoints")]
    pub endpoints: Vec<String>,
}

fn default_parallel_send() -> bool {
    true
}

fn default_jito_endpoints() -> Vec<String> {
    vec![
        "https://mainnet.block-engine.jito.wtf".to_string(),
        "https://amsterdam.mainnet.block-engine.jito.wtf".to_string(),
        "https://frankfurt.mainnet.block-engine.jito.wtf".to_string(),
        "https://ny.mainnet.block-engine.jito.wtf".to_string(),
        "https://tokyo.mainnet.block-engine.jito.wtf".to_string(),
        "https://slc.mainnet.block-engine.jito.wtf".to_string(),
        "https://singapore.mainnet.block-engine.jito.wtf".to_string(),
        "https://london.mainnet.block-engine.jito.wtf".to_string(),
        "https://dublin.mainnet.block-engine.jito.wtf".to_string(),
    ]
}

fn default_jito_tip_accounts() -> Vec<String> {
    vec![
        "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5".to_string(),
        "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe".to_string(),
        "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY".to_string(),
        "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49".to_string(),
        "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh".to_string(),
        "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt".to_string(),
        "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL".to_string(),
        "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT".to_string(),
    ]
}

impl Default for JitoConfig {
    fn default() -> Self {
        Self {
            protocol: JitoProtocol::Rest,
            uuid: String::new(),
            auth_keypair_path: String::new(),
            tip_account: String::new(),
            parallel_send: true,
            endpoints: default_jito_endpoints(),
        }
    }
}

impl JitoConfig {
    /// Get a random tip account from Jito's official list
    pub fn get_random_tip_account(&self) -> String {
        if !self.tip_account.is_empty() {
            return self.tip_account.clone();
        }
        let accounts = default_jito_tip_accounts();
        let idx = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as usize % accounts.len();
        accounts[idx].clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcConfig {
    /// Standard RPC endpoint for queries (get_account, get_balance, etc.)
    pub endpoint: String,
    /// Request timeout in seconds
    #[serde(default = "default_rpc_timeout")]
    pub timeout_secs: u64,
}

fn default_rpc_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcConfig {
    pub endpoint: String,
    pub x_token: String,
    #[serde(default = "default_reconnect_enabled")]
    pub reconnect_enabled: bool,
    #[serde(default = "default_max_reconnect_attempts")]
    pub max_reconnect_attempts: u32,
    /// WebSocket endpoint (alternative to gRPC)
    #[serde(default)]
    pub ws_endpoint: String,
}

fn default_reconnect_enabled() -> bool {
    true
}

fn default_max_reconnect_attempts() -> u32 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub trade_log_path: String,
    pub session_path: String,
    pub log_level: String,
}

impl Settings {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .map_err(|e| anyhow!("Failed to read config file: {}", e))?;
        
        let settings: Settings = toml::from_str(&content)
            .map_err(|e| anyhow!("Failed to parse config file: {}", e))?;
        
        settings.validate()?;
        
        Ok(settings)
    }
    
    fn validate(&self) -> Result<()> {
        use solana_sdk::pubkey::Pubkey;
        use std::str::FromStr;
        
        // Validate pool count (need at least 2 for arbitrage)
        if self.pools.addresses.len() < 2 {
            return Err(anyhow!("At least 2 pool addresses are required for arbitrage"));
        }
        if self.pools.addresses.len() > 5 {
            return Err(anyhow!("Maximum 5 pool addresses allowed"));
        }
        
        // Validate pool addresses are valid Pubkeys
        for (i, addr) in self.pools.addresses.iter().enumerate() {
            if Pubkey::from_str(addr).is_err() {
                return Err(anyhow!("Invalid pool address at index {}: {}", i, addr));
            }
        }
        
        // Validate target token
        if self.pools.target_token.is_empty() {
            return Err(anyhow!("Target token address is required"));
        }
        if Pubkey::from_str(&self.pools.target_token).is_err() {
            return Err(anyhow!("Invalid target token address: {}", self.pools.target_token));
        }
        
        // Validate limits
        if self.limits.max_buy_amount_sol <= 0.0 {
            return Err(anyhow!("max_buy_amount_sol must be positive"));
        }
        if self.limits.max_loss_sol <= 0.0 {
            return Err(anyhow!("max_loss_sol must be positive"));
        }
        if self.limits.min_profit_sol < 0.0 {
            return Err(anyhow!("min_profit_sol cannot be negative"));
        }
        
        // Validate slippage
        if self.slippage.buy_slippage_bps > 10000 {
            return Err(anyhow!("buy_slippage_bps cannot exceed 10000 (100%)"));
        }
        if self.slippage.sell_slippage_bps > 10000 {
            return Err(anyhow!("sell_slippage_bps cannot exceed 10000 (100%)"));
        }
        if self.slippage.emergency_slippage_bps > 10000 {
            return Err(anyhow!("emergency_slippage_bps cannot exceed 10000 (100%)"));
        }
        
        // Validate sizing
        if self.sizing.max_pool_liquidity_percent <= 0.0 || self.sizing.max_pool_liquidity_percent > 100.0 {
            return Err(anyhow!("max_pool_liquidity_percent must be between 0 and 100"));
        }
        
        // Validate Astralane config (required only if using astralane mode)
        if self.transaction_mode == TransactionMode::Astralane {
            if self.astralane.endpoint.is_empty() {
                return Err(anyhow!("Astralane endpoint is required when using astralane mode"));
            }
            if self.astralane.api_key.is_empty() {
                return Err(anyhow!("Astralane API key is required when using astralane mode"));
            }
            if self.astralane.tip_wallets.is_empty() {
                return Err(anyhow!("At least one Astralane tip wallet is required"));
            }
            for (i, tip_wallet) in self.astralane.tip_wallets.iter().enumerate() {
                if Pubkey::from_str(tip_wallet).is_err() {
                    return Err(anyhow!("Invalid Astralane tip wallet address at index {}: {}", i, tip_wallet));
                }
            }
        }
        
        // Validate Jito config (required only if using jito mode)
        if self.transaction_mode == TransactionMode::Jito {
            if self.jito.endpoints.is_empty() {
                return Err(anyhow!("At least one Jito endpoint is required when using jito mode"));
            }
            // Validate tip account if provided
            if !self.jito.tip_account.is_empty() {
                if Pubkey::from_str(&self.jito.tip_account).is_err() {
                    return Err(anyhow!("Invalid Jito tip account address: {}", self.jito.tip_account));
                }
            }
        }
        
        // Validate RPC endpoint (required for queries)
        if self.rpc.endpoint.is_empty() {
            return Err(anyhow!("RPC endpoint is required"));
        }
        
        // Validate wallet config (either keypair_path or private_key must be set)
        let has_keypair_path = !self.wallet.keypair_path.is_empty();
        let has_private_key = !self.wallet.private_key.is_empty();
        
        if !has_keypair_path && !has_private_key {
            return Err(anyhow!("Either wallet.keypair_path or wallet.private_key must be set"));
        }
        
        if has_keypair_path {
            let keypair_path = shellexpand::tilde(&self.wallet.keypair_path).to_string();
            if !std::path::Path::new(&keypair_path).exists() {
                return Err(anyhow!("Wallet keypair file not found: {}", self.wallet.keypair_path));
            }
        }
        
        if has_private_key {
            // Validate base58 format
            if bs58::decode(&self.wallet.private_key).into_vec().is_err() {
                return Err(anyhow!("Invalid base58 private key format"));
            }
        }
        
        // Validate retry config
        if self.retry.tip_multiplier < 1.0 {
            return Err(anyhow!("tip_multiplier must be >= 1.0"));
        }
        if self.retry.max_tip_lamports < self.fees.jito_tip_lamports {
            return Err(anyhow!("max_tip_lamports must be >= jito_tip_lamports"));
        }
        
        // Validate safety config
        if self.safety.emergency_drop_percent <= 0.0 || self.safety.emergency_drop_percent > 100.0 {
            return Err(anyhow!("emergency_drop_percent must be between 0 and 100"));
        }
        
        Ok(())
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            wallet: WalletConfig {
                keypair_path: "~/.config/solana/id.json".to_string(),
                private_key: String::new(),
            },
            rpc: RpcConfig {
                endpoint: "https://api.mainnet-beta.solana.com".to_string(),
                timeout_secs: 30,
            },
            pools: PoolsConfig {
                addresses: vec![],
                target_token: String::new(),
            },
            limits: LimitsConfig {
                max_trade_count: 1000,
                max_buy_amount_sol: 0.5,
                max_loss_sol: 2.0,
                min_profit_sol: 0.001,
            },
            sizing: SizingConfig {
                max_pool_liquidity_percent: 2.0,
            },
            slippage: SlippageConfig {
                buy_slippage_bps: 100,
                sell_slippage_bps: 100,
                emergency_slippage_bps: 1000,
            },
            fees: FeesConfig {
                base_priority_lamports: 10000,
                jito_tip_lamports: 10000,
            },
            retry: RetryConfig {
                max_retries: 3,
                tip_multiplier: 1.5,
                max_tip_lamports: 100000,
                retry_delay_ms: 200,
            },
            safety: SafetyConfig {
                emergency_drop_percent: 50.0,
                check_interval_ms: 100,
                trade_cooldown_ms: 3000,
            },
            transaction_mode: TransactionMode::Astralane,
            astralane: AstralaneConfig {
                endpoint: "https://ams.gateway.astralane.io/iris".to_string(),
                api_key: String::new(),
                tip_wallets: default_tip_wallets(),
                lookup_table: String::new(),
            },
            jito: JitoConfig::default(),
            grpc: GrpcConfig {
                endpoint: String::new(),
                x_token: String::new(),
                reconnect_enabled: true,
                max_reconnect_attempts: 10,
                ws_endpoint: String::new(),
            },
            logging: LoggingConfig {
                trade_log_path: "logs/trades.jsonl".to_string(),
                session_path: "logs/session.json".to_string(),
                log_level: "info".to_string(),
            },
        }
    }
}
