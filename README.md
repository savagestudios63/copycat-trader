# copycat-trader

A Solana copy-trading daemon written in Rust. Subscribes to a Yellowstone
Geyser gRPC stream filtered on a configurable list of target wallets, decodes
their swaps across Jupiter / Raydium (AMM + CLMM) / Orca Whirlpools /
Meteora DLMM / Pump.fun, and mirrors each buy via the Jupiter swap API —
landed on-chain through a Jito bundle with a tip transfer.

```
geyser (yellowstone gRPC)
     │ filtered tx frames (account_include = targets)
     ▼
decoder cascade ──► DecodedSwap ──► engine
                                      │ sizing, cooldown, blocklist
                                      │ risk::pre_buy_check
                                      ▼
                                  executor (Jupiter /quote + /swap)
                                      │ sign + compute-budget + tip
                                      ▼
                                  Jito block-engine bundle
                                      │
                                      ▼
                                  SQLite positions + trade log
                                      │
                                      ▼
                                  ratatui dashboard
```

## Quickstart

```bash
export GEYSER_X_TOKEN=...
export COPYCAT_KEYPAIR_PATH=/home/trader/keys/signer.json
export JITO_UUID=...        # optional
export JUPITER_API_KEY=...  # optional

cargo run --release --bin copycat -- --config config.toml
# or headless:
cargo run --release --bin copycat -- --config config.toml --headless
# or dry-run (decode + log, never sign/submit):
cargo run --release --bin copycat -- --config config.toml --dry-run
```

Tests (offline, no network):

```bash
cargo test --all
```

## Infrastructure requirements

### RPC & Geyser

This is a latency-sensitive trading bot. Every millisecond between the block
producer and your bundle submission is a millisecond of adverse selection.

- **Yellowstone Geyser gRPC** (required, hot path). Providers tested:
  - [Helius](https://helius.xyz) — regions: `ams`, `fra`, `nyc`
  - [Triton One](https://triton.one) — regions: `Frankfurt`, `NY`, `Tokyo`
  - [shyft.to](https://shyft.to)
  - self-hosted behind your own validator (best, if you run one)
- **Commitment = processed** is the intended mode. `confirmed` adds ~1 slot
  of latency. `finalized` is unusable for copy-trading.
- **Standard JSON-RPC** — only used for blockhashes, `sendTransaction`
  fallback, and `getAccountInfo` in the risk module. A plain Helius / Triton
  RPC endpoint is sufficient.
- **Jito block-engine**. Colocate with the region you're sending from.
  Default tip = 100,000 lamports (~$0.015). Raise this aggressively in
  congested mempool conditions.
- **Jupiter swap API**. The public endpoint rate-limits; for production,
  either use the `jupiter_api_key` tier or self-host the swap API.

### Recommended hardware

This bot is I/O-bound, not CPU-bound. Spend the budget on network, not cores.

| resource     | minimum                              | recommended                     |
| ------------ | ------------------------------------ | ------------------------------- |
| vCPU         | 4                                    | 8 (same-DC colo w/ provider)    |
| RAM          | 4 GB                                 | 8 GB                            |
| Disk         | 10 GB SSD (SQLite + logs)            | 40 GB NVMe                      |
| Network      | 100 Mbps, < 5 ms RTT to gRPC edge    | 1 Gbps, < 1 ms (same AZ)        |
| OS           | Linux 5.15+ (Ubuntu 22.04 / Debian 12)|                                |

Run under `systemd` with `Restart=always` and `LimitNOFILE=65535`. Pin
the daemon to performance cores; disable CPU frequency scaling.

## Architecture

| file                         | responsibility                                                  |
| ---------------------------- | --------------------------------------------------------------- |
| `src/geyser.rs`              | Yellowstone gRPC subscribe + reconnect + frame → `TxContext`    |
| `src/decoder/mod.rs`         | Cascade + `infer_from_balance_deltas` fallback                  |
| `src/decoder/jupiter.rs`     | Jupiter v4/v6 (aggregator — balance-delta driven)               |
| `src/decoder/raydium.rs`     | Raydium AMM v4 + CLMM (`swap_base_in/out`, `swap_v2`)           |
| `src/decoder/orca.rs`        | Orca Whirlpool (`swap`, `swap_v2`, a→b flag)                    |
| `src/decoder/meteora.rs`     | Meteora DLMM (`swap`, `swap_exact_out`)                         |
| `src/decoder/pumpfun.rs`     | Pump.fun bonding-curve (`buy`, `sell` discriminators)           |
| `src/engine.rs`              | Copy logic, cooldown, position tracker, TP/SL pass              |
| `src/executor.rs`            | Jupiter /quote + /swap, tx signing, Jito bundle                 |
| `src/risk.rs`                | Mint authority check, honeypot round-trip, sizing               |
| `src/db.rs`                  | SQLite via sqlx (positions, trades, per-wallet PnL)             |
| `src/tui.rs`                 | ratatui dashboard                                               |
| `src/config.rs`              | TOML config + `${ENV}` interpolation                            |

## Configuration

See [`config.toml`](./config.toml) — every field is documented inline. Target
wallets live in `[[wallets]]` blocks; each supports:

- `sizing_mode` — `fixed_sol` / `fixed_usd` / `pct_of_target`
- `sizing_value` — interpreted per mode
- `max_position_usd` — hard cap per copy
- `min_target_trade_usd` — ignore dust trades
- `cooldown_ms` — rate-limit per wallet
- `max_slippage_bps` — per-wallet slippage override
- `exit_strategy` — `mirror` / `tp_sl` / `mirror_then_tp_sl`
- `take_profit_pct` / `stop_loss_pct`
- `blocked_tokens` — never copy-buy these

## Risk controls

Before every copy buy:

1. The output mint is not on the wallet's blocklist.
2. `getAccountInfo(encoding=jsonParsed)` on the SPL mint — reject if
   `mintAuthority` / `freezeAuthority` is non-null (config-gated).
3. **Round-trip honeypot sim** — 0.1 SOL probe quote in, quote that exact
   output amount back out, reject if round-trip loss > `max_roundtrip_impact_bps`.
4. The configured per-position USD cap is enforced after Jupiter quotes the
   actual input→output conversion.

## Tests

Integration tests replay recorded Geyser-shaped transaction frames (see
`tests/fixtures/*.json`) through the decoder pipeline and assert on the
produced `DecodedSwap` (DEX, direction, amounts). To add a new fixture:

1. Capture a real tx via `geyser.rs` in logging mode (or with `--dry-run`).
2. Serialize the `TxContext` fields into the schema documented at the top of
   `tests/decoder_replay.rs`.
3. Drop it in `tests/fixtures/` and add an assertion.

Unit tests in `db.rs`, `config.rs`, and `engine.rs` cover migrations, env
interpolation, and pure sizing arithmetic respectively.

## Operational notes

- **Never** run without `--dry-run` first on a new wallet set. Verify the
  feed looks sane for 30 minutes before letting it sign.
- Use a **dedicated** signer. Do not reuse a wallet with other balances.
- SQLite uses WAL mode — back up `copycat.db` with `.backup`, not `cp`, while
  the daemon is running.
- `SIGINT` triggers a clean shutdown; in-flight bundles are **not** recalled
  (they're already at Jito).

## License

MIT.
