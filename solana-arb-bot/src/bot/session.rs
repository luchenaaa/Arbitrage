//! Session State Persistence
//! Saves and restores bot state for crash recovery

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Persistent session state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub trade_count: u32,
    pub total_pnl_sol: f64,
    pub total_fees_sol: f64,
    pub wins: u32,
    pub losses: u32,
    pub last_updated: DateTime<Utc>,
}

impl SessionState {
    const DEFAULT_PATH: &'static str = "logs/session.json";
    
    /// Load existing session or create new one
    pub fn load_or_create(path: Option<&str>) -> Self {
        let path = path.unwrap_or(Self::DEFAULT_PATH);
        
        if let Ok(data) = std::fs::read_to_string(path) {
            if let Ok(state) = serde_json::from_str::<SessionState>(&data) {
                tracing::info!(
                    "📂 Resuming session: {} trades, {:.6} SOL P&L",
                    state.trade_count,
                    state.total_pnl_sol
                );
                return state;
            }
        }
        
        tracing::info!("📂 Starting new session");
        Self::new()
    }
    
    /// Create a fresh session
    pub fn new() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            started_at: Utc::now(),
            trade_count: 0,
            total_pnl_sol: 0.0,
            total_fees_sol: 0.0,
            wins: 0,
            losses: 0,
            last_updated: Utc::now(),
        }
    }
    
    /// Save session to disk
    pub fn save(&self, path: Option<&str>) -> Result<()> {
        let path = path.unwrap_or(Self::DEFAULT_PATH);
        
        // Ensure directory exists
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        
        Ok(())
    }
    
    /// Record a completed trade
    pub fn record_trade(&mut self, profit_sol: f64, fees_sol: f64, path: Option<&str>) {
        self.trade_count += 1;
        self.total_pnl_sol += profit_sol;
        self.total_fees_sol += fees_sol;
        
        if profit_sol > 0.0 {
            self.wins += 1;
        } else {
            self.losses += 1;
        }
        
        self.last_updated = Utc::now();
        
        // Save after each trade for crash recovery
        if let Err(e) = self.save(path) {
            tracing::error!("Failed to save session state: {}", e);
        }
    }
    
    /// Calculate win rate percentage
    pub fn win_rate(&self) -> f64 {
        if self.trade_count == 0 {
            0.0
        } else {
            (self.wins as f64 / self.trade_count as f64) * 100.0
        }
    }
    
    /// Reset session (start fresh)
    pub fn reset(&mut self, path: Option<&str>) {
        *self = Self::new();
        let _ = self.save(path);
    }
    
    /// Get session duration
    pub fn duration(&self) -> chrono::Duration {
        Utc::now() - self.started_at
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}
