//! Real-time CLI Display
//! TUI for monitoring bot status, prices, and P&L

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Row, Table},
    Frame, Terminal,
};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::bot::{PnlTracker, SessionState};
use crate::pools::PoolState;

/// Display state for the TUI
pub struct DisplayState {
    pub session: Arc<RwLock<SessionState>>,
    pub pnl_tracker: Arc<RwLock<PnlTracker>>,
    pub pool_states: Arc<RwLock<Vec<PoolState>>>,
    pub target_token: String,
    pub max_trade_count: u32,
    pub max_loss_sol: f64,
    pub last_trade_time: Arc<RwLock<Option<Instant>>>,
    pub current_spread: Arc<RwLock<f64>>,
    pub bot_status: Arc<RwLock<String>>,
}

/// Run the TUI display
pub async fn run_display(state: DisplayState) -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();

    loop {
        // Draw UI
        terminal.draw(|f| draw_ui(f, &state))?;

        // Handle input
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if crossterm::event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => break,
                        KeyCode::Char('s') | KeyCode::Char('S') => {
                            // Stop signal would be sent here
                        }
                        KeyCode::Char('e') | KeyCode::Char('E') => {
                            // Emergency sell signal would be sent here
                        }
                        KeyCode::Esc => break,
                        _ => {}
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

/// Draw the main UI
fn draw_ui(f: &mut Frame, state: &DisplayState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),  // Header
            Constraint::Length(7),  // Pool prices
            Constraint::Length(7),  // Session stats
            Constraint::Length(6),  // Last trade
            Constraint::Length(3),  // Controls
        ])
        .split(f.area());

    draw_header(f, chunks[0], state);
    draw_pool_prices(f, chunks[1], state);
    draw_session_stats(f, chunks[2], state);
    draw_last_trade(f, chunks[3], state);
    draw_controls(f, chunks[4]);
}

