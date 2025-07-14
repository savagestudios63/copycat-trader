//! Meteora DLMM decoder.
//!
//! DLMM (Dynamic Liquidity Market Maker) uses bin arrays and a `swap` Anchor
//! instruction. The cheapest correct path for us is:
//!   1. Match the Anchor "swap" discriminator.
//!   2. Read the two user token accounts (userInTokenAccount / userOutTokenAccount).
//!   3. Derive mints and amounts from the signer's balance deltas — same
//!      logic as Raydium/Orca. DLMM's per-bin pricing does not change this.

use super::{raydium::build_swap, DecodedIx, TxContext};
use crate::types::{DecodedSwap, Dex};
use anyhow::Result;

pub fn decode(ctx: &TxContext<'_>, ix: &DecodedIx) -> Result<Option<DecodedSwap>> {
    // Anchor `swap` + `swap_exact_out` + `swap_with_price_impact` discriminators.
    const SWAP: [u8; 8] = [0xF8, 0xC6, 0x9E, 0x91, 0xE1, 0x75, 0x87, 0xC8];
    const SWAP_EXACT_OUT: [u8; 8] = [0xFA, 0x49, 0x65, 0x2C, 0x68, 0x2B, 0x9A, 0x7D];
    const SWAP_WPI: [u8; 8] = [0x36, 0x77, 0x15, 0xA7, 0x57, 0x04, 0x97, 0x46];

    if ix.data.len() < 8 {
        return Ok(None);
    }
    let disc: [u8; 8] = ix.data[..8].try_into().unwrap();
    if disc != SWAP && disc != SWAP_EXACT_OUT && disc != SWAP_WPI {
        return Ok(None);
    }

    // DLMM swap accounts (condensed):
    //   0 lb_pair
    //   1 bin_array_bitmap_extension (opt)
    //   2 reserve_x
    //   3 reserve_y
    //   4 user_token_in
    //   5 user_token_out
    //   6 token_x_mint
    //   7 token_y_mint
    //   8 oracle
    //   9+ bin_arrays
    //   ... user (signer) near end
    let in_idx = *ix.accounts.get(4).ok_or_else(|| anyhow::anyhow!("short ix"))?;
    let out_idx = *ix.accounts.get(5).ok_or_else(|| anyhow::anyhow!("short ix"))?;

    let in_mint = ctx
        .post_token_balances
        .iter()
        .chain(ctx.pre_token_balances.iter())
        .find(|b| b.account_index == in_idx)
        .map(|b| b.mint.clone());
    let out_mint = ctx
        .post_token_balances
        .iter()
        .chain(ctx.pre_token_balances.iter())
        .find(|b| b.account_index == out_idx)
        .map(|b| b.mint.clone());

    let (Some(in_mint), Some(out_mint)) = (in_mint, out_mint) else {
        return Ok(None);
    };
    build_swap(ctx, ix, Dex::MeteoraDlmm, &in_mint, &out_mint)
}
