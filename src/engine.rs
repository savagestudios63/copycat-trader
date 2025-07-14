//! Core copy logic.
//!
//! Responsibilities:
//!   - Classify DecodedSwap as target-buy or target-sell.
//!   - Apply per-wallet cooldown, block-list, min trade size, position cap.
//!   - Compute copy size via risk::sizer.
//!   - Run rug checks (risk::rug_check).
//!   - Call executor::copy_buy / copy_sell.
//!   - Track open positions in SQLite; fire TP/SL on live price snapshots.

use crate::config::{Config, ExitStrategy, WalletCfg};
#[cfg(test)]
use crate::config::SizingMode;
use crate::db::{Db, Position, TradeRow};
use crate::executor::Executor;
use crate::risk;
use crate::types::{mints, DecodedSwap, Direction, UiEvent};
use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

pub async fn run(
    cfg: Arc<Config>,
    db: Db,
    mut rx: mpsc::Receiver<DecodedSwap>,
    events: mpsc::Sender<UiEvent>,
    mut shutdown: broadcast::Receiver<()>,
    dry_run: bool,
) -> Result<()> {
    let executor = Arc::new(Executor::new(cfg.clone())?);
    let state = Arc::new(EngineState::new());

    // Background task: periodic TP/SL check on open positions.
    let tp_sl_task = {
        let cfg = cfg.clone();
        let db = db.clone();
        let executor = executor.clone();
        let events = events.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            loop {
                ticker.tick().await;
                if let Err(e) = tp_sl_pass(&cfg, &db, &executor, &events, &state, dry_run).await {
                    warn!(error = ?e, "tp_sl pass failed");
                }
            }
        })
    };

    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                tp_sl_task.abort();
                info!("engine shutdown");
                return Ok(());
            }
            maybe = rx.recv() => {
                let Some(swap) = maybe else {
                    tp_sl_task.abort();
                    return Ok(());
                };
                if let Err(e) = handle_swap(&cfg, &db, &executor, &events, &state, swap, dry_run).await {
                    warn!(error = ?e, "handle_swap failed");
                }
            }
        }
    }
}

struct EngineState {
    /// wallet_address → last copy-trade Instant
    last_copy: DashMap<String, Instant>,
    /// (target_wallet, mint) → open position row id
    open_positions: Mutex<HashMap<(String, String), i64>>,
}
impl EngineState {
    fn new() -> Self {
        Self {
            last_copy: DashMap::new(),
            open_positions: Mutex::new(HashMap::new()),
        }
    }
    fn in_cooldown(&self, wallet: &str, cooldown_ms: u64) -> bool {
        if cooldown_ms == 0 {
            return false;
        }
        if let Some(last) = self.last_copy.get(wallet) {
            return last.elapsed() < Duration::from_millis(cooldown_ms);
        }
        false
    }
    fn mark_copy(&self, wallet: &str) {
        self.last_copy.insert(wallet.to_string(), Instant::now());
    }
}

async fn handle_swap(
    cfg: &Config,
    db: &Db,
    executor: &Executor,
    events: &mpsc::Sender<UiEvent>,
    state: &EngineState,
    swap: DecodedSwap,
    dry_run: bool,
) -> Result<()> {
    let _ = events.try_send(UiEvent::TargetTrade(swap.clone()));

    // Always log target's trade.
    let _ = db
        .insert_trade(&TradeRow {
            id: 0,
            signature: swap.signature.clone(),
            target_wallet: swap.target_wallet.clone(),
            side: match swap.direction {
                Direction::Buy => "target_buy".into(),
                Direction::Sell => "target_sell".into(),
                Direction::Unknown => "target_unknown".into(),
            },
            dex: swap.dex.as_str().into(),
            input_mint: swap.input_mint.clone(),
            input_amount: swap.input_amount as i64,
            output_mint: swap.output_mint.clone(),
            output_amount: swap.output_amount as i64,
            slot: swap.slot as i64,
            ts: Utc::now(),
        })
        .await;

    let Some(wcfg) = cfg.wallet_by_address(&swap.target_wallet) else {
        return Ok(());
    };

    match swap.direction {
        Direction::Buy => on_target_buy(cfg, db, executor, events, state, wcfg, &swap, dry_run).await,
        Direction::Sell => on_target_sell(cfg, db, executor, events, state, wcfg, &swap, dry_run).await,
        Direction::Unknown => Ok(()),
    }
}

