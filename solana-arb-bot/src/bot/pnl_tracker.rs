//! P&L Tracker
//! Tracks profit/loss and triggers emergency stop on max loss

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Individual trade record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub timestamp: DateTime<Utc>,
    pub trade_type: String,
    pub buy_pool: String,
    pub sell_pool: String,
    pub buy_pool_type: String,
    pub sell_pool_type: String,
    pub token: String,
    pub amount_in_sol: f64,
    pub tokens_traded: u64,
    pub amount_out_sol: f64,
    pub profit_sol: f64,
    pub fees_sol: f64,
    pub tx_sig: String,
    pub status: String,
}

/// P&L Tracker with trade history
pub struct PnlTracker {
    max_loss_sol: f64,
    current_pnl: f64,
    total_fees: f64,
    trade_count: u32,
    recent_trades: VecDeque<TradeRecord>,
    log_path: String,
}

impl PnlTracker {
    pub fn new(max_loss_sol: f64, log_path: &str) -> Self {
        Self {
            max_loss_sol,
            current_pnl: 0.0,
            total_fees: 0.0,
            trade_count: 0,
            recent_trades: VecDeque::with_capacity(100),
            log_path: log_path.to_string(),
        }
    }
    
    /// Restore state from session
    pub fn restore_from_session(&mut self, pnl: f64, fees: f64, trade_count: u32) {
        self.current_pnl = pnl;
        self.total_fees = fees;
        self.trade_count = trade_count;
        tracing::info!(
            "📊 Restored P&L tracker: {:.6} SOL P&L, {} trades",
            pnl,
            trade_count
        );
    }
    
    /// Record a trade
    pub fn record_trade(&mut self, record: TradeRecord) {
        self.current_pnl += record.profit_sol;
        self.total_fees += record.fees_sol;
        self.trade_count += 1;
        
        // Log to file
        if let Err(e) = self.append_to_log(&record) {
            tracing::error!("Failed to write trade log: {}", e);
        }
        
        // Keep recent trades in memory
        if self.recent_trades.len() >= 100 {
            self.recent_trades.pop_front();
        }
        self.recent_trades.push_back(record);
    }
    
    /// Check if max loss has been reached
    pub fn is_max_loss_reached(&self) -> bool {
        self.current_pnl <= -self.max_loss_sol
    }
    
    /// Get current P&L
    pub fn current_pnl(&self) -> f64 {
        self.current_pnl
    }
    
    /// Get remaining loss allowance
    pub fn remaining_loss_allowance(&self) -> f64 {
        self.max_loss_sol + self.current_pnl
    }
    
    /// Get total fees paid
    pub fn total_fees(&self) -> f64 {
        self.total_fees
    }
    
    /// Get trade count
    pub fn trade_count(&self) -> u32 {
        self.trade_count
    }
    
    /// Get recent trades
    pub fn recent_trades(&self) -> &VecDeque<TradeRecord> {
        &self.recent_trades
    }
    
    /// Get last trade
    pub fn last_trade(&self) -> Option<&TradeRecord> {
        self.recent_trades.back()
    }
    
    /// Append trade record to JSONL log file
    fn append_to_log(&self, record: &TradeRecord) -> std::io::Result<()> {
        // Ensure directory exists
        if let Some(parent) = Path::new(&self.log_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        
        let json = serde_json::to_string(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        
        writeln!(file, "{}", json)?;
        
        Ok(())
    }
}
