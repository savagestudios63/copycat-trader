//! Pump.fun bonding-curve decoder.
//!
//! Pump.fun uses two Anchor instructions:
//!   - `buy(amount: u64, max_sol_cost: u64)`
//!   - `sell(amount: u64, min_sol_output: u64)`
//!
//! Account layout (buy):
//!   0 global
//!   1 fee_recipient
//!   2 mint
//!   3 bonding_curve
//!   4 associated_bonding_curve
//!   5 associated_user
//!   6 user (signer)
//!   ...
//!
//! For our purposes the mint account (index 2) identifies the meme token. The
//! input/output amounts again come from the signer's SOL + token deltas.

use super::DecodedIx;
use crate::decoder::TxContext;
use crate::types::{mints, DecodedSwap, Dex, Direction};
use anyhow::Result;

pub fn decode(ctx: &TxContext<'_>, ix: &DecodedIx) -> Result<Option<DecodedSwap>> {
    // Anchor discriminators for pump.fun bonding curve (from IDL).
    const BUY: [u8; 8] = [0x66, 0x06, 0x3D, 0x12, 0x01, 0xDA, 0xEB, 0xEA];
    const SELL: [u8; 8] = [0x33, 0xE6, 0x85, 0xA4, 0x01, 0x7F, 0x83, 0xAD];

    if ix.data.len() < 8 {
        return Ok(None);
    }
    let disc: [u8; 8] = ix.data[..8].try_into().unwrap();
    let is_buy = disc == BUY;
    let is_sell = disc == SELL;
    if !is_buy && !is_sell {
        return Ok(None);
    }

    // Resolve the mint from account index 2.
    let mint_idx = *ix.accounts.get(2).ok_or_else(|| anyhow::anyhow!("short ix"))?;
    let mint_pubkey = ctx
        .account_keys
        .get(mint_idx as usize)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("account index oob"))?;

    // Signer's native SOL delta (lamports).
    let sol_delta: i128 = ctx.post_sol.first().copied().unwrap_or(0) as i128
        - ctx.pre_sol.first().copied().unwrap_or(0) as i128;

    // Signer's token delta on the pump mint.
    let tok_delta = crate::decoder::signer_token_delta(ctx, &mint_pubkey);

    let (input_mint, input_amount, output_mint, output_amount, direction) = if is_buy {
        // Target spent SOL, received token.
        if sol_delta >= 0 || tok_delta <= 0 {
            return Ok(None);
        }
        (
            mints::WSOL.to_string(),
            (-sol_delta) as u64,
            mint_pubkey.clone(),
            tok_delta as u64,
            Direction::Buy,
        )
    } else {
        // Target sold token, received SOL.
        if sol_delta <= 0 || tok_delta >= 0 {
            return Ok(None);
        }
        (
            mint_pubkey.clone(),
            (-tok_delta) as u64,
            mints::WSOL.to_string(),
            sol_delta as u64,
            Direction::Sell,
        )
    };

    Ok(Some(DecodedSwap {
        signature: ctx.signature.to_string(),
        slot: ctx.slot,
        target_wallet: ctx.signer.to_string(),
        dex: Dex::PumpFun,
        direction,
        input_mint,
        input_amount,
        output_mint,
        output_amount,
        program_id: ix.program_id.clone(),
        observed_at_ms: ctx.observed_at_ms,
    }))
}
