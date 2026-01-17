//! Simple Status Printer
//! Prints bot status to console without TUI (simpler, works in all terminals)

use std::time::Instant;

use crate::pools::PoolState;

/// Print bot startup banner
pub fn print_startup_banner(
    target_token: &str,
    pool_count: usize,
    wallet: &str,
    balance_sol: f64,
) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║              SOLANA ARBITRAGE BOT - STARTING                     ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  Token: {}...                                    ║", &target_token[..8.min(target_token.len())]);
    println!("║  Pools: {} active                                              ║", pool_count);
    println!("║  Wallet: {}...                                   ║", &wallet[..8.min(wallet.len())]);
    println!("║  Balance: {:.4} SOL                                          ║", balance_sol);
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();
}

/// Print pool prices
pub fn print_pool_prices(pools: &[PoolState], spread: f64) {
    println!("┌─────────────────────────────────────────────────────────────────┐");
    println!("│ POOL PRICES                                                     │");
    println!("├─────────────────────────────────────────────────────────────────┤");
    
    for (i, pool) in pools.iter().enumerate() {
        let liq_bar = create_liquidity_bar(pool.liquidity_sol);
        println!(
            "│ Pool {} ({:?}): {:.10} SOL  {}  │",
            i + 1,
            pool.pool_type,
            pool.price,
            liq_bar
        );
    }
    
    let spread_indicator = if spread > 1.0 { "🟢" } else if spread > 0.5 { "🟡" } else { "🔴" };
    println!("│ Spread: {:.2}% {}                                              │", spread, spread_indicator);
    println!("└─────────────────────────────────────────────────────────────────┘");
}

/// Print session stats
pub fn print_session_stats(
    trade_count: u32,
    max_trades: u32,
    pnl: f64,
    win_rate: f64,
    fees: f64,
    remaining_loss: f64,
) {
    let pnl_sign = if pnl >= 0.0 { "+" } else { "" };
    let pnl_color = if pnl >= 0.0 { "🟢" } else { "🔴" };
    
    println!("┌─────────────────────────────────────────────────────────────────┐");
    println!("│ SESSION STATS                                                   │");
    println!("├─────────────────────────────────────────────────────────────────┤");
    println!("│ Trades: {} / {}                                                │", trade_count, max_trades);
    println!("│ P&L: {}{:.6} SOL {}                                        │", pnl_sign, pnl, pnl_color);
    println!("│ Win Rate: {:.1}%                                               │", win_rate);
    println!("│ Fees Paid: {:.6} SOL                                        │", fees);
    println!("│ Max Loss Remaining: {:.4} SOL                                 │", remaining_loss);
    println!("└─────────────────────────────────────────────────────────────────┘");
}

/// Print trade execution
pub fn print_trade_execution(
    buy_pool_type: &str,
    sell_pool_type: &str,
    sol_in: f64,
    tokens: u64,
    sol_out: f64,
    profit: f64,
    fees: f64,
    signature: &str,
) {
    let profit_sign = if profit >= 0.0 { "+" } else { "" };
    let status = if profit >= 0.0 { "✅" } else { "❌" };
    
    println!();
    println!("┌─────────────────────────────────────────────────────────────────┐");
    println!("│ {} TRADE EXECUTED                                              │", status);
    println!("├─────────────────────────────────────────────────────────────────┤");
    println!("│ Buy: {:.4} SOL → {} tokens ({})                    │", sol_in, tokens, buy_pool_type);
    println!("│ Sell: {} tokens → {:.4} SOL ({})                   │", tokens, sol_out, sell_pool_type);
    println!("│ Profit: {}{:.6} SOL (fees: {:.6} SOL)                     │", profit_sign, profit, fees);
    println!("│ Tx: {}...                                          │", &signature[..16.min(signature.len())]);
    println!("└─────────────────────────────────────────────────────────────────┘");
    println!();
}

/// Print opportunity found
pub fn print_opportunity_found(spread: f64, expected_profit: f64, buy_pool: &str, sell_pool: &str) {
    println!(
        "💡 Opportunity: {:.2}% spread, ~{:.6} SOL profit | Buy: {}... → Sell: {}...",
        spread,
        expected_profit,
        &buy_pool[..8.min(buy_pool.len())],
        &sell_pool[..8.min(sell_pool.len())]
    );
}

/// Print no opportunity
pub fn print_no_opportunity() {
    print!(".");
    use std::io::Write;
    std::io::stdout().flush().ok();
}

/// Print emergency warning
pub fn print_emergency_warning(reason: &str) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║ 🚨🚨🚨 EMERGENCY TRIGGERED 🚨🚨🚨                                ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║ Reason: {}                                          ║", reason);
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();
}

/// Print bot stopped
pub fn print_bot_stopped(reason: &str) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║ 🛑 BOT STOPPED                                                   ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║ Reason: {}                                          ║", reason);
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();
}

/// Create a simple ASCII liquidity bar
fn create_liquidity_bar(liquidity_sol: f64) -> String {
    let percent = (liquidity_sol / 10.0 * 100.0).min(100.0); // Assume 10 SOL = 100%
    let filled = (percent / 10.0) as usize;
    let empty = 10 - filled;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

/// Status update interval tracker
pub struct StatusPrinter {
    last_full_update: Instant,
    update_interval_secs: u64,
    dot_count: u32,
}

impl StatusPrinter {
    pub fn new(update_interval_secs: u64) -> Self {
        Self {
            last_full_update: Instant::now(),
            update_interval_secs,
            dot_count: 0,
        }
    }
    
    /// Check if it's time for a full status update
    pub fn should_print_full_status(&mut self) -> bool {
        if self.last_full_update.elapsed().as_secs() >= self.update_interval_secs {
            self.last_full_update = Instant::now();
            self.dot_count = 0;
            println!(); // New line after dots
            true
        } else {
            false
        }
    }
    
    /// Print a dot for activity indication
    pub fn print_activity_dot(&mut self) {
        self.dot_count += 1;
        if self.dot_count % 60 == 0 {
            println!(); // New line every 60 dots
        }
        print_no_opportunity();
    }
}
