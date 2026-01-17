# Solana Arbitrage Bot

A high-performance, production-ready arbitrage bot for Solana DEX pools. Detects and executes cross-pool arbitrage opportunities with sub-second latency using real-time gRPC/WebSocket data feeds.

## Table of Contents

- [Features](#features)
- [Supported DEXs](#supported-dexs)
- [Architecture](#architecture)
- [Requirements](#requirements)
- [Installation](#installation)
- [Configuration](#configuration)
- [Usage](#usage)
- [Transaction Modes](#transaction-modes)
- [Real-time Data Feeds](#real-time-data-feeds)
- [Address Lookup Tables](#address-lookup-tables)
- [Safety Features](#safety-features)
- [Troubleshooting](#troubleshooting)
- [Project Structure](#project-structure)

---

## Features

- **Multi-DEX Arbitrage**: Detects price discrepancies across 5 major Solana DEXs
- **Atomic Execution**: Buy and sell in a single transaction - no partial fills
- **Real-time Data**: Yellowstone gRPC or WebSocket for sub-100ms pool state updates
- **Dual Transaction Modes**: 
  - Astralane (MEV-protected, API key required)
  - Jito bundles (REST or gRPC, parallel endpoint sending)
- **Smart Retry Logic**: Exponential tip increase on failed transactions
- **Address Lookup Tables**: Auto-creates ALTs for large transactions (PumpSwap + DLMM)
- **Safety Controls**: Max loss limits, emergency sell, configurable slippage
- **Session Recovery**: Crash recovery with persistent session state
- **TUI Dashboard**: Real-time P&L tracking, trade history, pool status

---

## Supported DEXs

| DEX | Program ID | Notes |
|-----|------------|-------|
| **PumpSwap** | `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA` | Pump.fun AMM pools |
| **Meteora DLMM** | `LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo` | Dynamic liquidity market maker |
| **Meteora DAMM V2** | `cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG` | Dynamic AMM V2 |
| **Raydium CPMM** | `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C` | Constant product AMM |
| **Orca Whirlpool** | `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc` | Concentrated liquidity |

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Bot Controller                           │
│  - Main loop orchestration                                      │
│  - Session management                                           │
│  - Emergency handling                                           │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐    ┌───────────────┐    ┌───────────────┐
│  gRPC/WS      │    │  Arbitrage    │    │  Arbitrage    │
│  Subscriber   │───▶│  Detector     │───▶│  Executor     │
│  (Real-time)  │    │  (Profit Calc)│    │  (Tx Builder) │
└───────────────┘    └───────────────┘    └───────────────┘
        │                                         │
        ▼                                         ▼
┌───────────────┐                        ┌───────────────┐
│  Pool State   │                        │  Astralane /  │
│  Cache        │                        │  Jito Sender  │
└───────────────┘                        └───────────────┘
```

**Flow:**
1. gRPC/WebSocket receives real-time pool state updates
2. Detector calculates arbitrage opportunities across all pool pairs
3. If profit > min_profit_sol, executor builds atomic transaction
4. Transaction sent via Astralane (MEV-protected) or Jito (bundle)
5. P&L tracker records result, session state updated

---

## Requirements

- **Rust**: 1.75+ (for async traits)
- **OS**: Linux (Ubuntu 22.04+), macOS, or Windows (WSL2 recommended)
- **RPC**: Reliable Solana RPC endpoint (Helius, Triton, QuickNode)
- **Optional**: 
  - Astralane API key for MEV protection
  - Yellowstone gRPC endpoint for fastest updates
  - Jito UUID for authenticated bundle access

---

## Installation

### From Source

```bash
# Clone the repository
git clone <repository-url>
cd solana-arb-bot

# Build release binary
cargo build --release

# Binary location
./target/release/arb-bot --help
```

### Verify Installation

```bash
./target/release/arb-bot --version
```

---

## Configuration

### Quick Start

```bash
# Generate config template
./target/release/arb-bot init

# Edit config.toml with your settings
nano config.toml

# Validate your pools
./target/release/arb-bot validate-pools

# Start the bot
./target/release/arb-bot start
```

### Configuration File (config.toml)

```toml
# ═══════════════════════════════════════════════════════════════
# WALLET CONFIGURATION
# ═══════════════════════════════════════════════════════════════
[wallet]
# Option 1: Path to Solana keypair JSON file
keypair_path = "~/.config/solana/id.json"
# Option 2: Base58 encoded private key (use this OR keypair_path)
private_key = ""

# ═══════════════════════════════════════════════════════════════
# RPC CONFIGURATION
# ═══════════════════════════════════════════════════════════════
[rpc]
# Standard RPC endpoint for queries (NOT for transaction submission)
# Use a reliable provider: Helius, Triton, QuickNode, etc.
endpoint = "https://api.mainnet-beta.solana.com"
timeout_secs = 30

# ═══════════════════════════════════════════════════════════════
# POOL CONFIGURATION
# ═══════════════════════════════════════════════════════════════
[pools]
# 2-5 pool addresses for the same token pair
addresses = [
    "POOL_ADDRESS_1",  # e.g., PumpSwap pool
    "POOL_ADDRESS_2",  # e.g., DLMM pool
]
# The token you're arbitraging (must exist in all pools)
target_token = "TOKEN_MINT_ADDRESS"

# ═══════════════════════════════════════════════════════════════
# TRADING LIMITS
# ═══════════════════════════════════════════════════════════════
[limits]
max_trade_count = 1000        # Stop after N trades
max_buy_amount_sol = 0.5      # Max SOL per trade
max_loss_sol = 2.0            # Emergency stop if losses exceed this
min_profit_sol = 0.001        # Minimum profit to execute (after fees)

[sizing]
max_pool_liquidity_percent = 2.0  # Don't take >2% of pool liquidity

[slippage]
buy_slippage_bps = 100        # 1% slippage tolerance
sell_slippage_bps = 100
emergency_slippage_bps = 1000 # 10% for emergency sells

# ═══════════════════════════════════════════════════════════════
# FEES & RETRY
# ═══════════════════════════════════════════════════════════════
[fees]
base_priority_lamports = 10000
jito_tip_lamports = 10000     # Minimum 10000 (0.00001 SOL)

[retry]
max_retries = 3
tip_multiplier = 1.5          # Increase tip by 50% on retry
max_tip_lamports = 100000
retry_delay_ms = 200

# ═══════════════════════════════════════════════════════════════
# SAFETY
# ═══════════════════════════════════════════════════════════════
[safety]
emergency_drop_percent = 50.0 # Emergency sell if price drops 50%
check_interval_ms = 100       # How often to check for opportunities

# ═══════════════════════════════════════════════════════════════
# TRANSACTION MODE: "astralane" or "jito"
# ═══════════════════════════════════════════════════════════════
transaction_mode = "astralane"

# ═══════════════════════════════════════════════════════════════
# ASTRALANE CONFIGURATION (if transaction_mode = "astralane")
# ═══════════════════════════════════════════════════════════════
[astralane]
endpoint = "https://ams.gateway.astralane.io/iris"
api_key = "YOUR_ASTRALANE_API_KEY"
tip_wallets = [
    "astrazznxsGUhWShqgNtAdfrzP2G83DzcWVJDxwV9bF",
    "astra4uejePWneqNaJKuFFA8oonqCE1sqF6b45kDMZm",
    "astra9xWY93QyfG6yM8zwsKsRodscjQ2uU2HKNL5prk",
    "astraRVUuTHjpwEVvNBeQEgwYx9w9CFyfxjYoobCZhL",
    "astraEJ2fEj8Xmy6KLG7B3VfbKfsHXhHrNdCQx7iGJK",
    "astraubkDw81n4LuutzSQ8uzHCv4BhPVhfvTcYv8SKC",
    "astraZW5GLFefxNPAatceHhYjfA1ciq9gvfEg2S47xk",
    "astrawVNP4xDBKT7rAdxrLYiTSTdqtUr63fSMduivXK",
]
# Auto-generated ALT address (created by 'create-alt' command)
lookup_table = ""

# ═══════════════════════════════════════════════════════════════
# JITO CONFIGURATION (if transaction_mode = "jito")
# ═══════════════════════════════════════════════════════════════
[jito]
protocol = "rest"             # "rest" or "grpc"
uuid = ""                     # Optional: Jito UUID for authenticated access
auth_keypair_path = ""        # Optional: For gRPC authentication
tip_account = ""              # Optional: Custom tip account
parallel_send = true          # Send to all endpoints simultaneously

# All 9 Jito block engine endpoints
endpoints = [
    "https://mainnet.block-engine.jito.wtf",
    "https://amsterdam.mainnet.block-engine.jito.wtf",
    "https://frankfurt.mainnet.block-engine.jito.wtf",
    "https://ny.mainnet.block-engine.jito.wtf",
    "https://tokyo.mainnet.block-engine.jito.wtf",
    "https://slc.mainnet.block-engine.jito.wtf",
    "https://singapore.mainnet.block-engine.jito.wtf",
    "https://london.mainnet.block-engine.jito.wtf",
    "https://dublin.mainnet.block-engine.jito.wtf",
]

# ═══════════════════════════════════════════════════════════════
# REAL-TIME DATA (gRPC or WebSocket)
# ═══════════════════════════════════════════════════════════════
[grpc]
# Yellowstone gRPC endpoint (fastest option)
endpoint = ""
x_token = ""
reconnect_enabled = true
max_reconnect_attempts = 10

# WebSocket endpoint (alternative to gRPC)
ws_endpoint = ""

# ═══════════════════════════════════════════════════════════════
# LOGGING
# ═══════════════════════════════════════════════════════════════
[logging]
trade_log_path = "logs/trades.jsonl"
session_path = "logs/session.json"
log_level = "info"
```

---

## Usage

### CLI Commands

```bash
# Initialize new config file
arb-bot init [--output config.toml] [--force]

# Start the bot
arb-bot start [--config config.toml] [--fresh]

# Check bot status
arb-bot status

# View trade history
arb-bot history [--last 20]

# Validate pool addresses
arb-bot validate-pools [--config config.toml]

# Create Address Lookup Table
arb-bot create-alt [--config config.toml]

# Extend existing ALT
arb-bot extend-alt --alt <ALT_ADDRESS> [--config config.toml]

# Stop the bot (use Ctrl+C in running terminal)
arb-bot stop

# Emergency sell (triggers on Ctrl+C if holding tokens)
arb-bot emergency-sell
```

### Example Workflow

```bash
# 1. Initialize config
./target/release/arb-bot init

# 2. Edit config.toml with your pools and API keys
nano config.toml

# 3. Validate pools are correct
./target/release/arb-bot validate-pools

# 4. Create ALT if using PumpSwap + DLMM (large transactions)
./target/release/arb-bot create-alt

# 5. Start the bot
./target/release/arb-bot start

# 6. Monitor in another terminal
tail -f logs/trades.jsonl | jq .
```

---

## Transaction Modes

### Astralane (Recommended for MEV Protection)

Astralane provides MEV-protected transaction submission with high landing rates.

```toml
transaction_mode = "astralane"

[astralane]
endpoint = "https://ams.gateway.astralane.io/iris"
api_key = "YOUR_API_KEY"  # Get from https://astralane.io
```

**Regions:**
- `ams.gateway.astralane.io` - Amsterdam (EU)
- `fr.gateway.astralane.io` - Frankfurt (EU)
- `ny.gateway.astralane.io` - New York (US)

### Jito Bundles

Jito bundles ensure atomic execution with tip-based priority.

```toml
transaction_mode = "jito"

[jito]
protocol = "rest"        # or "grpc"
parallel_send = true     # Send to all 9 endpoints
uuid = ""                # Optional: authenticated access
```

**Protocol Options:**
- `rest`: Simple REST API, works with UUID authentication
- `grpc`: Official Jito SDK protocol, supports keypair authentication

**Parallel Send:** When enabled, sends bundles to all 9 Jito endpoints simultaneously for maximum landing rate. Recommended if you don't have a UUID.

---

## Real-time Data Feeds

### Yellowstone gRPC (Fastest)

Sub-100ms pool state updates via Triton, Chainstack, or Helius.

```toml
[grpc]
endpoint = "https://ams17.rpcpool.com:443"
x_token = "YOUR_TOKEN"
```

**Providers:**
- Triton: `https://ams17.rpcpool.com:443`
- Chainstack: `https://solana-mainnet.core.chainstack.com:443`
- Helius: `https://mainnet.helius-rpc.com:443`

### WebSocket (Alternative)

Uses `accountSubscribe` with processed commitment.

```toml
[grpc]
endpoint = ""  # Leave empty to use WebSocket
ws_endpoint = "wss://your-rpc-provider.com"
```

### RPC Polling (Fallback)

If both gRPC and WebSocket are empty, falls back to RPC polling at `check_interval_ms`.

---

## Address Lookup Tables

Large transactions (especially PumpSwap + DLMM) can exceed Solana's 1232-byte limit. ALTs compress account addresses from 32 bytes to 1 byte.

### Auto-Creation

The bot automatically creates an ALT on startup if needed:

```
⚠️ Some pool pairs may exceed Solana's 1232 byte transaction limit:
   PumpSwap + DLMM ≈ 1450 bytes (limit: 1232)
📋 No ALT configured. Creating one automatically...
✅ ALT created and saved to config: <ALT_ADDRESS>
```

### Manual Creation

```bash
# Create new ALT
arb-bot create-alt

# Extend existing ALT with more addresses
arb-bot extend-alt --alt <ALT_ADDRESS>
```

**Important:** Do NOT add Astralane tip wallets to ALT. Astralane needs to see the full 32-byte address to detect tips.

---

## Safety Features

### Max Loss Protection

```toml
[limits]
max_loss_sol = 2.0  # Stop if cumulative losses exceed 2 SOL
```

### Emergency Sell

Triggers automatically when:
- Price drops > `emergency_drop_percent` from baseline
- Max loss exceeded
- Ctrl+C pressed while holding tokens

```toml
[safety]
emergency_drop_percent = 50.0
```

### Slippage Protection

```toml
[slippage]
buy_slippage_bps = 100        # 1%
sell_slippage_bps = 100       # 1%
emergency_slippage_bps = 1000 # 10% for emergency
```

### Session Recovery

Session state is persisted to `logs/session.json`. On restart, the bot resumes from the last known state unless `--fresh` is specified.

---

## Troubleshooting

### Transaction Too Large

```
Error: Transaction too large (1450 bytes > 1232 limit)
```

**Solution:** Create an ALT:
```bash
arb-bot create-alt
```

### Astralane Tip Error

```
Error: transaction tip [0] is less than min tip [10000]
```

**Cause:** Tip wallet is in ALT (compressed to 1-byte index).

**Solution:** Remove tip wallets from ALT. The bot handles this automatically.

### gRPC Connection Failed

```
Error: Failed to connect to gRPC
```

**Solutions:**
1. Check endpoint URL includes port (`:443`)
2. Verify x_token is correct
3. Try WebSocket as fallback

### Pool Validation Failed

```
Error: Pool XXXX is not a supported pool type
```

**Solution:** Ensure pool addresses are for supported DEXs (PumpSwap, DLMM, DAMM V2, CPMM, Whirlpool).

### Rate Limited (Jito)

```
Error: 429 Too Many Requests
```

**Solutions:**
1. Get a Jito UUID from https://web.jito.wtf/
2. Enable `parallel_send = true` to distribute across endpoints
3. Increase `retry_delay_ms`

---

## Project Structure

```
solana-arb-bot/
├── src/
│   ├── main.rs              # Entry point
│   ├── arbitrage/           # Core arbitrage logic
│   │   ├── calculator.rs    # Profit calculation
│   │   ├── detector.rs      # Opportunity detection
│   │   ├── executor.rs      # Transaction building & sending
│   │   └── jito_sender.rs   # Jito bundle sender
│   ├── bot/                 # Bot orchestration
│   │   ├── controller.rs    # Main loop
│   │   ├── emergency.rs     # Emergency sell handler
│   │   ├── pnl_tracker.rs   # P&L tracking
│   │   └── session.rs       # Session state persistence
│   ├── cli/                 # CLI interface
│   │   ├── commands.rs      # Command implementations
│   │   └── display.rs       # TUI dashboard
│   ├── config/              # Configuration
│   │   └── settings.rs      # Config parsing
│   ├── grpc/                # Real-time data
│   │   ├── subscriber.rs    # Yellowstone gRPC
│   │   ├── vault_subscriber.rs  # Vault account updates
│   │   └── ws_subscriber.rs # WebSocket fallback
│   ├── jito_grpc/           # Jito gRPC client
│   │   └── client.rs        # Bundle submission
│   ├── pools/               # DEX adapters
│   │   ├── pumpswap.rs      # PumpSwap
│   │   ├── dlmm.rs          # Meteora DLMM
│   │   ├── damm_v2.rs       # Meteora DAMM V2
│   │   ├── cpmm.rs          # Raydium CPMM
│   │   └── whirlpool.rs     # Orca Whirlpool
│   └── token/               # Token utilities
│       └── program_cache.rs # Token program detection
├── config.example.toml      # Example configuration
├── Cargo.toml               # Dependencies
└── README.md                # This file
```

---

## Performance Tips

1. **Use gRPC**: Yellowstone gRPC provides the fastest pool updates (~50-100ms vs 500ms+ for RPC)

2. **Parallel Jito Send**: Enable `parallel_send = true` for better landing rates

3. **Optimize Tip**: Start with 10000 lamports, increase if landing rate is low

4. **Reduce Pools**: More pools = more computation. 2-3 pools is optimal

5. **Use ALT**: Always create an ALT for PumpSwap + DLMM combinations

6. **Regional Endpoints**: Use endpoints closest to your server location

---

## License

MIT

---

## Disclaimer

This software is for educational purposes only. Trading cryptocurrencies involves significant risk. Use at your own risk. The authors are not responsible for any financial losses.
