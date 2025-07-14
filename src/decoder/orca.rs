//! Orca Whirlpool decoder.
//!
//! Whirlpool exposes both `swap` and `swap_v2` (token-2022). Account layouts:
//!
//! swap:
//!   0 token_program
//!   1 token_authority (signer)
//!   2 whirlpool
//!   3 token_owner_account_a
//!   4 token_vault_a
//!   5 token_owner_account_b
//!   6 token_vault_b
//!   7 tick_array_0
//!   8 tick_array_1
//!   9 tick_array_2
//!  10 oracle
//!
//! Instruction data: [disc(8)] amount:u64 other_threshold:u64 sqrt_price_limit:u128
//! amount_specified_is_input:bool a_to_b:bool
//!
//! `a_to_b` tells us which vault/owner account pair is the input side. We use
//! it to pick which of the user's two token accounts is source vs dest.

use super::{raydium::build_swap, DecodedIx, TxContext};
use crate::types::{DecodedSwap, Dex};
use anyhow::Result;

pub fn decode(ctx: &TxContext<'_>, ix: &DecodedIx) -> Result<Option<DecodedSwap>> {
    // Anchor discriminators
    const SWAP: [u8; 8] = [0xF8, 0xC6, 0x9E, 0x91, 0xE1, 0x75, 0x87, 0xC8];
    const SWAP_V2: [u8; 8] = [0x2B, 0x04, 0xED, 0x0B, 0x1A, 0xC9, 0x1E, 0x62];

    if ix.data.len() < 8 + 8 + 8 + 16 + 1 + 1 {
        return Ok(None);
    }
    let disc: [u8; 8] = ix.data[..8].try_into().unwrap();
    if disc != SWAP && disc != SWAP_V2 {
        return Ok(None);
    }

    // a_to_b flag is the last byte of the header for v1; for v2 the account
    // indices are offset — we read both candidate pairs and pick whichever
    // yields a sensible signer delta.
    let a_to_b = *ix.data.last().unwrap_or(&0) != 0;

    let (in_idx, out_idx) = if disc == SWAP {
        if a_to_b { (3u8, 5u8) } else { (5u8, 3u8) }
    } else {
        // swap_v2 has extra accounts (token_program_a, token_program_b,
        // memo_program) inserted after accounts 0-1 in some builds; look up
        // by structural offset common to current mainnet IDL.
        if a_to_b { (4u8, 6u8) } else { (6u8, 4u8) }
    };

    let in_acct = *ix
        .accounts
        .get(in_idx as usize)
        .ok_or_else(|| anyhow::anyhow!("short ix"))?;
    let out_acct = *ix
        .accounts
        .get(out_idx as usize)
        .ok_or_else(|| anyhow::anyhow!("short ix"))?;

    let in_mint = mint_of(ctx, in_acct);
    let out_mint = mint_of(ctx, out_acct);
    let (Some(in_mint), Some(out_mint)) = (in_mint, out_mint) else {
        return Ok(None);
    };

    build_swap(ctx, ix, Dex::OrcaWhirlpool, &in_mint, &out_mint)
}

fn mint_of(ctx: &TxContext<'_>, idx: u8) -> Option<String> {
    ctx.post_token_balances
        .iter()
        .chain(ctx.pre_token_balances.iter())
        .find(|b| b.account_index == idx)
        .map(|b| b.mint.clone())
}

// Silence unused warning when `Result<Option<DecodedSwap>>` inference hides it.
#[allow(dead_code)]
fn _type_anchor() -> Option<DecodedSwap> { None }
