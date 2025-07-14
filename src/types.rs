//! Core types shared across the pipeline.

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

/// Which DEX the swap originated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dex {
    Jupiter,
    RaydiumAmm,
    RaydiumClmm,
    OrcaWhirlpool,
    MeteoraDlmm,
    PumpFun,
    Unknown,
}

impl Dex {
    pub fn as_str(&self) -> &'static str {
        match self {
            Dex::Jupiter => "jupiter",
            Dex::RaydiumAmm => "raydium_amm",
            Dex::RaydiumClmm => "raydium_clmm",
            Dex::OrcaWhirlpool => "orca_whirlpool",
            Dex::MeteoraDlmm => "meteora_dlmm",
            Dex::PumpFun => "pumpfun",
            Dex::Unknown => "unknown",
        }
    }
}

/// Direction of the copy candidate relative to SOL/USDC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Buy,
    Sell,
    Unknown,
}

/// A decoded swap observed on-chain. Produced by the decoder pipeline and
/// fed to the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedSwap {
    pub signature: String,
    pub slot: u64,
    /// The tracked wallet that authored this swap.
    pub target_wallet: String,
    pub dex: Dex,
    pub direction: Direction,

    /// The token *consumed* by the target.
    pub input_mint: String,
    pub input_amount: u64,
    /// The token *received* by the target.
    pub output_mint: String,
    pub output_amount: u64,

    /// Optional: raw program ID of the outermost swap instruction.
    pub program_id: String,
    /// UNIX timestamp millis when the frame arrived.
    pub observed_at_ms: i64,
}

/// Events surfaced to the TUI.
#[derive(Debug, Clone)]
pub enum UiEvent {
    TargetTrade(DecodedSwap),
    CopyBuySubmitted {
        target_wallet: String,
        signature: String,
        input_mint: String,
        input_amount: u64,
        output_mint: String,
    },
    CopySellSubmitted {
        target_wallet: String,
        signature: String,
        mint: String,
    },
    CopyRejected {
        target_wallet: String,
        reason: String,
    },
    PositionClosed {
        mint: String,
        realized_pnl_usd: f64,
    },
    Log(String),
}

/// Well-known mints.
pub mod mints {
    pub const WSOL: &str = "So11111111111111111111111111111111111111112";
    pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    pub const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

    pub fn is_quote(mint: &str) -> bool {
        matches!(mint, WSOL | USDC | USDT)
    }
}

/// Well-known program IDs (base58).
pub mod programs {
    pub const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
    pub const JUPITER_V4: &str = "JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB";
    pub const RAYDIUM_AMM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
    pub const RAYDIUM_CLMM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
    pub const ORCA_WHIRLPOOL: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
    pub const METEORA_DLMM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
    pub const PUMPFUN: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

    pub fn all() -> &'static [&'static str] {
        &[
            JUPITER_V6,
            JUPITER_V4,
            RAYDIUM_AMM_V4,
            RAYDIUM_CLMM,
            ORCA_WHIRLPOOL,
            METEORA_DLMM,
            PUMPFUN,
        ]
    }
}

/// Parse a base58 string into a Pubkey, returning None on error.
pub fn parse_pubkey(s: &str) -> Option<Pubkey> {
    s.parse().ok()
}
