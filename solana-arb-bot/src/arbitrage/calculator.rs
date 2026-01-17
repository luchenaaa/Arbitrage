//! Profit Calculator
//! Calculates expected profit including all fees (priority, tip, network)

use crate::config::Settings;

/// Result of profit calculation
#[derive(Debug, Clone)]
pub struct ProfitResult {
    /// Tokens bought in "whole token" units (NOT raw lamports)
    /// To convert to raw amount: tokens_bought * 10^decimals
    pub tokens_bought: f64,
    /// SOL received from selling tokens
    pub sol_received: f64,
    /// Gross profit before fees (sol_received - sol_spent)
    pub gross_profit_sol: f64,
    /// Total transaction fees (priority + tip + base)
    pub total_fees_sol: f64,
    /// Net profit after fees
    pub net_profit_sol: f64,
    /// Whether this opportunity meets minimum profit threshold
    pub is_profitable: bool,
    /// Price spread percentage between buy and sell pools
    pub spread_percent: f64,
    /// Return on investment percentage
    pub roi_percent: f64,
}

/// Profit calculator with configurable thresholds
pub struct ProfitCalculator {
    pub min_profit_sol: f64,
    pub priority_fee_lamports: u64,
    pub tip_lamports: u64,
}

impl ProfitCalculator {
    pub fn new(settings: &Settings) -> Self {
        Self {
            min_profit_sol: settings.limits.min_profit_sol,
            priority_fee_lamports: settings.fees.base_priority_lamports,
            tip_lamports: settings.fees.jito_tip_lamports,
        }
    }
    
    /// Calculate expected profit from an arbitrage opportunity
    pub fn calculate(
        &self,
        buy_price: f64,           // SOL per token on buy pool
        sell_price: f64,          // SOL per token on sell pool
        sol_amount: f64,          // SOL to spend
        buy_slippage: f64,        // e.g., 0.01 for 1%
        sell_slippage: f64,
    ) -> ProfitResult {
        // Tokens bought (after slippage)
        let tokens_bought = if buy_price > 0.0 {
            (sol_amount / buy_price) * (1.0 - buy_slippage)
        } else {
            0.0
        };
        
        // SOL received from sell (after slippage)
        let sol_received = tokens_bought * sell_price * (1.0 - sell_slippage);
        
        // Total fees in SOL
        // Priority fee + tip + base tx fee (~5000 lamports)
        let total_fees_sol = (self.priority_fee_lamports + self.tip_lamports + 5000) as f64 / 1e9;
        
        // Gross and net profit
        let gross_profit_sol = sol_received - sol_amount;
        let net_profit_sol = gross_profit_sol - total_fees_sol;
        
        // Spread percentage
        let spread_percent = if buy_price > 0.0 {
            ((sell_price - buy_price) / buy_price) * 100.0
        } else {
            0.0
        };
        
        // ROI percentage
        let roi_percent = if sol_amount > 0.0 {
            (net_profit_sol / sol_amount) * 100.0
        } else {
            0.0
        };
        
        ProfitResult {
            tokens_bought,
            sol_received,
            gross_profit_sol,
            total_fees_sol,
            net_profit_sol,
            is_profitable: net_profit_sol >= self.min_profit_sol,
            spread_percent,
            roi_percent,
        }
    }
    
    /// Quick check if spread is worth investigating
    pub fn is_spread_profitable(&self, buy_price: f64, sell_price: f64) -> bool {
        if buy_price <= 0.0 || sell_price <= 0.0 {
            return false;
        }
        
        let spread = (sell_price - buy_price) / buy_price;
        // Need at least 0.5% spread to cover fees typically
        spread > 0.005
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_profit_calculation() {
        let calc = ProfitCalculator {
            min_profit_sol: 0.001,
            priority_fee_lamports: 10000,
            tip_lamports: 10000,
        };
        
        // 2% spread scenario
        let buy_price = 0.0001;   // 0.0001 SOL per token
        let sell_price = 0.000102; // 0.000102 SOL per token (2% higher)
        let sol_amount = 0.5;
        
        let result = calc.calculate(buy_price, sell_price, sol_amount, 0.01, 0.01);
        
        assert!(result.spread_percent > 1.9 && result.spread_percent < 2.1);
        assert!(result.tokens_bought > 0.0);
        assert!(result.sol_received > 0.0);
    }
    
    #[test]
    fn test_unprofitable_spread() {
        let calc = ProfitCalculator {
            min_profit_sol: 0.001,
            priority_fee_lamports: 10000,
            tip_lamports: 10000,
        };
        
        // 0.1% spread - too small
        let buy_price = 0.0001;
        let sell_price = 0.0001001;
        
        assert!(!calc.is_spread_profitable(buy_price, sell_price));
    }
}