async fn on_target_buy(
    cfg: &Config,
    db: &Db,
    executor: &Executor,
    events: &mpsc::Sender<UiEvent>,
    state: &EngineState,
    w: &WalletCfg,
    swap: &DecodedSwap,
    dry_run: bool,
) -> Result<()> {
    if state.in_cooldown(&w.address, w.cooldown_ms) {
        reject(events, &w.address, "cooldown").await;
        return Ok(());
    }

    if w.blocked_tokens.iter().any(|t| t == &swap.output_mint) {
        reject(events, &w.address, "blocked_token").await;
        return Ok(());
    }

    // Translate target's input amount to approximate USD via quote-mint price.
    let target_usd = executor
        .estimate_input_usd(&swap.input_mint, swap.input_amount)
        .await
        .unwrap_or(0.0);

    if target_usd < w.min_target_trade_usd {
        reject(events, &w.address, "below_min_trade_size").await;
        return Ok(());
    }

    // Rug / honeypot checks before sizing — fast-path abort.
    if let Err(reason) = risk::pre_buy_check(cfg, executor, &swap.output_mint).await {
        reject(events, &w.address, &format!("risk:{reason}")).await;
        return Ok(());
    }

    // Size the copy trade in the same input token as the target.
    let copy_input_amount = risk::size_copy(
        w,
        &swap.input_mint,
        swap.input_amount,
        target_usd,
        executor,
    )
    .await?;
    if copy_input_amount == 0 {
        reject(events, &w.address, "zero_size").await;
        return Ok(());
    }

    // Enforce per-position cap in USD.
    let copy_usd = executor
        .estimate_input_usd(&swap.input_mint, copy_input_amount)
        .await
        .unwrap_or(target_usd);
    if copy_usd > w.max_position_usd {
        reject(events, &w.address, "max_position_usd_exceeded").await;
        return Ok(());
    }

    if dry_run {
        info!(wallet = %w.address, mint = %swap.output_mint, usd = copy_usd, "dry-run: would buy");
        return Ok(());
    }

    let slippage_bps = if w.max_slippage_bps > 0 {
        w.max_slippage_bps
    } else {
        cfg.executor.default_slippage_bps
    };

    let result = executor
        .copy_buy(
            &swap.input_mint,
            &swap.output_mint,
            copy_input_amount,
            slippage_bps,
        )
        .await;

    match result {
        Ok(res) => {
            state.mark_copy(&w.address);
            let _ = events
                .try_send(UiEvent::CopyBuySubmitted {
                    target_wallet: w.address.clone(),
                    signature: res.signature.clone(),
                    input_mint: swap.input_mint.clone(),
                    input_amount: copy_input_amount,
                    output_mint: swap.output_mint.clone(),
                });

            // Persist position.
            let pos = Position {
                id: 0,
                target_wallet: w.address.clone(),
                mint: swap.output_mint.clone(),
                entry_signature: res.signature.clone(),
                entry_input_mint: swap.input_mint.clone(),
                entry_input_amount: copy_input_amount as i64,
                entry_output_amount: res.out_amount as i64,
                entry_price_usd: if res.out_amount > 0 {
                    copy_usd / (res.out_amount as f64)
                } else {
                    0.0
                },
                entry_slot: swap.slot as i64,
                opened_at: Utc::now(),
                closed_at: None,
                exit_signature: None,
                exit_output_amount: None,
                realized_pnl_usd: None,
                tp_pct: if w.take_profit_pct > 0.0 { Some(w.take_profit_pct) } else { None },
                sl_pct: if w.stop_loss_pct > 0.0 { Some(w.stop_loss_pct) } else { None },
                exit_strategy: match w.exit_strategy {
                    ExitStrategy::Mirror => "mirror".into(),
                    ExitStrategy::TpSl => "tp_sl".into(),
                    ExitStrategy::MirrorThenTpSl => "mirror_then_tp_sl".into(),
                },
            };
            let id = db.open_position(&pos).await?;
            state
                .open_positions
                .lock()
                .insert((w.address.clone(), swap.output_mint.clone()), id);

            // Log copy_buy trade row.
            let _ = db
                .insert_trade(&TradeRow {
                    id: 0,
                    signature: res.signature,
                    target_wallet: w.address.clone(),
                    side: "copy_buy".into(),
                    dex: "jupiter".into(),
                    input_mint: swap.input_mint.clone(),
                    input_amount: copy_input_amount as i64,
                    output_mint: swap.output_mint.clone(),
                    output_amount: res.out_amount as i64,
                    slot: swap.slot as i64,
                    ts: Utc::now(),
                })
                .await;
        }
        Err(e) => {
            warn!(error = ?e, "copy_buy failed");
            reject(events, &w.address, &format!("executor:{e}")).await;
        }
    }
    Ok(())
}

