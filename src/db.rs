//! SQLite persistence — open positions, trade history, per-wallet PnL.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Pool, Sqlite};
use std::str::FromStr;
use std::time::Duration;

#[derive(Clone)]
pub struct Db {
    pool: Pool<Sqlite>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Position {
    pub id: i64,
    pub target_wallet: String,
    pub mint: String,
    pub entry_signature: String,
    pub entry_input_mint: String,
    pub entry_input_amount: i64,
    pub entry_output_amount: i64,
    pub entry_price_usd: f64,
    pub entry_slot: i64,
    pub opened_at: DateTime<Utc>,
    /// None => open; Some => closed
    pub closed_at: Option<DateTime<Utc>>,
    pub exit_signature: Option<String>,
    pub exit_output_amount: Option<i64>,
    pub realized_pnl_usd: Option<f64>,
    pub tp_pct: Option<f64>,
    pub sl_pct: Option<f64>,
    pub exit_strategy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TradeRow {
    pub id: i64,
    pub signature: String,
    pub target_wallet: String,
    pub side: String, // "copy_buy" | "copy_sell" | "target_buy" | "target_sell"
    pub dex: String,
    pub input_mint: String,
    pub input_amount: i64,
    pub output_mint: String,
    pub output_amount: i64,
    pub slot: i64,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct WalletPnl {
    pub target_wallet: String,
    pub trades: i64,
    pub realized_pnl_usd: f64,
    pub open_positions: i64,
}

impl Db {
    pub async fn open(url: &str, busy_timeout_ms: u64) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .busy_timeout(Duration::from_millis(busy_timeout_ms))
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::query(SCHEMA).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn insert_trade(&self, t: &TradeRow) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO trades
             (signature, target_wallet, side, dex, input_mint, input_amount,
              output_mint, output_amount, slot, ts)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(&t.signature)
        .bind(&t.target_wallet)
        .bind(&t.side)
        .bind(&t.dex)
        .bind(&t.input_mint)
        .bind(t.input_amount)
        .bind(&t.output_mint)
        .bind(t.output_amount)
        .bind(t.slot)
        .bind(t.ts)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn open_position(&self, p: &Position) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO positions
             (target_wallet, mint, entry_signature, entry_input_mint,
              entry_input_amount, entry_output_amount, entry_price_usd,
              entry_slot, opened_at, tp_pct, sl_pct, exit_strategy)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(&p.target_wallet)
        .bind(&p.mint)
        .bind(&p.entry_signature)
        .bind(&p.entry_input_mint)
        .bind(p.entry_input_amount)
        .bind(p.entry_output_amount)
        .bind(p.entry_price_usd)
        .bind(p.entry_slot)
        .bind(p.opened_at)
        .bind(p.tp_pct)
        .bind(p.sl_pct)
        .bind(&p.exit_strategy)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn close_position(
        &self,
        id: i64,
        exit_sig: &str,
        exit_output_amount: i64,
        realized_pnl_usd: f64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE positions
             SET closed_at = ?, exit_signature = ?,
                 exit_output_amount = ?, realized_pnl_usd = ?
             WHERE id = ?",
        )
        .bind(Utc::now())
        .bind(exit_sig)
        .bind(exit_output_amount)
        .bind(realized_pnl_usd)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn open_positions(&self) -> Result<Vec<Position>> {
        Ok(sqlx::query_as::<_, Position>(
            "SELECT * FROM positions WHERE closed_at IS NULL ORDER BY opened_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn open_positions_for_wallet(&self, wallet: &str) -> Result<Vec<Position>> {
        Ok(sqlx::query_as::<_, Position>(
            "SELECT * FROM positions
             WHERE closed_at IS NULL AND target_wallet = ?
             ORDER BY opened_at DESC",
        )
        .bind(wallet)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn wallet_pnl(&self) -> Result<Vec<WalletPnl>> {
        Ok(sqlx::query_as::<_, WalletPnl>(
            "SELECT
               p.target_wallet AS target_wallet,
               COUNT(*) AS trades,
               COALESCE(SUM(p.realized_pnl_usd), 0.0) AS realized_pnl_usd,
               SUM(CASE WHEN p.closed_at IS NULL THEN 1 ELSE 0 END) AS open_positions
             FROM positions p
             GROUP BY p.target_wallet
             ORDER BY realized_pnl_usd DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn recent_trades(&self, limit: i64) -> Result<Vec<TradeRow>> {
        Ok(sqlx::query_as::<_, TradeRow>(
            "SELECT * FROM trades ORDER BY ts DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS trades (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    signature        TEXT NOT NULL,
    target_wallet    TEXT NOT NULL,
    side             TEXT NOT NULL,
    dex              TEXT NOT NULL,
    input_mint       TEXT NOT NULL,
    input_amount     INTEGER NOT NULL,
    output_mint      TEXT NOT NULL,
    output_amount    INTEGER NOT NULL,
    slot             INTEGER NOT NULL,
    ts               TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_trades_wallet_ts ON trades(target_wallet, ts);
CREATE INDEX IF NOT EXISTS ix_trades_sig       ON trades(signature);

CREATE TABLE IF NOT EXISTS positions (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    target_wallet        TEXT NOT NULL,
    mint                 TEXT NOT NULL,
    entry_signature      TEXT NOT NULL,
    entry_input_mint     TEXT NOT NULL,
    entry_input_amount   INTEGER NOT NULL,
    entry_output_amount  INTEGER NOT NULL,
    entry_price_usd      REAL NOT NULL,
    entry_slot           INTEGER NOT NULL,
    opened_at            TEXT NOT NULL,
    closed_at            TEXT,
    exit_signature       TEXT,
    exit_output_amount   INTEGER,
    realized_pnl_usd     REAL,
    tp_pct               REAL,
    sl_pct               REAL,
    exit_strategy        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_positions_open
    ON positions(target_wallet, mint, closed_at);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrate_and_roundtrip() {
        let db = Db::open("sqlite::memory:", 1000).await.unwrap();
        db.migrate().await.unwrap();

        let t = TradeRow {
            id: 0,
            signature: "sig1".into(),
            target_wallet: "wallet1".into(),
            side: "target_buy".into(),
            dex: "jupiter".into(),
            input_mint: "sol".into(),
            input_amount: 1_000_000_000,
            output_mint: "tok".into(),
            output_amount: 500_000_000,
            slot: 42,
            ts: Utc::now(),
        };
        let id = db.insert_trade(&t).await.unwrap();
        assert!(id > 0);
        let rows = db.recent_trades(10).await.unwrap();
        assert_eq!(rows.len(), 1);
    }
}
