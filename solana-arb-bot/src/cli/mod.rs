mod commands;
pub mod display;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "arb-bot")]
#[command(about = "Solana arbitrage bot for PumpSwap, Meteora DLMM/DAMM V2, Raydium CPMM, and Orca Whirlpool pools")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a new config file
    Init {
        /// Output path for config file
        #[arg(short, long, default_value = "config.toml")]
        output: String,
        
        /// Overwrite existing config file
        #[arg(long)]
        force: bool,
    },
    
    /// Start the arbitrage bot
    Start {
        /// Path to config file
        #[arg(short, long, default_value = "config.toml")]
        config: String,
        
        /// Start fresh session (ignore previous state)
        #[arg(long)]
        fresh: bool,
    },
    
    /// Check bot status
    Status,
    
    /// Trigger emergency sell
    EmergencySell,
    
    /// Stop the bot gracefully
    Stop,
    
    /// View trade history
    History {
        /// Number of recent trades to show
        #[arg(short, long, default_value = "20")]
        last: usize,
    },
    
    /// Validate pool addresses and detect types
    ValidatePools {
        /// Path to config file
        #[arg(short, long, default_value = "config.toml")]
        config: String,
    },
    
    /// Create an Address Lookup Table (ALT) for transaction size optimization
    /// Required for PumpSwap + DLMM arbitrage
    CreateAlt {
        /// Path to config file
        #[arg(short, long, default_value = "config.toml")]
        config: String,
    },
    
    /// Extend an existing ALT with more addresses
    ExtendAlt {
        /// Path to config file
        #[arg(short, long, default_value = "config.toml")]
        config: String,
        
        /// ALT address to extend
        #[arg(short, long)]
        alt: String,
    },
}

pub async fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init { output, force } => commands::init(&output, force).await,
        Commands::Start { config, fresh } => commands::start(&config, fresh).await,
        Commands::Status => commands::status().await,
        Commands::EmergencySell => commands::emergency_sell().await,
        Commands::Stop => commands::stop().await,
        Commands::History { last } => commands::history(last).await,
        Commands::ValidatePools { config } => commands::validate_pools(&config).await,
        Commands::CreateAlt { config } => commands::create_alt(&config).await,
        Commands::ExtendAlt { config, alt } => commands::extend_alt(&config, &alt).await,
    }
}
