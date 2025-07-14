//! Integration tests: replay recorded Geyser transaction frames (stored as
//! JSON snapshots in `tests/fixtures/`) through the decoder pipeline and
//! assert on the produced `DecodedSwap`.
//!
//! Fixture schema (stable; can be extended):
//! ```json
//! {
//!   "signature": "...",
//!   "slot": 123,
//!   "signer":   "...",
//!   "account_keys": ["..."],
//!   "instructions": [
//!     {
//!       "program_id": "...",
//!       "data_b64":   "...",
//!       "accounts":   [0,1,2],
//!       "is_inner":   false
//!     }
//!   ],
//!   "pre_token_balances":  [ {"account_index":N,"mint":"..","owner":"..","amount":"1","decimals":9}, ... ],
//!   "post_token_balances": [ ... ],
//!   "pre_sol":  [1000000000],
//!   "post_sol": [ 900000000]
//! }
//! ```

use base64::Engine;
use copycat_trader::decoder::{decode_transaction, DecodedIx, TokenBalance, TxContext};
use copycat_trader::types::{Direction, Dex};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Frame {
    signature: String,
    slot: u64,
    signer: String,
    account_keys: Vec<String>,
    instructions: Vec<FrameIx>,
    pre_token_balances: Vec<FrameTb>,
    post_token_balances: Vec<FrameTb>,
    pre_sol: Vec<u64>,
    post_sol: Vec<u64>,
}

#[derive(Deserialize)]
struct FrameIx {
    program_id: String,
    data_b64: String,
    accounts: Vec<u8>,
    is_inner: bool,
}

#[derive(Deserialize)]
struct FrameTb {
    account_index: u8,
    mint: String,
    owner: String,
    amount: String,
    decimals: u8,
}

fn load_frame(name: &str) -> Frame {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    let s = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()));
    serde_json::from_str(&s).expect("parse fixture json")
}

fn run_frame(f: &Frame) -> Option<copycat_trader::types::DecodedSwap> {
    let ixs: Vec<DecodedIx> = f
        .instructions
        .iter()
        .map(|i| DecodedIx {
            program_id: i.program_id.clone(),
            data: base64::engine::general_purpose::STANDARD
                .decode(&i.data_b64)
                .expect("b64"),
            accounts: i.accounts.clone(),
            is_inner: i.is_inner,
        })
        .collect();
    let pre: Vec<TokenBalance> = f
        .pre_token_balances
        .iter()
        .map(|b| TokenBalance {
            account_index: b.account_index,
            mint: b.mint.clone(),
            owner: b.owner.clone(),
            amount: b.amount.parse().unwrap_or(0),
            decimals: b.decimals,
        })
        .collect();
    let post: Vec<TokenBalance> = f
        .post_token_balances
        .iter()
        .map(|b| TokenBalance {
            account_index: b.account_index,
            mint: b.mint.clone(),
            owner: b.owner.clone(),
            amount: b.amount.parse().unwrap_or(0),
            decimals: b.decimals,
        })
        .collect();

    let ctx = TxContext {
        signature: &f.signature,
        slot: f.slot,
        signer: &f.signer,
        observed_at_ms: 0,
        account_keys: &f.account_keys,
        instructions: &ixs,
        pre_token_balances: &pre,
        post_token_balances: &post,
        pre_sol: &f.pre_sol,
        post_sol: &f.post_sol,
    };
    decode_transaction(&ctx).expect("decode")
}

#[test]
fn jupiter_buy_sol_to_token() {
    let f = load_frame("jupiter_buy.json");
    let d = run_frame(&f).expect("should decode");
    assert_eq!(d.dex, Dex::Jupiter);
    assert_eq!(d.direction, Direction::Buy);
    assert!(d.input_amount > 0);
    assert!(d.output_amount > 0);
    assert_eq!(d.target_wallet, f.signer);
}

#[test]
fn pumpfun_buy() {
    let f = load_frame("pumpfun_buy.json");
    let d = run_frame(&f).expect("should decode");
    assert_eq!(d.dex, Dex::PumpFun);
    assert_eq!(d.direction, Direction::Buy);
    assert!(d.input_amount > 0);
}

#[test]
fn raydium_amm_sell() {
    let f = load_frame("raydium_amm_sell.json");
    let d = run_frame(&f).expect("should decode");
    assert_eq!(d.dex, Dex::RaydiumAmm);
    assert_eq!(d.direction, Direction::Sell);
}

#[test]
fn non_tracked_program_returns_none() {
    let f = load_frame("unrelated_transfer.json");
    assert!(run_frame(&f).is_none());
}
