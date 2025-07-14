//! Yellowstone Geyser gRPC subscription + decode pipeline.
//!
//! Each target wallet becomes an `accountInclude` transaction filter entry.
//! On every frame we:
//!   1. Construct a TxContext from the Geyser tx message.
//!   2. Run the decoder cascade.
//!   3. Push DecodedSwap into the engine channel.
//!
//! Reconnects with exponential backoff; a missed frame is better than a
//! crashed daemon.

use crate::config::Config;
use crate::decoder::{self, DecodedIx, TokenBalance, TxContext};
use crate::types::DecodedSwap;
use anyhow::{anyhow, Context, Result};
use futures::{sink::SinkExt, stream::StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterTransactions, SubscribeRequestPing,
};

pub async fn run(
    cfg: Arc<Config>,
    out: mpsc::Sender<DecodedSwap>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    let mut backoff_ms: u64 = cfg.geyser.reconnect_ms;
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                info!("geyser shutdown");
                return Ok(());
            }
            res = connect_and_stream(&cfg, &out, shutdown.resubscribe()) => {
                match res {
                    Ok(()) => {
                        info!("geyser stream closed cleanly — reconnecting");
                        backoff_ms = cfg.geyser.reconnect_ms;
                    }
                    Err(e) => {
                        error!(error = ?e, "geyser stream error — reconnecting in {} ms", backoff_ms);
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
                    }
                }
            }
        }
    }
}

async fn connect_and_stream(
    cfg: &Config,
    out: &mpsc::Sender<DecodedSwap>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    let x_token = if cfg.geyser.x_token.is_empty() {
        None
    } else {
        Some(cfg.geyser.x_token.clone())
    };

    let mut client = GeyserGrpcClient::build_from_shared(cfg.geyser.endpoint.clone())?
        .x_token(x_token)?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .tls_config(yellowstone_grpc_client::ClientTlsConfig::new().with_native_roots())?
        .connect()
        .await
        .context("geyser connect")?;

    let (mut tx, mut stream) = client.subscribe().await.context("geyser subscribe")?;
    let req = build_request(cfg);
    tx.send(req).await.context("send subscribe req")?;
    info!(
        endpoint = %cfg.geyser.endpoint,
        wallets = cfg.wallets.iter().filter(|w| w.enabled).count(),
        "geyser subscribed"
    );

    // Ping loop.
    let ping_tx = tokio::sync::Mutex::new(tx);
    let ping_ms = cfg.geyser.ping_ms;
    let ping_task = {
        let ping_tx = &ping_tx;
        async move {
            let mut i = 0i32;
            loop {
                tokio::time::sleep(Duration::from_millis(ping_ms)).await;
                i = i.wrapping_add(1);
                let mut g = ping_tx.lock().await;
                if let Err(e) = g
                    .send(SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: i }),
                        ..Default::default()
                    })
                    .await
                {
                    warn!(error = ?e, "ping send failed");
                    break;
                }
            }
        }
    };
    tokio::pin!(ping_task);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => return Ok(()),
            _ = &mut ping_task => return Err(anyhow!("ping loop died")),
            msg = stream.next() => {
                let Some(msg) = msg else { return Ok(()); };
                let upd = msg.context("stream recv")?;
                if let Some(UpdateOneof::Transaction(txu)) = upd.update_oneof {
                    if let Err(e) = handle_tx(cfg, &txu, out).await {
                        debug!(error = ?e, "skip tx");
                    }
                }
            }
        }
    }
}

fn build_request(cfg: &Config) -> SubscribeRequest {
    let mut transactions = HashMap::new();
    let accounts: Vec<String> = cfg
        .wallets
        .iter()
        .filter(|w| w.enabled)
        .map(|w| w.address.clone())
        .collect();
    transactions.insert(
        "copycat-targets".into(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            signature: None,
            account_include: accounts,
            account_exclude: vec![],
            account_required: vec![],
        },
    );
    let commitment = match cfg.rpc.commitment.as_str() {
        "finalized" => CommitmentLevel::Finalized,
        "confirmed" => CommitmentLevel::Confirmed,
        _ => CommitmentLevel::Processed,
    };
    SubscribeRequest {
        accounts: HashMap::new(),
        slots: HashMap::new(),
        transactions,
        transactions_status: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        entry: HashMap::new(),
        commitment: Some(commitment as i32),
        accounts_data_slice: vec![],
        ping: None,
        from_slot: None,
    }
}

