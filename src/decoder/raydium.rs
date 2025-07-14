//! Raydium AMM v4 + CLMM decoders.
//!
//! For AMM v4, the `swap_base_in` (u8=9) / `swap_base_out` (u8=11) instructions
//! are the ones we care about. Account layout (swap_base_in):
//!   0 token_program
//!   1 amm
//!   2 amm_authority
//!   3 amm_open_orders
//!   4 amm_target_orders
//!   5 pool_coin_vault
//!   6 pool_pc_vault
//!   7 serum_program
//!   8 serum_market
//!   9 serum_bids
//!  10 serum_asks
//!  11 serum_event_queue
//!  12 serum_coin_vault
//!  13 serum_pc_vault
//!  14 serum_vault_signer
//!  15 user_source_token
//!  16 user_dest_token
//!  17 user_source_owner (signer)
//!
//! For CLMM, the swap_v2 discriminator is an Anchor 8-byte hash. Instruction
//! data carries amount + other_amount_threshold + sqrt_price_limit_x64 + is_base_input.
//!
//! In both cases the cheapest reliable signal is to take the user's pre/post
//! token balance deltas rather than parsing in/out amounts from instruction
//! data (which can under-report due to slippage). We use that as the primary
//! source and the instruction data as a sanity check that the tx really did
//! a swap.

use super::{signer_token_delta, DecodedIx, TxContext};
use crate::types::{mints, DecodedSwap, Dex, Direction};
use anyhow::Result;

const AMM_SWAP_BASE_IN: u8 = 9;
const AMM_SWAP_BASE_OUT: u8 = 11;

pub fn decode_amm(ctx: &TxContext<'_>, ix: &DecodedIx) -> Result<Option<DecodedSwap>> {
    if ix.data.is_empty() {
        return Ok(None);
    }
    let tag = ix.data[0];
    if tag != AMM_SWAP_BASE_IN && tag != AMM_SWAP_BASE_OUT {
        return Ok(None);
    }

    // Look up the two user token accounts (src / dst) → determine mints from
    // the token balance map.
    let src_idx = *ix.accounts.get(15).ok_or_else(|| anyhow::anyhow!("short ix"))?;
    let dst_idx = *ix.accounts.get(16).ok_or_else(|| anyhow::anyhow!("short ix"))?;

    let src_mint = balance_mint_for(ctx, src_idx);
    let dst_mint = balance_mint_for(ctx, dst_idx);
    let (Some(src_mint), Some(dst_mint)) = (src_mint, dst_mint) else {
        return Ok(None);
    };

    build_swap(ctx, ix, Dex::RaydiumAmm, &src_mint, &dst_mint)
}

pub fn decode_clmm(ctx: &TxContext<'_>, ix: &DecodedIx) -> Result<Option<DecodedSwap>> {
    // Anchor discriminator for "swap" / "swap_v2".
    const SWAP: [u8; 8] = [0xF8, 0xC6, 0x9E, 0x91, 0xE1, 0x75, 0x87, 0xC8];
    const SWAP_V2: [u8; 8] = [0x2B, 0x04, 0xED, 0x0B, 0x1A, 0xC9, 0x1E, 0x62];

    if ix.data.len() < 8 {
        return Ok(None);
    }
    let disc: [u8; 8] = ix.data[..8].try_into().unwrap();
    if disc != SWAP && disc != SWAP_V2 {
        return Ok(None);
    }

    // CLMM swap_v2 account layout (relevant slice):
    //   0 payer (signer)
    //   1 amm_config
    //   2 pool_state
    //   3 input_token_account
    //   4 output_token_account
    //   5 input_vault
    //   6 output_vault
    //   ...
    let in_idx = *ix.accounts.get(3).ok_or_else(|| anyhow::anyhow!("short ix"))?;
    let out_idx = *ix.accounts.get(4).ok_or_else(|| anyhow::anyhow!("short ix"))?;
    let in_mint = balance_mint_for(ctx, in_idx);
    let out_mint = balance_mint_for(ctx, out_idx);
    let (Some(in_mint), Some(out_mint)) = (in_mint, out_mint) else {
        return Ok(None);
    };

    build_swap(ctx, ix, Dex::RaydiumClmm, &in_mint, &out_mint)
}

fn balance_mint_for(ctx: &TxContext<'_>, idx: u8) -> Option<String> {
    ctx.post_token_balances
        .iter()
        .chain(ctx.pre_token_balances.iter())
        .find(|b| b.account_index == idx)
        .map(|b| b.mint.clone())
}

pub(crate) fn build_swap(
    ctx: &TxContext<'_>,
    ix: &DecodedIx,
    dex: Dex,
    in_mint: &str,
    out_mint: &str,
) -> Result<Option<DecodedSwap>> {
    // Signer deltas — authoritative source for actual in/out amounts.
    let in_delta = signer_token_delta(ctx, in_mint); // expect negative
    let out_delta = signer_token_delta(ctx, out_mint); // expect positive

    // Handle native-SOL leg: if in_mint is wSOL but the user burned lamports
    // via a wrap, the token delta is zero — fall back to the SOL delta.
    let in_amount = if in_delta < 0 {
        (-in_delta) as u64
    } else if in_mint == mints::WSOL {
        let sol_diff = ctx.post_sol.first().copied().unwrap_or(0) as i128
            - ctx.pre_sol.first().copied().unwrap_or(0) as i128;
        if sol_diff < 0 { (-sol_diff) as u64 } else { 0 }
    } else {
        0
    };
    let out_amount = if out_delta > 0 { out_delta as u64 } else { 0 };

    if in_amount == 0 || out_amount == 0 {
        return Ok(None);
    }

    let direction = classify(in_mint, out_mint);
    Ok(Some(DecodedSwap {
        signature: ctx.signature.to_string(),
        slot: ctx.slot,
        target_wallet: ctx.signer.to_string(),
        dex,
        direction,
        input_mint: in_mint.to_string(),
        input_amount: in_amount,
        output_mint: out_mint.to_string(),
        output_amount: out_amount,
        program_id: ix.program_id.clone(),
        observed_at_ms: ctx.observed_at_ms,
    }))
}

pub(crate) fn classify(in_mint: &str, out_mint: &str) -> Direction {
    if mints::is_quote(in_mint) && !mints::is_quote(out_mint) {
        Direction::Buy
    } else if !mints::is_quote(in_mint) && mints::is_quote(out_mint) {
        Direction::Sell
    } else {
        Direction::Unknown
    }
}