async fn on_target_sell(
    cfg: &Config,
    db: &Db,
    executor: &Executor,
    events: &mpsc::Sender<UiEvent>,
    state: &EngineState,
    w: &WalletCfg,
    swap: &DecodedSwap,
    _dry_run: bool,
) -> Result<()> {
    let mirror = matches!(
        w.exit_strategy,
        ExitStrategy::Mirror | ExitStrategy::MirrorThenTpSl
    );
    if !mirror {
        return Ok(());
    }

    let key = (w.address.clone(), swap.input_mint.clone());
    let pos_id = { state.open_positions.lock().get(&key).copied() };
    let Some(pos_id) = pos_id else {
        debug!("sell observed but no local position");
        return Ok(());
    };

    let open_positions = db.open_positions_for_wallet(&w.address).await?;
    let Some(pos) = open_positions.into_iter().find(|p| p.id == pos_id) else {
        return Ok(());
    };

    // Quote in terms of the quote mint the target used (fallback WSOL).
    let quote_mint = if mints::is_quote(&swap.output_mint) {
        swap.output_mint.clone()
    } else {
        mints::WSOL.to_string()
    };
    let slippage_bps = if w.max_slippage_bps > 0 {
        w.max_slippage_bps
    } else {
        cfg.executor.default_slippage_bps
    };

    let res = executor
        .copy_sell(
            &swap.input_mint,
            &quote_mint,
            pos.entry_output_amount as u64,
            slippage_bps,
        )
        .await;

    match res {
        Ok(r) => {
            let exit_usd = executor
                .estimate_input_usd(&quote_mint, r.out_amount)
                .await
                .unwrap_or(0.0);
            let entry_usd =
                pos.entry_price_usd * pos.entry_output_amount as f64;
            let pnl = exit_usd - entry_usd;
            db.close_position(pos.id, &r.signature, r.out_amount as i64, pnl).await?;
            state.open_positions.lock().remove(&key);
            let _ = events.try_send(UiEvent::CopySellSubmitted {
                target_wallet: w.address.clone(),
                signature: r.signature,
                mint: swap.input_mint.clone(),
            });
            let _ = events.try_send(UiEvent::PositionClosed {
                mint: swap.input_mint.clone(),
                realized_pnl_usd: pnl,
            });
        }
        Err(e) => {
            warn!(error = ?e, "copy_sell failed");
            reject(events, &w.address, &format!("executor_sell:{e}")).await;
        }
    }

    Ok(())
}

/// Periodic scan of open positions for TP/SL trigger.
async fn tp_sl_pass(
    cfg: &Config,
    db: &Db,
    executor: &Executor,
    events: &mpsc::Sender<UiEvent>,
    state: &EngineState,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    let positions = db.open_positions().await?;
    for pos in positions {
        let Some(wcfg) = cfg.wallet_by_address(&pos.target_wallet) else {
            continue;
        };
        if !matches!(
            wcfg.exit_strategy,
            ExitStrategy::TpSl | ExitStrategy::MirrorThenTpSl
        ) {
            continue;
        }
        let tp = pos.tp_pct.unwrap_or(0.0);
        let sl = pos.sl_pct.unwrap_or(0.0);
        if tp <= 0.0 && sl <= 0.0 {
            continue;
        }

        // Current mark: quote selling the full position back to entry quote.
        let quote_mint = if mints::is_quote(&pos.entry_input_mint) {
            pos.entry_input_mint.clone()
        } else {
            mints::WSOL.to_string()
        };
        let Ok(quote) = executor
            .quote(
                &pos.mint,
                &quote_mint,
                pos.entry_output_amount as u64,
                wcfg.max_slippage_bps.max(cfg.executor.default_slippage_bps),
            )
            .await
        else {
            continue;
        };
        let entry_usd = pos.entry_price_usd * pos.entry_output_amount as f64;
        let exit_usd = executor
            .estimate_input_usd(&quote_mint, quote.out_amount)
            .await
            .unwrap_or(0.0);
        if entry_usd <= 0.0 {
            continue;
        }
        let pnl_pct = ((exit_usd - entry_usd) / entry_usd) * 100.0;

        let should_exit = (tp > 0.0 && pnl_pct >= tp) || (sl > 0.0 && pnl_pct <= -sl);
        if !should_exit {
            continue;
        }

        match executor
            .copy_sell(
                &pos.mint,
                &quote_mint,
                pos.entry_output_amount as u64,
                wcfg.max_slippage_bps.max(cfg.executor.default_slippage_bps),
            )
            .await
        {
            Ok(r) => {
                let realized = exit_usd - entry_usd;
                db.close_position(pos.id, &r.signature, r.out_amount as i64, realized)
                    .await?;
                state
                    .open_positions
                    .lock()
                    .remove(&(pos.target_wallet.clone(), pos.mint.clone()));
                let _ = events.try_send(UiEvent::PositionClosed {
                    mint: pos.mint.clone(),
                    realized_pnl_usd: realized,
                });
                info!(mint = %pos.mint, pnl = realized, "tp_sl exit");
            }
            Err(e) => warn!(error = ?e, "tp_sl exit failed"),
        }
    }
    Ok(())
}

async fn reject(events: &mpsc::Sender<UiEvent>, wallet: &str, reason: &str) {
    let _ = events
        .try_send(UiEvent::CopyRejected {
            target_wallet: wallet.to_string(),
            reason: reason.to_string(),
        });
}

/// Helper exposed for tests: pure sizing decision without touching network.
#[cfg(test)]
pub fn size_for_test(w: &WalletCfg, target_input_amount: u64, target_usd: f64) -> u64 {
    match w.sizing_mode {
        SizingMode::FixedSol => (w.sizing_value * 1e9) as u64,
        SizingMode::FixedUsd => {
            if target_usd > 0.0 {
                ((w.sizing_value / target_usd) * target_input_amount as f64) as u64
            } else {
                0
            }
        }
        SizingMode::PctOfTarget => ((w.sizing_value / 100.0) * target_input_amount as f64) as u64,
    }
}