/// Draw header with bot status
fn draw_header(f: &mut Frame, area: Rect, state: &DisplayState) {
    let status = futures::executor::block_on(async {
        state.bot_status.read().await.clone()
    });
    
    let status_color = match status.as_str() {
        "RUNNING" => Color::Green,
        "STOPPED" => Color::Red,
        "EMERGENCY" => Color::Yellow,
        _ => Color::White,
    };

    let header = Paragraph::new(Line::from(vec![
        Span::styled("ARBITRAGE BOT - ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(status, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
    ]))
    .block(Block::default().borders(Borders::ALL).title("Status"));

    f.render_widget(header, area);
}

/// Draw pool prices section
fn draw_pool_prices(f: &mut Frame, area: Rect, state: &DisplayState) {
    let pool_states = futures::executor::block_on(async {
        state.pool_states.read().await.clone()
    });
    
    let spread = futures::executor::block_on(async {
        *state.current_spread.read().await
    });

    let mut rows = Vec::new();
    
    for (i, pool) in pool_states.iter().enumerate() {
        let liq_percent = (pool.liquidity_sol / 100.0 * 100.0).min(100.0);
        let liq_bar = create_liquidity_bar(liq_percent);
        
        rows.push(Row::new(vec![
            format!("Pool {} ({:?})", i + 1, pool.pool_type),
            format!("{:.10} SOL", pool.price),
            liq_bar,
        ]));
    }
    
    // Add spread row
    rows.push(Row::new(vec![
        "Spread".to_string(),
        format!("{:.2}%", spread),
        String::new(),
    ]).style(Style::default().fg(if spread > 0.5 { Color::Green } else { Color::Yellow })));

    let table = Table::new(
        rows,
        [Constraint::Length(25), Constraint::Length(20), Constraint::Length(15)],
    )
    .header(Row::new(vec!["Pool", "Price", "Liquidity"]).style(Style::default().fg(Color::Cyan)))
    .block(Block::default().borders(Borders::ALL).title(format!("Pool Prices - Token: {}...", &state.target_token[..8.min(state.target_token.len())])));

    f.render_widget(table, area);
}

/// Draw session statistics
fn draw_session_stats(f: &mut Frame, area: Rect, state: &DisplayState) {
    let (session, pnl_current, pnl_fees, trade_count) = futures::executor::block_on(async {
        let session = state.session.read().await;
        let pnl = state.pnl_tracker.read().await;
        (
            session.clone(),
            pnl.current_pnl(),
            pnl.total_fees(),
            pnl.trade_count(),
        )
    });

    let pnl_color = if pnl_current >= 0.0 { Color::Green } else { Color::Red };
    let pnl_sign = if pnl_current >= 0.0 { "+" } else { "" };
    
    let remaining_loss = state.max_loss_sol + pnl_current;
    let loss_progress = ((state.max_loss_sol - remaining_loss) / state.max_loss_sol * 100.0).max(0.0).min(100.0);

    let stats_text = vec![
        Line::from(vec![
            Span::raw("Trades: "),
            Span::styled(format!("{} / {}", trade_count, state.max_trade_count), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::raw("P&L: "),
            Span::styled(format!("{}{:.6} SOL", pnl_sign, pnl_current), Style::default().fg(pnl_color)),
        ]),
        Line::from(vec![
            Span::raw("Win Rate: "),
            Span::styled(format!("{:.1}%", session.win_rate()), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::raw("Fees Paid: "),
            Span::styled(format!("{:.6} SOL", pnl_fees), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::raw("Max Loss Remaining: "),
            Span::styled(format!("{:.4} SOL", remaining_loss), Style::default().fg(if remaining_loss > 0.5 { Color::Green } else { Color::Red })),
        ]),
    ];

    let stats = Paragraph::new(stats_text)
        .block(Block::default().borders(Borders::ALL).title("Session Stats"));

    f.render_widget(stats, area);
}

/// Draw last trade info
fn draw_last_trade(f: &mut Frame, area: Rect, state: &DisplayState) {
    let (last_trade, time_ago) = futures::executor::block_on(async {
        let pnl = state.pnl_tracker.read().await;
        let last = pnl.last_trade().cloned();
        let time = state.last_trade_time.read().await;
        let ago = time.map(|t| t.elapsed().as_secs());
        (last, ago)
    });

    let content = if let Some(trade) = last_trade {
        let profit_color = if trade.profit_sol >= 0.0 { Color::Green } else { Color::Red };
        let profit_sign = if trade.profit_sol >= 0.0 { "+" } else { "" };
        
        vec![
            Line::from(format!("Time: {}s ago", time_ago.unwrap_or(0))),
            Line::from(format!("Buy: {:.4} SOL → {} tokens", trade.amount_in_sol, trade.tokens_traded)),
            Line::from(format!("Sell: {} tokens → {:.4} SOL", trade.tokens_traded, trade.amount_out_sol)),
            Line::from(vec![
                Span::raw("Profit: "),
                Span::styled(
                    format!("{}{:.6} SOL (fees: {:.6})", profit_sign, trade.profit_sol, trade.fees_sol),
                    Style::default().fg(profit_color),
                ),
            ]),
        ]
    } else {
        vec![Line::from("No trades yet")]
    };

    let last_trade_widget = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title("Last Trade"));

    f.render_widget(last_trade_widget, area);
}

/// Draw control hints
fn draw_controls(f: &mut Frame, area: Rect) {
    let controls = Paragraph::new(Line::from(vec![
        Span::styled("[S]", Style::default().fg(Color::Yellow)),
        Span::raw("top  "),
        Span::styled("[E]", Style::default().fg(Color::Red)),
        Span::raw("mergency Sell  "),
        Span::styled("[Q]", Style::default().fg(Color::Cyan)),
        Span::raw("uit"),
    ]))
    .block(Block::default().borders(Borders::ALL));

    f.render_widget(controls, area);
}

/// Create a simple ASCII liquidity bar
fn create_liquidity_bar(percent: f64) -> String {
    let filled = (percent / 10.0) as usize;
    let empty = 10 - filled;
    format!("[{}{}] {:.0}%", "█".repeat(filled), "░".repeat(empty), percent)
}
