//! ratatui dashboard. Three panes:
//!   [top]   Per-wallet PnL summary + open-position count.
//!   [mid]   Open positions (mint, target wallet, entry price, size, strategy).
//!   [bot]   Live target feed (decoded swaps + copy actions).

use crate::config::Config;
use crate::db::{Db, Position, WalletPnl};
use crate::types::UiEvent;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame, Terminal,
};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const FEED_CAP: usize = 200;

struct Ui {
    feed: VecDeque<String>,
    positions: Vec<Position>,
    pnl: Vec<WalletPnl>,
}

pub async fn run(
    cfg: Arc<Config>,
    db: Db,
    mut events: mpsc::Receiver<UiEvent>,
    refresh_hz: u32,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick = Duration::from_millis(1000 / refresh_hz.max(1) as u64);
    let mut ui = Ui {
        feed: VecDeque::with_capacity(FEED_CAP),
        positions: Vec::new(),
        pnl: Vec::new(),
    };

    let res = event_loop(&mut terminal, &cfg, &db, &mut ui, &mut events, tick).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    res
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cfg: &Config,
    db: &Db,
    ui: &mut Ui,
    events: &mut mpsc::Receiver<UiEvent>,
    tick: Duration,
) -> Result<()> {
    let mut ticker = tokio::time::interval(tick);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Refresh DB-backed views on each tick.
                if let Ok(p) = db.open_positions().await { ui.positions = p; }
                if let Ok(pnl) = db.wallet_pnl().await { ui.pnl = pnl; }
                terminal.draw(|f| draw(f, cfg, ui))?;

                // Drain key events briefly each tick.
                while event::poll(Duration::from_millis(0))? {
                    if let Event::Key(k) = event::read()? {
                        match k.code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            _ => {}
                        }
                    }
                }
            }
            Some(ev) = events.recv() => push_event(ui, ev),
        }
    }
}

fn push_event(ui: &mut Ui, ev: UiEvent) {
    let ts = chrono::Local::now().format("%H:%M:%S%.3f");
    let line = match ev {
        UiEvent::TargetTrade(s) => format!(
            "{ts} TARGET {} {:?} {} → {} ({} -> {}) {}",
            short(&s.target_wallet),
            s.direction,
            s.input_amount,
            s.output_amount,
            short(&s.input_mint),
            short(&s.output_mint),
            s.dex.as_str()
        ),
        UiEvent::CopyBuySubmitted {
            target_wallet,
            signature,
            output_mint,
            ..
        } => format!(
            "{ts} COPY BUY  {} → {} sig={}",
            short(&target_wallet),
            short(&output_mint),
            short(&signature)
        ),
        UiEvent::CopySellSubmitted {
            target_wallet,
            signature,
            mint,
        } => format!(
            "{ts} COPY SELL {} → {} sig={}",
            short(&target_wallet),
            short(&mint),
            short(&signature)
        ),
        UiEvent::CopyRejected {
            target_wallet,
            reason,
        } => format!("{ts} REJECT    {} reason={}", short(&target_wallet), reason),
        UiEvent::PositionClosed {
            mint,
            realized_pnl_usd,
        } => format!("{ts} CLOSED    {} pnl=${:+.2}", short(&mint), realized_pnl_usd),
        UiEvent::Log(s) => format!("{ts} {}", s),
    };
    if ui.feed.len() >= FEED_CAP {
        ui.feed.pop_front();
    }
    ui.feed.push_back(line);
}

fn draw(f: &mut Frame, cfg: &Config, ui: &Ui) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1 + cfg.wallets.len().min(8) as u16 + 2),
            Constraint::Percentage(40),
            Constraint::Min(8),
        ])
        .split(f.area());

    draw_pnl(f, areas[0], cfg, ui);
    draw_positions(f, areas[1], ui);
    draw_feed(f, areas[2], ui);
}

fn draw_pnl(f: &mut Frame, area: ratatui::layout::Rect, cfg: &Config, ui: &Ui) {
    let header = Row::new(vec!["wallet", "trades", "open", "realized $"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = cfg.wallets.iter().map(|w| {
        let row = ui.pnl.iter().find(|p| p.target_wallet == w.address);
        let trades = row.map(|r| r.trades).unwrap_or(0);
        let open = row.map(|r| r.open_positions).unwrap_or(0);
        let pnl = row.map(|r| r.realized_pnl_usd).unwrap_or(0.0);
        let pnl_color = if pnl >= 0.0 { Color::Green } else { Color::Red };
        Row::new(vec![
            Cell::from(format!("{} ({})", w.name, short(&w.address))),
            Cell::from(trades.to_string()),
            Cell::from(open.to_string()),
            Cell::from(format!("{:+.2}", pnl)).style(Style::default().fg(pnl_color)),
        ])
    });
    let widths = [
        Constraint::Percentage(50),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(14),
    ];
    let t = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" per-wallet pnl "));
    f.render_widget(t, area);
}

fn draw_positions(f: &mut Frame, area: ratatui::layout::Rect, ui: &Ui) {
    let header = Row::new(vec!["mint", "target", "entry px", "qty", "strat", "tp/sl"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = ui.positions.iter().map(|p| {
        Row::new(vec![
            Cell::from(short(&p.mint)),
            Cell::from(short(&p.target_wallet)),
            Cell::from(format!("{:.9}", p.entry_price_usd)),
            Cell::from(p.entry_output_amount.to_string()),
            Cell::from(p.exit_strategy.clone()),
            Cell::from(format!(
                "{}/{}",
                p.tp_pct.map(|v| format!("{:.1}", v)).unwrap_or("-".into()),
                p.sl_pct.map(|v| format!("{:.1}", v)).unwrap_or("-".into())
            )),
        ])
    });
    let widths = [
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(16),
        Constraint::Length(14),
        Constraint::Length(20),
        Constraint::Length(10),
    ];
    let t = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" open positions "));
    f.render_widget(t, area);
}

fn draw_feed(f: &mut Frame, area: ratatui::layout::Rect, ui: &Ui) {
    let visible = area.height.saturating_sub(2) as usize;
    let start = ui.feed.len().saturating_sub(visible);
    let lines: Vec<Line> = ui
        .feed
        .iter()
        .skip(start)
        .map(|s| Line::from(Span::raw(s.clone())))
        .collect();
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" live feed  [q=quit] "));
    f.render_widget(p, area);
}

fn short(s: &str) -> String {
    if s.len() <= 8 {
        s.to_string()
    } else {
        format!("{}..{}", &s[..4], &s[s.len() - 4..])
    }
}
