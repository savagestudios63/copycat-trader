//! Jupiter swap builder + Jito bundle submission.
//!
//! Flow for a copy buy:
//!   1. GET /v6/quote       — get route quote
//!   2. POST /v6/swap       — get prebuilt transaction (base64)
//!   3. Sign with our keypair, attach compute-budget + tip-transfer ixs
//!   4. Submit as a Jito bundle (or fall back to plain sendTransaction).
//!
//! NOTE ON PRICES: we estimate USD by quoting token→USDC via Jupiter. We
//! cache the last USDC price per SOL for 60 seconds to avoid hammering the
//! API from the sizing hot path.

use crate::config::Config;
use crate::types::mints;
use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::json;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

pub struct Executor {
    cfg: Arc<Config>,
    http: reqwest::Client,
    keypair: Option<Keypair>,
    usd_cache: RwLock<Option<(Instant, f64)>>, // (fetched_at, sol_usd)
    /// Jito tip accounts rotate; we pick one each bundle.
    jito_tip_accounts: Vec<Pubkey>,
}

#[derive(Debug, Clone)]
pub struct ExecuteResult {
    pub signature: String,
    /// Best-effort actual out-amount returned by Jupiter's swap response.
    pub out_amount: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JupQuote {
    #[serde(rename = "inputMint")]
    pub input_mint: String,
    #[serde(rename = "outputMint")]
    pub output_mint: String,
    #[serde(rename = "inAmount")]
    pub in_amount_str: String,
    #[serde(rename = "outAmount")]
    pub out_amount_str: String,
    #[serde(rename = "otherAmountThreshold", default)]
    pub other_amount_threshold: String,
    #[serde(rename = "priceImpactPct", default)]
    pub price_impact_pct: String,
    #[serde(rename = "slippageBps", default)]
    pub slippage_bps: u64,
    #[serde(default)]
    pub swap_mode: Option<String>,
    #[serde(rename = "routePlan", default)]
    pub route_plan: serde_json::Value,
}

impl JupQuote {
    pub fn out_amount(&self) -> u64 {
        self.out_amount_str.parse().unwrap_or(0)
    }
    pub fn in_amount(&self) -> u64 {
        self.in_amount_str.parse().unwrap_or(0)
    }
    pub fn price_impact(&self) -> f64 {
        self.price_impact_pct.parse().unwrap_or(0.0)
    }
}

#[derive(Debug, Clone)]
pub struct QuoteResult {
    pub out_amount: u64,
    pub price_impact_pct: f64,
}

impl Executor {
    pub fn new(cfg: Arc<Config>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .http2_prior_knowledge()
            .pool_max_idle_per_host(16)
            .timeout(Duration::from_secs(10))
            .build()?;
        let keypair = if cfg.executor.keypair_path.is_empty() {
            None
        } else {
            match read_keypair_file(&cfg.executor.keypair_path) {
                Ok(k) => Some(k),
                Err(e) => {
                    warn!(error = ?e, "keypair not loaded — executor in read-only mode");
                    None
                }
            }
        };

        // Well-known Jito tip accounts (rotate across these).
        let jito_tip_accounts = [
            "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
            "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
            "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
            "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
            "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
            "ADuUkR4vqLUMWXxW9gh6D6L8pivKeVULzzefmHt8w9WJ",
            "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
            "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
        ]
        .iter()
        .filter_map(|s| Pubkey::from_str(s).ok())
        .collect::<Vec<_>>();

        Ok(Self {
            cfg,
            http,
            keypair,
            usd_cache: RwLock::new(None),
            jito_tip_accounts,
        })
    }

