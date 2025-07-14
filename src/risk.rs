//! Rug / honeypot screening + position sizer.
//!
//! Checks applied before a copy-buy:
//!   1. Mint is not on the wallet's blocklist (caller's responsibility).
//!   2. Mint authority null (if required by config).
//!   3. Freeze authority null (if required by config).
//!   4. Round-trip honeypot simulation: quote buy then quote sell; reject if
//!      price-impact exceeds the config threshold.

use crate::config::{Config, SizingMode, WalletCfg};
use crate::executor::Executor;
use crate::types::mints;
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;
use tracing::debug;

/// Runs all risk checks against a target output mint. Returns Ok(()) if the
/// mint is acceptable, Err(reason) otherwise.
pub async fn pre_buy_check(
    cfg: &Config,
    executor: &Executor,
    mint: &str,
) -> Result<(), String> {
    // Never "buy" the quote mints themselves.
    if mints::is_quote(mint) {
        return Err("is_quote_mint".into());
    }

    if cfg.risk.require_mint_authority_null || cfg.risk.require_freeze_authority_null {
        match fetch_mint_account(cfg, executor, mint).await {
            Ok(info) => {
                if cfg.risk.require_mint_authority_null && info.mint_authority.is_some() {
                    return Err("mint_authority_not_null".into());
                }
                if cfg.risk.require_freeze_authority_null && info.freeze_authority.is_some() {
                    return Err("freeze_authority_not_null".into());
                }
            }
            Err(e) => {
                debug!(error = ?e, mint, "mint account fetch failed — allow (soft)");
            }
        }
    }

    if cfg.risk.simulate_sell_before_buy {
        if let Err(e) = honeypot_simulation(cfg, executor, mint).await {
            return Err(format!("honeypot:{e}"));
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct MintAccountInfo {
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub decimals: u8,
    #[serde(default)]
    pub is_initialized: bool,
    #[serde(default)]
    pub supply: String,
}

/// Fetch SPL mint decoded via getAccountInfo(encoding=jsonParsed).
async fn fetch_mint_account(
    cfg: &Config,
    executor: &Executor,
    mint: &str,
) -> Result<MintAccountInfo> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [mint, {"encoding": "jsonParsed", "commitment": cfg.rpc.commitment}]
    });
    let resp = executor
        .jupiter_http()
        .post(&cfg.rpc.http_url)
        .json(&body)
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    let parsed = v["result"]["value"]["data"]["parsed"]["info"].clone();
    if parsed.is_null() {
        return Err(anyhow!("mint account missing or not SPL"));
    }
    let info: MintAccountInfo = serde_json::from_value(parsed)?;
    Ok(info)
}

/// Round-trip: quote a tiny buy, then quote an immediate sell of that exact
/// output amount back to the source mint. If total slippage exceeds the
/// configured cap, the token is either illiquid or honeypotted.
async fn honeypot_simulation(cfg: &Config, executor: &Executor, mint: &str) -> Result<()> {
    let probe_in = 100_000_000u64; // 0.1 SOL probe
    let q1 = executor
        .quote(
            mints::WSOL,
            mint,
            probe_in,
            cfg.executor.default_slippage_bps,
        )
        .await?;
    if q1.out_amount == 0 {
        return Err(anyhow!("no route"));
    }
    let q2 = executor
        .quote(mint, mints::WSOL, q1.out_amount, cfg.executor.default_slippage_bps)
        .await?;
    if q2.out_amount == 0 {
        return Err(anyhow!("no sell route"));
    }

    // Round-trip loss in bps vs the original probe.
    let loss_bps = if q2.out_amount >= probe_in {
        0i64
    } else {
        (((probe_in - q2.out_amount) as i128) * 10_000 / probe_in as i128) as i64
    };
    if loss_bps as u16 > cfg.risk.max_roundtrip_impact_bps {
        return Err(anyhow!(
            "round-trip loss {} bps > {}",
            loss_bps,
            cfg.risk.max_roundtrip_impact_bps
        ));
    }
    Ok(())
}

/// Size a copy buy in the target's input token units.
pub async fn size_copy(
    w: &WalletCfg,
    input_mint: &str,
    target_input_amount: u64,
    target_usd: f64,
    executor: &Executor,
) -> Result<u64> {
    let out = match w.sizing_mode {
        SizingMode::PctOfTarget => {
            ((w.sizing_value / 100.0) * (target_input_amount as f64)) as u64
        }
        SizingMode::FixedSol => {
            // Convert sizing_value SOL → input_mint units.
            if input_mint == mints::WSOL {
                (w.sizing_value * 1e9) as u64
            } else {
                // Quote SOL-equivalent amount of input_mint.
                let sol_in_lamports = (w.sizing_value * 1e9) as u64;
                let q = executor
                    .quote(mints::WSOL, input_mint, sol_in_lamports, 500)
                    .await?;
                q.out_amount
            }
        }
        SizingMode::FixedUsd => {
            if target_usd <= 0.0 {
                return Ok(0);
            }
            // ratio of desired_usd / target_usd applied to input_amount.
            ((w.sizing_value / target_usd) * (target_input_amount as f64)) as u64
        }
    };
    Ok(out)
}
