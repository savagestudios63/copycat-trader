//! Decoder pipeline. A single `decode_transaction` fans out to per-program
//! decoders keyed by the outermost invoked program. Each per-program decoder
//! returns Option<DecodedSwap>; the first successful decode wins.

pub mod jupiter;
pub mod meteora;
pub mod orca;
pub mod pumpfun;
pub mod raydium;

use crate::types::{programs, DecodedSwap, Dex};
use anyhow::Result;

/// Minimal representation of a Solana transaction that carries the inputs
/// the decoders actually need.
#[derive(Debug, Clone)]
pub struct TxContext<'a> {
    pub signature: &'a str,
    pub slot: u64,
    pub signer: &'a str,
    pub observed_at_ms: i64,
    /// Account keys referenced by index in the instruction.
    pub account_keys: &'a [String],
    /// All instructions, in execution order, including inner ones from inner
    /// instruction groups that we care about.
    pub instructions: &'a [DecodedIx],
    /// Pre/post token balances keyed by account index → (mint, owner, amount).
    pub pre_token_balances: &'a [TokenBalance],
    pub post_token_balances: &'a [TokenBalance],
    /// Pre/post SOL balances keyed by account index.
    pub pre_sol: &'a [u64],
    pub post_sol: &'a [u64],
}

#[derive(Debug, Clone)]
pub struct DecodedIx {
    pub program_id: String,
    pub data: Vec<u8>,
    /// Indices into `account_keys`.
    pub accounts: Vec<u8>,
    pub is_inner: bool,
}

#[derive(Debug, Clone)]
pub struct TokenBalance {
    pub account_index: u8,
    pub mint: String,
    pub owner: String,
    pub amount: u64,
    pub decimals: u8,
}

/// Run the cascade. Returns the first successful decode.
pub fn decode_transaction(ctx: &TxContext<'_>) -> Result<Option<DecodedSwap>> {
    for ix in ctx.instructions {
        if ix.is_inner {
            continue; // only top-level; inner are considered by the program decoders
        }
        let pid = ix.program_id.as_str();
        let decoded = match pid {
            programs::JUPITER_V6 | programs::JUPITER_V4 => jupiter::decode(ctx, ix)?,
            programs::RAYDIUM_AMM_V4 => raydium::decode_amm(ctx, ix)?,
            programs::RAYDIUM_CLMM => raydium::decode_clmm(ctx, ix)?,
            programs::ORCA_WHIRLPOOL => orca::decode(ctx, ix)?,
            programs::METEORA_DLMM => meteora::decode(ctx, ix)?,
            programs::PUMPFUN => pumpfun::decode(ctx, ix)?,
            _ => None,
        };
        if decoded.is_some() {
            return Ok(decoded);
        }
    }
    // Fallback: infer swap from net token balance deltas if any known DEX
    // program appeared anywhere in the tx (inner or outer).
    if ctx
        .instructions
        .iter()
        .any(|ix| programs::all().contains(&ix.program_id.as_str()))
    {
        if let Some(inferred) = infer_from_balance_deltas(ctx) {
            return Ok(Some(inferred));
        }
    }
    Ok(None)
}

/// Reconstruct a (input_mint, output_mint, input_amt, output_amt) tuple
/// from the pre/post token balance diffs for the target signer.
pub fn infer_from_balance_deltas(ctx: &TxContext<'_>) -> Option<DecodedSwap> {
    use std::collections::HashMap;
    let mut deltas: HashMap<String, i128> = HashMap::new();

    for pre in ctx.pre_token_balances.iter().filter(|b| b.owner == ctx.signer) {
        *deltas.entry(pre.mint.clone()).or_insert(0) -= pre.amount as i128;
    }
    for post in ctx.post_token_balances.iter().filter(|b| b.owner == ctx.signer) {
        *deltas.entry(post.mint.clone()).or_insert(0) += post.amount as i128;
    }

    // Also reflect native SOL delta for signer (index 0 by convention).
    let sol_delta: i128 =
        ctx.post_sol.first().copied().unwrap_or(0) as i128
            - ctx.pre_sol.first().copied().unwrap_or(0) as i128;
    if sol_delta.abs() > 5_000 {
        *deltas
            .entry(crate::types::mints::WSOL.to_string())
            .or_insert(0) += sol_delta;
    }

    // Need exactly one negative (input) + one positive (output) of meaningful size.
    let negs: Vec<_> = deltas.iter().filter(|(_, v)| **v < -1).collect();
    let poss: Vec<_> = deltas.iter().filter(|(_, v)| **v > 1).collect();
    if negs.len() != 1 || poss.len() != 1 {
        return None;
    }
    let (in_mint, in_amt) = negs[0];
    let (out_mint, out_amt) = poss[0];

    use crate::types::{mints, Direction};
    let direction = if mints::is_quote(in_mint) && !mints::is_quote(out_mint) {
        Direction::Buy
    } else if !mints::is_quote(in_mint) && mints::is_quote(out_mint) {
        Direction::Sell
    } else {
        Direction::Unknown
    };

    Some(DecodedSwap {
        signature: ctx.signature.to_string(),
        slot: ctx.slot,
        target_wallet: ctx.signer.to_string(),
        dex: Dex::Unknown,
        direction,
        input_mint: in_mint.clone(),
        input_amount: (-*in_amt) as u64,
        output_mint: out_mint.clone(),
        output_amount: (*out_amt) as u64,
        program_id: String::new(),
        observed_at_ms: ctx.observed_at_ms,
    })
}

/// Helper: from a list of account indices, compute the signer's +/- token
/// balance delta on that mint. Used by per-program decoders that only have
/// structural hints about "user in / user out" accounts.
pub(crate) fn signer_token_delta(ctx: &TxContext<'_>, mint: &str) -> i128 {
    let pre: i128 = ctx
        .pre_token_balances
        .iter()
        .filter(|b| b.owner == ctx.signer && b.mint == mint)
        .map(|b| b.amount as i128)
        .sum();
    let post: i128 = ctx
        .post_token_balances
        .iter()
        .filter(|b| b.owner == ctx.signer && b.mint == mint)
        .map(|b| b.amount as i128)
        .sum();
    post - pre
}
