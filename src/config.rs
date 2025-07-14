//! Config loading with env var interpolation (`${VAR_NAME}`).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub rpc: RpcCfg,
    pub geyser: GeyserCfg,
    pub executor: ExecutorCfg,
    pub risk: RiskCfg,
    pub db: DbCfg,
    pub tui: TuiCfg,
    #[serde(default, rename = "wallets")]
    pub wallets: Vec<WalletCfg>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcCfg {
    pub http_url: String,
    pub ws_url: String,
    #[serde(default = "default_commitment")]
    pub commitment: String,
}
fn default_commitment() -> String { "processed".into() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeyserCfg {
    pub endpoint: String,
    #[serde(default)]
    pub x_token: String,
    #[serde(default = "default_reconnect")]
    pub reconnect_ms: u64,
    #[serde(default = "default_ping")]
    pub ping_ms: u64,
}
fn default_reconnect() -> u64 { 1500 }
fn default_ping() -> u64 { 10_000 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecutorCfg {
    pub keypair_path: String,
    pub jupiter_url: String,
    #[serde(default)]
    pub jupiter_api_key: String,
    pub jito_block_engine: String,
    #[serde(default = "default_tip")]
    pub jito_tip_lamports: u64,
    #[serde(default)]
    pub jito_uuid: String,
    #[serde(default = "default_slippage")]
    pub default_slippage_bps: u16,
    #[serde(default = "default_priority_fee")]
    pub priority_fee_microlamports: u64,
    #[serde(default = "default_cu_limit")]
    pub compute_unit_limit: u32,
}
fn default_tip() -> u64 { 100_000 }
fn default_slippage() -> u16 { 150 }
fn default_priority_fee() -> u64 { 500_000 }
fn default_cu_limit() -> u32 { 600_000 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RiskCfg {
    #[serde(default = "t")]
    pub simulate_sell_before_buy: bool,
    #[serde(default = "default_max_impact")]
    pub max_roundtrip_impact_bps: u16,
    #[serde(default = "t")]
    pub require_mint_authority_null: bool,
    #[serde(default = "t")]
    pub require_freeze_authority_null: bool,
    #[serde(default)]
    pub blocked_programs: Vec<String>,
}
fn t() -> bool { true }
fn default_max_impact() -> u16 { 2000 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DbCfg {
    pub url: String,
    #[serde(default = "default_busy")]
    pub busy_timeout_ms: u64,
}
fn default_busy() -> u64 { 5_000 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TuiCfg {
    #[serde(default = "t")]
    pub enabled: bool,
    #[serde(default = "default_refresh")]
    pub refresh_hz: u32,
}
fn default_refresh() -> u32 { 10 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalletCfg {
    pub name: String,
    pub address: String,
    #[serde(default = "t")]
    pub enabled: bool,
    pub sizing_mode: SizingMode,
    pub sizing_value: f64,
    pub max_position_usd: f64,
    #[serde(default)]
    pub min_target_trade_usd: f64,
    #[serde(default)]
    pub cooldown_ms: u64,
    #[serde(default)]
    pub max_slippage_bps: u16,
    #[serde(default = "default_exit")]
    pub exit_strategy: ExitStrategy,
    #[serde(default)]
    pub take_profit_pct: f64,
    #[serde(default)]
    pub stop_loss_pct: f64,
    #[serde(default)]
    pub blocked_tokens: Vec<String>,
}
fn default_exit() -> ExitStrategy { ExitStrategy::Mirror }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SizingMode {
    FixedSol,
    FixedUsd,
    PctOfTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStrategy {
    Mirror,
    TpSl,
    MirrorThenTpSl,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let expanded = expand_env(&raw)?;
        let cfg: Config = toml::from_str(&expanded).context("parsing toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.wallets.is_empty() {
            return Err(anyhow!("config has no [[wallets]] entries"));
        }
        for w in &self.wallets {
            if w.sizing_value <= 0.0 {
                return Err(anyhow!("wallet {}: sizing_value must be > 0", w.name));
            }
            if w.address.parse::<solana_sdk::pubkey::Pubkey>().is_err() {
                return Err(anyhow!("wallet {}: invalid address {}", w.name, w.address));
            }
        }
        Ok(())
    }

    /// Returns the config for a target wallet address.
    pub fn wallet_by_address(&self, addr: &str) -> Option<&WalletCfg> {
        self.wallets.iter().find(|w| w.address == addr && w.enabled)
    }
}

/// Expand `${VAR}` occurrences from environment. Leaves unset vars as empty
/// strings so optional auth fields stay valid TOML.
fn expand_env(input: &str) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| anyhow!("unterminated ${{..}} in config"))?;
        let var = &after[..end];
        let val = std::env::var(var).unwrap_or_default();
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_env_vars() {
        std::env::set_var("FOO", "bar");
        let s = expand_env("x=${FOO};y=${UNSET_VAR}").unwrap();
        assert_eq!(s, "x=bar;y=");
    }
}