async fn handle_tx(
    cfg: &Config,
    txu: &yellowstone_grpc_proto::prelude::SubscribeUpdateTransaction,
    out: &mpsc::Sender<DecodedSwap>,
) -> Result<()> {
    let info = txu.transaction.as_ref().ok_or_else(|| anyhow!("no tx"))?;
    let meta = info.meta.as_ref().ok_or_else(|| anyhow!("no meta"))?;

    // Skip failed tx just in case filter didn't
    if meta.err.is_some() {
        return Ok(());
    }

    let tx = info.transaction.as_ref().ok_or_else(|| anyhow!("no inner tx"))?;
    let msg = tx.message.as_ref().ok_or_else(|| anyhow!("no msg"))?;

    // Account keys = static + loaded writable + loaded readonly (ALT)
    let mut account_keys: Vec<String> = msg
        .account_keys
        .iter()
        .map(|k| bs58::encode(k).into_string())
        .collect();
    for w in &meta.loaded_writable_addresses {
        account_keys.push(bs58::encode(w).into_string());
    }
    for r in &meta.loaded_readonly_addresses {
        account_keys.push(bs58::encode(r).into_string());
    }

    // Signer = first account key.
    let signer = account_keys.first().cloned().unwrap_or_default();

    // Only dispatch if the signer is one of our tracked wallets.
    let Some(_wallet_cfg) = cfg.wallet_by_address(&signer) else {
        return Ok(());
    };

    // Build DecodedIx list (top-level + inner).
    let mut ixs: Vec<DecodedIx> = msg
        .instructions
        .iter()
        .map(|ci| DecodedIx {
            program_id: account_keys
                .get(ci.program_id_index as usize)
                .cloned()
                .unwrap_or_default(),
            data: ci.data.clone(),
            accounts: ci.accounts.clone(),
            is_inner: false,
        })
        .collect();
    for inner in &meta.inner_instructions {
        for ci in &inner.instructions {
            ixs.push(DecodedIx {
                program_id: account_keys
                    .get(ci.program_id_index as usize)
                    .cloned()
                    .unwrap_or_default(),
                data: ci.data.clone(),
                accounts: ci.accounts.clone(),
                is_inner: true,
            });
        }
    }

    let pre_tb: Vec<TokenBalance> = meta
        .pre_token_balances
        .iter()
        .filter_map(|b| {
            let ui = b.ui_token_amount.as_ref()?;
            Some(TokenBalance {
                account_index: b.account_index as u8,
                mint: b.mint.clone(),
                owner: b.owner.clone(),
                amount: ui.amount.parse::<u64>().ok()?,
                decimals: ui.decimals as u8,
            })
        })
        .collect();
    let post_tb: Vec<TokenBalance> = meta
        .post_token_balances
        .iter()
        .filter_map(|b| {
            let ui = b.ui_token_amount.as_ref()?;
            Some(TokenBalance {
                account_index: b.account_index as u8,
                mint: b.mint.clone(),
                owner: b.owner.clone(),
                amount: ui.amount.parse::<u64>().ok()?,
                decimals: ui.decimals as u8,
            })
        })
        .collect();

    let sig_bytes = info.signature.clone();
    let signature = bs58::encode(sig_bytes).into_string();
    let observed_at_ms = chrono::Utc::now().timestamp_millis();

    let ctx = TxContext {
        signature: &signature,
        slot: txu.slot,
        signer: &signer,
        observed_at_ms,
        account_keys: &account_keys,
        instructions: &ixs,
        pre_token_balances: &pre_tb,
        post_token_balances: &post_tb,
        pre_sol: &meta.pre_balances,
        post_sol: &meta.post_balances,
    };

    if let Some(decoded) = decoder::decode_transaction(&ctx)? {
        if let Err(e) = out.send(decoded).await {
            warn!(error = ?e, "engine channel closed");
        }
    }
    Ok(())
}