    fn jup_url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.executor.jupiter_url.trim_end_matches('/'), path)
    }

    fn jup_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.cfg.executor.jupiter_api_key.is_empty() {
            req
        } else {
            req.header("x-api-key", &self.cfg.executor.jupiter_api_key)
        }
    }

    /// GET quote-only. Does not sign or submit.
    pub async fn quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResult> {
        let q = self.get_quote_raw(input_mint, output_mint, amount, slippage_bps).await?;
        Ok(QuoteResult {
            out_amount: q.out_amount(),
            price_impact_pct: q.price_impact(),
        })
    }

    async fn get_quote_raw(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<JupQuote> {
        let url = self.jup_url("/quote");
        let resp = self
            .jup_auth(
                self.http
                    .get(&url)
                    .query(&[
                        ("inputMint", input_mint),
                        ("outputMint", output_mint),
                        ("amount", &amount.to_string()),
                        ("slippageBps", &slippage_bps.to_string()),
                        ("onlyDirectRoutes", "false"),
                        ("asLegacyTransaction", "false"),
                    ]),
            )
            .send()
            .await
            .context("jupiter /quote")?;
        if !resp.status().is_success() {
            return Err(anyhow!("jupiter /quote status {}", resp.status()));
        }
        let q: JupQuote = resp.json().await.context("decode quote")?;
        Ok(q)
    }

    /// Execute a buy: quote → build swap tx → sign → submit.
    pub async fn copy_buy(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<ExecuteResult> {
        self.execute_swap(input_mint, output_mint, amount, slippage_bps).await
    }

    pub async fn copy_sell(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<ExecuteResult> {
        self.execute_swap(input_mint, output_mint, amount, slippage_bps).await
    }

    async fn execute_swap(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<ExecuteResult> {
        let kp = self
            .keypair
            .as_ref()
            .ok_or_else(|| anyhow!("no keypair loaded"))?;
        let user = kp.pubkey();

        let q = self.get_quote_raw(input_mint, output_mint, amount, slippage_bps).await?;
        let out_amount = q.out_amount();

        let swap_url = self.jup_url("/swap");
        let body = json!({
            "quoteResponse": q,
            "userPublicKey": user.to_string(),
            "wrapAndUnwrapSol": true,
            "useSharedAccounts": true,
            "computeUnitPriceMicroLamports": self.cfg.executor.priority_fee_microlamports,
            "dynamicComputeUnitLimit": false,
            "prioritizationFeeLamports": "auto",
            "asLegacyTransaction": false,
        });
        let resp = self
            .jup_auth(self.http.post(&swap_url).json(&body))
            .send()
            .await
            .context("jupiter /swap")?;
        if !resp.status().is_success() {
            let s = resp.status();
            let t = resp.text().await.unwrap_or_default();
            return Err(anyhow!("jupiter /swap {}: {}", s, t));
        }
        #[derive(Deserialize)]
        struct SwapResp {
            #[serde(rename = "swapTransaction")]
            swap_tx: String,
        }
        let sr: SwapResp = resp.json().await?;
        let tx_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, sr.swap_tx)
            .context("decode swap_tx base64")?;
        let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes).context("bincode tx")?;

        // Re-sign with our keypair (Jupiter returns a tx with message signed
        // by no one; the `userPublicKey` is the fee payer and only signer).
        tx.signatures[0] = kp.sign_message(&tx.message.serialize());

        // Build a Jito bundle: [user swap tx] + [tip transfer tx]. If Jito
        // isn't configured, fall back to a direct sendTransaction via RPC.
        let sig = if !self.cfg.executor.jito_block_engine.is_empty()
            && self.cfg.executor.jito_tip_lamports > 0
        {
            self.submit_jito_bundle(&tx, kp).await?
        } else {
            self.submit_plain(&tx).await?
        };

        Ok(ExecuteResult {
            signature: sig,
            out_amount,
        })
    }

    async fn submit_plain(&self, tx: &VersionedTransaction) -> Result<String> {
        let url = self.cfg.rpc.http_url.clone();
        let tx_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bincode::serialize(tx)?);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [tx_b64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]
        });
        let resp = self.http.post(url).json(&body).send().await?;
        let v: serde_json::Value = resp.json().await?;
        if let Some(err) = v.get("error") {
            return Err(anyhow!("sendTransaction: {}", err));
        }
        let sig = v
            .get("result")
            .and_then(|r| r.as_str())
            .ok_or_else(|| anyhow!("no signature"))?
            .to_string();
        Ok(sig)
    }

    async fn submit_jito_bundle(
        &self,
        swap_tx: &VersionedTransaction,
        kp: &Keypair,
    ) -> Result<String> {
        // Build tip transfer tx as a separate signed message — this is the
        // conventional Jito bundle shape.
        let tip_ix = system_instruction::transfer(
            &kp.pubkey(),
            self.pick_tip_account(),
            self.cfg.executor.jito_tip_lamports,
        );
        let cu_price = ComputeBudgetInstruction::set_compute_unit_price(
            self.cfg.executor.priority_fee_microlamports,
        );
        let cu_limit =
            ComputeBudgetInstruction::set_compute_unit_limit(self.cfg.executor.compute_unit_limit);

        let recent = self.get_blockhash().await?;
        let tip_msg = solana_sdk::message::Message::new_with_blockhash(
            &[cu_price, cu_limit, tip_ix],
            Some(&kp.pubkey()),
            &recent,
        );
        let tip_tx = solana_sdk::transaction::Transaction::new(&[kp], tip_msg, recent);

        // Serialize both to base64.
        let swap_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            bincode::serialize(swap_tx)?,
        );
        let tip_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            bincode::serialize(&tip_tx)?,
        );

        let endpoint = format!(
            "{}/api/v1/bundles",
            self.cfg.executor.jito_block_engine.trim_end_matches('/')
        );
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [[swap_b64, tip_b64], {"encoding": "base64"}]
        });
        let mut req = self.http.post(&endpoint).json(&body);
        if !self.cfg.executor.jito_uuid.is_empty() {
            req = req.header("x-jito-auth", &self.cfg.executor.jito_uuid);
        }
        let resp = req.send().await.context("jito sendBundle")?;
        let v: serde_json::Value = resp.json().await?;
        if let Some(err) = v.get("error") {
            return Err(anyhow!("jito sendBundle: {}", err));
        }
        // Jito returns bundleId, not signature. The swap tx's own signature
        // is authoritative on-chain.
        Ok(bs58::encode(swap_tx.signatures[0].as_ref()).into_string())
    }

    fn pick_tip_account(&self) -> &Pubkey {
        let idx = (chrono::Utc::now().timestamp_subsec_nanos() as usize) % self.jito_tip_accounts.len();
        &self.jito_tip_accounts[idx]
    }

    async fn get_blockhash(&self) -> Result<solana_sdk::hash::Hash> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLatestBlockhash",
            "params": [{"commitment": self.cfg.rpc.commitment}]
        });
        let resp = self.http.post(&self.cfg.rpc.http_url).json(&body).send().await?;
        let v: serde_json::Value = resp.json().await?;
        let bh = v["result"]["value"]["blockhash"]
            .as_str()
            .ok_or_else(|| anyhow!("no blockhash"))?;
        let bytes = bs58::decode(bh).into_vec()?;
        Ok(solana_sdk::hash::Hash::new(&bytes))
    }

    /// Approximate USD value of an `amount` of `mint`. Uses cached SOL/USD
    /// price (refreshed every 60s via Jupiter quote) for wSOL; for USDC/USDT
    /// this is trivial; for anything else we route-quote to USDC.
    pub async fn estimate_input_usd(&self, mint: &str, amount: u64) -> Result<f64> {
        match mint {
            mints::USDC | mints::USDT => Ok((amount as f64) / 1e6),
            mints::WSOL => {
                let sol_usd = self.sol_usd().await.unwrap_or(0.0);
                Ok((amount as f64 / 1e9) * sol_usd)
            }
            _ => {
                // Quote `amount` of mint → USDC.
                match self.get_quote_raw(mint, mints::USDC, amount, 500).await {
                    Ok(q) => Ok((q.out_amount() as f64) / 1e6),
                    Err(e) => {
                        debug!(error = ?e, "estimate_input_usd quote failed");
                        Ok(0.0)
                    }
                }
            }
        }
    }

    async fn sol_usd(&self) -> Result<f64> {
        if let Some((when, px)) = *self.usd_cache.read() {
            if when.elapsed() < Duration::from_secs(60) {
                return Ok(px);
            }
        }
        let q = self
            .get_quote_raw(mints::WSOL, mints::USDC, 1_000_000_000, 500)
            .await?;
        let px = (q.out_amount() as f64) / 1e6;
        *self.usd_cache.write() = Some((Instant::now(), px));
        Ok(px)
    }

    pub fn jupiter_http(&self) -> &reqwest::Client {
        &self.http
    }
}
