//! Jupiter v4/v6 aggregator decoder.
//!
//! Jupiter routes through many inner programs; its own instruction data is a
//! route schema we don't need to parse. The cleanest signal for copy-trading
//! is the net user token delta, which `infer_from_balance_deltas` already
//! computes — we just re-label the DEX so downstream telemetry is correct.

use super::{infer_from_balance_deltas, DecodedIx, TxContext};
use crate::types::{DecodedSwap, Dex};
use anyhow::Result;

pub fn decode(ctx: &TxContext<'_>, ix: &DecodedIx) -> Result<Option<DecodedSwap>> {
    // Jupiter ixs we care about:
    //   route / sharedAccountsRoute / exactOutRoute / sharedAccountsExactOutRoute
    // Anchor discriminator is the first 8 bytes. We check for well-known ones,
    // but accept any Jupiter ix with a user-impacting balance delta as a swap.
    const KNOWN: &[[u8; 8]] = &[
        // sha256("global:route")[..8]
        [0xE5, 0x17, 0xCB, 0x97, 0x7A, 0xE3, 0xAD, 0x2A],
        // sha256("global:shared_accounts_route")[..8]
        [0xC1, 0x20, 0x9B, 0x33, 0x41, 0xD6, 0x9C, 0x81],
        // sha256("global:exact_out_route")[..8]
        [0xD0, 0x33, 0xEF, 0x97, 0x7B, 0x2B, 0xED, 0x5C],
    ];

    if ix.data.len() < 8 {
        return Ok(None);
    }
    let disc = &ix.data[..8];
    let is_known = KNOWN.iter().any(|k| k == disc);

    // Even for unknown Jupiter ixs, trust the net balance delta if present.
    let mut inferred = infer_from_balance_deltas(ctx);
    if let Some(ref mut d) = inferred {
        d.dex = Dex::Jupiter;
        d.program_id = ix.program_id.clone();
    }

    if is_known || inferred.is_some() {
        Ok(inferred)
    } else {
        Ok(None)
    }
}
