# rgb-arkade

RGB assets on [Arkade](https://github.com/arkade-os/arkd): a working regtest implementation that lets RGB assets live inside Ark VTXOs. Assets can enter Arkade, move off-chain with instant preconfirmed transactions, be refreshed into new batches, leave on-chain (cooperatively or unilaterally), and survive long offline periods. Receivers verify everything client-side, without trusting or even contacting the Ark operator.

It is made of small patches to arkd, the Ark rust-sdk and rgb-lib (KaleidoSwap fork), plus a demo client (`ark-rgb-demo`) that runs every scenario end to end on a local regtest stack.

> Status: research prototype, regtest only. Not production ready; see [Limitations](#status-and-limitations) and [Security model](#security-model).

## Contents

- [Architecture](#architecture)
- [How it works](#how-it-works)
- [Quickstart](#quickstart)
- [Prototype app](#prototype-app)
- [Scenarios and test suite](#scenarios-and-test-suite)
- [Status and limitations](#status-and-limitations)
- [Security model](#security-model)
- [Roadmap](#roadmap)
- [Repository layout](#repository-layout)
- [Contributing](#contributing)
- [License](#license)

Further reading: [docs/design.md](docs/design.md) (full design, atomic swap theory, open questions for Ark Labs), [docs/test-results.md](docs/test-results.md) (measured results, txids, payload sizes), [PATCHES.md](PATCHES.md) (every upstream change). An Italian, non-technical write-up is in the Page ["RGB su Arkade"](https://cursor.com/canvas/store-e5dae6c1-22ce-4fc9-be4f-958a0cdf37c7/d7de931f-98e3-4a02-af31-8ccd68868a97).

## Architecture

| Component | Where | Role |
|---|---|---|
| arkd patches | `patches/arkd.patch` on arkd `v0.9.15`, image `arkd:rgb` (`arkd-image/`) | Opt-in user `OP_RETURN` (the RGB opret commitment) in checkpoint and forfeit txs, reproduced by arkd's deterministic rebuild; 0-sat opret treated as data; operator co-signs stored forfeits; forfeit-based recovery of swept VTXOs. |
| rust-sdk patches | `patches/rust-sdk.patch` on rust-sdk `v0.12.0` | Client side of the above: checkpoint payload, forfeit hooks for refresh / collaborative exit / recovery, exit hooks, forfeit and connector lookups. |
| rgb-lib Ark module | `patches/rgb-lib.patch` on kaleidoswap/rgb-lib `8852b2e` (`src/ark_proof.rs` + wallet methods) | Coloring PSBTs with seals on VTXOs/checkpoints, Ark-aware validation of unmined witnesses, self-contained Ark proofs, VTXO status and Ark balance, rollback of failed transfers, fraud proofs. |
| Demo / CLI | `demo/` (`ark-rgb-demo`) | Wallet-side flows for every feature (`scenario <name>`), negative tests, benchmarks. |
| Prototype app | `ark-rgb-demo app` (`demo/src/app.rs`, `demo/app/index.html`) | Local web UI: two wallets side by side driving the same flows by hand. |
| Watchtower | `ark-rgb-demo watchtowerd <dir>` (`demo/src/watchtowerd.rs`) | Optional keyless exit watchtower: holds the user's presigned exit branch and fee ladder, never a user key. |
| Regtest stack | `arkade-regtest` (pinned) + `patches/arkade-regtest.*`, `pools/` | bitcoind, electrs/esplora, `arkd:rgb`, arkd-wallet; offline mempool pools mirror. |

## How it works

RGB binds asset state to single-use seals (outpoints); whoever spends a seal must commit, in that transaction, to the transition that moves the state. Arkade VTXOs are outpoints of pre-signed off-chain transactions, so the question is always: *which transaction spends the VTXO, and can it carry an RGB commitment?* Taproot outputs in Ark are fixed by the protocol, so commitments are **opret only**: an `OP_RETURN <32 bytes>`, 0 sat, at output index 0. Details in [docs/design.md](docs/design.md).

- **Onboarding (jump-in).** The asset is issued on-chain and moved by an ordinary on-chain RGB transfer to a seal on the user's VTXO, which the user gets by boarding BTC. Direct boarding of a colored UTXO is not possible: the commitment tx cannot carry a user commitment, and the VTXO txid is unknown when the intent is signed.
- **Off-chain transfer.** An off-chain spend is a checkpoint tx followed by an ark tx. The checkpoint carries transition T1 (VTXO → checkpoint output) and the ark tx carries T2 (checkpoint output → receivers and change). arkd rebuilds both and compares txids, and the opret is part of the rebuild. Transfers chain over any number of hops; asset change and BTC change are supported.
- **Refresh and collaborative exit.** When a VTXO is refreshed into a new batch or exited to an on-chain address, the only tx that spends it is the user's forfeit tx. The forfeit carries the transition to the new VTXO or to the on-chain output, both known when the forfeit is signed. The forfeit is normally never mined, so receivers accept it as a witness only after attesting it at the Ark level.
- **Offline proof.** The sender attaches to the consignment an Ark proof (`<consignment>.arkproof.json`) with every unmined Ark tx needed down to a mined commitment tx. Receivers verify it against the chain only: spends, operator and tree signatures, and the opret layout. They need no arkd and no trust in the operator for history. A proof is about 2.5 kB per off-chain hop.
- **Sweep recovery.** When a batch expires, the operator sweeps it. A returning user, even months later, recovers the swept VTXO into a new batch through a forfeit that also carries the RGB transition, so asset and BTC come back together. rgb-lib reports such an asset as "to recover", and a recovery is accepted only if the chain shows the VTXO's branch was actually swept.
- **Exit watchtower (optional).** For users who want an on-chain exit that does not depend on the operator: a keyless daemon publishes the presigned exit branch before expiry if the VTXO was not refreshed, paying fees from a ladder of CPFP children presigned by the user.
- **Fraud evidence.** If the operator and a VTXO owner collude to co-sign two forfeits of the same VTXO, both receivers validate offline. The double spend cannot be prevented, but it is provable: `verify_ark_fraud_proof` checks the two operator-co-signed forfeits against the chain. An online receiver can detect it through the indexer's `spent_by`.

## Quickstart

Requirements: Linux, docker, Go ≥ 1.23 (arkd build), rustup (the demo pins Rust 1.96.0), node ≥ 20 (arkade-regtest), git, python3, curl. Around 5 GB of disk.

```bash
git clone <this repo> rgb-arkade && cd rgb-arkade
make setup        # clone pinned upstreams into vendor/ and arkade-regtest/, apply patches,
                  # build arkd:rgb, tools/arkdb and the demo
make regtest-up   # start the regtest stack; prints arkd's /v1/info (version v0.9.15-rgb4)
make list         # list scenarios
make scenario SCENARIO=offchain-transfer
make scenario SCENARIO=collab-exit ARGS=--offline-proof
make test         # quick regression suite
```

Paths: everything (state, logs, regtest checkout, watchtower data) lives under the repository root, or under `$RGB_ARKADE_DIR` if set. The scripts use `$CARGO_TARGET_DIR` if you build the demo elsewhere. Each run writes `state/run-<ts>-<scenario>/` (wallets, consignments, proofs, `summary.json`) and prints a `... RESULT: PASS` line.

Docker networking: on hosts where the iptables FORWARD policy is DROP, containers on the regtest network need `iptables -I FORWARD 1 -s 172.16.0.0/12 -d 172.16.0.0/12 -j ACCEPT`.

## Prototype app

A local web app to try the features by hand: two wallets (alice and bob) side by side on the regtest stack, with buttons for every flow and a timeline of what happened, txids included.

```bash
make app              # or: scripts/app.sh [port]
# open http://127.0.0.1:8080
```

`make app` starts the regtest stack if arkd does not answer, builds the demo and serves the app on `127.0.0.1` (port 8080, or `PORT=...`). The app is a subcommand of the demo binary (`ark-rgb-demo app [port]`): an axum backend that reuses the scenario code, and a single static page with no build step.

| Action | What happens |
|---|---|
| Board BTC | Faucet to the wallet's boarding address and settle it into a batch: a BTC-only VTXO. |
| Issue asset + jump-in | Issue an NIA on-chain with stock rgb-lib, then move it with an on-chain RGB transfer onto one of the wallet's VTXOs. |
| Send off-chain | Checkpoint tx (T1) + ark tx (T2) to the other wallet, with asset change and BTC change back to the sender. The receiver validates **with arkd paused**, from the consignment, the Ark proof and the bitcoin chain only; tampered proofs are rejected first. |
| Refresh | Move the colored VTXO into a new batch; the forfeit tx carries the transition. |
| Collaborative exit | Leave to an on-chain output of the commitment tx; the forfeit carries the transition. |
| Unilateral exit | Broadcast the VTXO's branch without the operator. Other outputs of yours in the same branch go on-chain too: colored ones become `unrolled` holdings, live BTC-only ones can be claimed with *Sweep exited BTC*, and ones you already forfeited or spent off-chain are left alone (they are the operator's / the next tx's). |
| Sweep exited BTC | Claim those BTC-only outputs to the wallet's on-chain address after the exit delay. |
| Send on-chain | Spend an exited output (collaborative or unilateral) to the other wallet with an on-chain RGB transfer, carrying the Ark proof of its history. After a unilateral exit this waits out the exit delay (512 s on regtest), mining blocks meanwhile. |
| Recover swept | Recover a VTXO the operator swept after expiry, asset included. To try it: set the batch expiry to 600 s, issue or refresh, wait until the VTXO shows `swept` (about 10 to 15 min, mine a block now and then), then recover. Set the expiry back to `604672` afterwards. |
| Mine / Set batch expiry | Regtest time controls: mine blocks, change arkd's `vtxoTreeExpiry` through the admin API (applies to new batches). |
| Reset | Delete the data dir and start with two fresh wallets. |

The page has a step indicator for the main flow (Board → Issue + jump-in → Send off-chain → Refresh → Exit → Sweep recovery) with a plain-language explanation of each step on hover, and a **Demo mode** button that runs board, issue, both off-chain sends, refresh, collaborative exit and on-chain send end to end (about one minute). Buttons that cannot run say why; the activity log shows one headline per action with its txids (click to copy) and keeps the technical lines, including the tampered-proof checks, folded.

Each wallet shows its off-chain BTC balance, its VTXOs with expiry, the Ark-aware asset balance (active, to recover, unrolled, in flight) next to rgb-lib's stock balance, and the colored outputs the app is tracking. The API is `GET /api/state` and `POST /api/action` with a JSON body such as `{"action":"send","from":"alice","to":"bob","asset_id":"rgb:...","amount":300}`; actions on a colored output accept an optional `"outpoint"` to pick one when a wallet holds several.

**Persistence.** Everything lives in a data dir, `state/app-data/` by default (`--data-dir=<path>` or `RGB_ARKADE_APP_DATA` to change it; `state/` is gitignored): wallet keys and mnemonics, the rgb-lib data dirs (RGB stock and Ark registry), consignments and Ark proofs, and `app.json` with the tracked colored outputs and the activity log. Restarting the app reopens the same wallets (Ark contracts are rediscovered from arkd's indexer) and continues where it left off. **Reset** (or deleting the dir while the app is stopped) starts fresh.

Limits: one action runs at a time; regtest only; an action interrupted by a restart is not resumed (its log entry stays open; re-run it). An off-chain send splits the VTXO's sats in half when there is asset change, so long chains of partial sends eventually need a refresh.

## Scenarios and test suite

`ark-rgb-demo scenario <name>` runs one flow. `scripts/run-scenario.sh <name>` wraps it and handles the orchestration some scenarios need: changing the batch expiry, a background miner, and starting/killing watchtower daemons. `scripts/run-all.sh` runs the regression suite sequentially. Parallel runs trip electrs ("Invalid indexer").

| Scenario | Feature | What it checks | Time |
|---|---|---|---|
| `basic` | jump-in, off-chain send, unilateral exit | Asset issued on the predicted checkpoint of a VTXO; 1 hop; Bob exits; Carol `settled: 1000` | ~15 min |
| `basic-onchain-genesis` | jump-in | Same, with on-chain issuance and a jump-in tx | ~15 min |
| `offchain-transfer` | multi-hop off-chain transfer | 2 hops with asset change, unilateral exit, on-chain send; Dave `settled: 700` | ~10 min |
| `refresh` | forfeit commitment | Refresh a colored VTXO, then hop, exit, on-chain send; Carol `settled: 1000` | ~10 min |
| `collab-exit` | forfeit commitment | Collaborative exit to an on-chain address, then on-chain send; Dave `settled: 1000` | ~3 min |
| `forfeit-negatives` | forfeit commitment | Malformed checkpoint/forfeit commitments are rejected by arkd; unattested forfeits are rejected by receivers | ~3 min |
| any of the above + `--offline-proof` | offline proof | Receivers validate with arkd paused, from consignment + Ark proof + chain; 9 kinds of tampered proofs are rejected | same |
| `multi-user` | batches | One batch with 4 users, 2 assets and a BTC-only VTXO, after an off-chain send with BTC + asset change; Ark balances are checked | ~5 min |
| `failed-batch` | batches, rollback | A round fails (missing forfeit), the colored forfeit is rolled back, and the retry succeeds | ~3 min |
| `fraud` | fraud evidence | Colluding operator (DB edit through `tools/arkdb`) co-signs two forfeits; fraud proof verified offline; online detection | ~5 min |
| `sweep-recovery [offline_s]` | sweep recovery, Ark balance, sweep check | Offline past the sweep; recovery of BTC + asset; balance goes "to recover" → "active"; `verify-recovery` re-checks it from the chain, with a not-swept negative | ~20 min |
| `long-offline` | watchtower | Scaled 2 weeks offline (1 day = 73 s): refresh policy, watchtower exit with arkd paused, watchtower-down control | ~40 min |
| `exit-watchtower` | watchtower (in-process) | Watched VTXO exits before expiry; unwatched control is swept; the watchtower cannot redirect | ~30 min |
| `multihop-exit` | app flows, unilateral exit | The app's actions headless: asset Alice → Bob → Alice off-chain, Alice exits her change collaboratively, then unilaterally exits the 100 she got back and spends it on-chain. The unrolled branch also holds her forfeited change output, which matures first; the exit spend must still spend the colored output, and the Ark balance must drop the spent `unrolled` amount; Bob `100` on-chain | ~15 min |

```bash
scripts/run-scenario.sh refresh                 # one scenario, log in logs/scenario-refresh.log
scripts/run-all.sh quick                        # everything except the time-scaled scenarios (make test)
scripts/run-all.sh                              # full suite, ~3 h (make test-all)
scripts/bench.sh                                # payload size / validation time, see docs/test-results.md
```

Measured results and txids: [docs/test-results.md](docs/test-results.md).

## Status and limitations

What works (measured on regtest): onboarding, multi-hop off-chain transfers with BTC and asset change, refresh, collaborative and unilateral exit, multi-user and multi-asset batches, failed-batch rollback and retry, offline verification with arkd down, recovery after the sweep months later, the optional keyless watchtower, and fraud proofs.

Limitations:
- **Regtest only.** The patches target arkd `v0.9.15`, and none are upstream.
- **No direct onboarding** of a colored UTXO through boarding (it would need large arkd changes). Use the jump-in.
- **rgb-lib integration is partial.** Ark state is kept in JSON files in the wallet dir (`ark_vtxos.json`, `ark_transfers.json`), not in rgb-lib's database. The stock `get_asset_balance` does not see Ark state; use `get_asset_balance_ark`. Rollback after a failed round is a manual call (`ark_rollback_transfer`).
- **Electrum only.** The on-chain sweep check (`spender_of`) is not implemented for Esplora.
- **History grows.** Receivers must keep and forward consignment + Ark proof. The Ark part stays bounded (about 1 kB per unmined hop in binary form, reset when mined), but RGB history grows with every transfer. Unilateral exit cost grows linearly with the off-chain chain.
- **The demo is a test harness**, not a wallet. Its code is organised by scenario, with in-process users and fixed amounts.

## Security model

- **The operator alone cannot move or steal an asset.** Every transition needs the owner's signature on a tx committing to it. Rewritten commitments and operator-only spends are rejected (tested).
- **Unilateral exit is always possible** before expiry, and the asset reaches the chain with mined witnesses.
- **Preconfirmed transfers** carry the same trust in the operator as BTC on Arkade, no more.
- **Unmined forfeits make seal uniqueness depend on Ark.** An honest operator refuses a second forfeit of the same VTXO (`VTXO_ALREADY_SPENT`, tested). If the operator and the owner collude, they can co-sign two forfeits of one VTXO and two offline receivers will both accept. This is **detected, not prevented**: a public fraud proof exists, and an online receiver sees it via `spent_by`. Deterrence (bonds, reputation) is outside Ark and RGB.
- **Rolling back a failed round is safe** as long as the failed round's commitment tx is never published. If the operator published it later, that would be the fraud case above.
- **Liveness depends on the operator's cooperation.** The operator can refuse a refresh or a recovery: the asset is then frozen, not destroyed. The optional watchtower gives an exit that does not depend on the operator, at on-chain fee cost.
- **Privacy:** only a 32-byte commitment is visible to the operator and on-chain. Opret makes RGB transactions recognizable, so selective censorship is possible.

More in [docs/design.md](docs/design.md#security-model).

## Roadmap

- **Upstream to Ark Labs:** propose the arkd changes (all small and opt-in) and the rust-sdk hooks; open questions are in [docs/design.md](docs/design.md#open-questions-for-ark-labs).
- **rgb-lib:** move the Ark module into the KaleidoSwap fork with tests and database persistence, add Esplora support, and make rollback automatic.
- **Delta consignments:** a hub keeps the verified history, and payments carry only the new part (with a validation cache). This needs rgb-lib work, not a consensus change.
- **Delegated refresh:** refreshing for an offline user needs arkd support, because the forfeit must commit to an output that is unknown until the batch is built.
- **Atomic swaps** (asset ↔ BTC and asset ↔ asset in one ark tx or batch; HTLC variants towards on-chain and RGB Lightning). The design is in [docs/design.md](docs/design.md#atomic-swap-on-arkade-with-rgb-theory-only) and not implemented.
- **Wallet integration:** automatic refresh and recovery, `spent_by` monitoring, and a watchtower as a service.

## Repository layout

| Path | What |
|---|---|
| `PATCHES.md`, `patches/` | One cumulative patch per upstream repo (arkd, rust-sdk, rgb-lib, arkade-regtest) plus the regtest `.env` |
| `arkd-image/Dockerfile` | `arkd:rgb` = upstream arkd image with the patched binary |
| `demo/` | `ark-rgb-demo`: `src/main.rs` (CLI), `common.rs` (wallet/client helpers), `paths.rs`, one module per feature (`basic`, `offchain_transfer`, `forfeit`, `proof`, `exit_watchtower`, `long_offline`, `watchtowerd`, `sweep_recovery`, `batch_scenarios`, `bench`), `regtest.rs`; `src/app.rs` + `app/index.html` (prototype app) |
| `scripts/` | `setup.sh`, `regtest-up.sh`, `app.sh`, `run-scenario.sh`, `run-all.sh`, `bench.sh`, `patches-check.sh`, `lib.sh` |
| `tools/arkdb-sim/` | Test tool that plays a colluding operator editing its own sqlite DB (built into `tools/arkdb`) |
| `bench/` | Fits and USDt-like extrapolation for the benchmark output |
| `pools/` | Offline mirror of mempool's `pools-v2.json` |
| `docs/` | `design.md`, `test-results.md` |
| `Makefile` | `setup`, `build`, `regtest-up`, `app`, `scenario`, `test`, `test-all`, `bench`, `patches-check` |

Generated, not committed: `vendor/`, `arkade-regtest/`, `state/`, `logs/`, `watchtowerd/`, `tools/arkdb`, `arkd-image/arkd`, `demo/target/`.

## Contributing

- Change upstream code in `vendor/<repo>` (created by `make setup`), then regenerate its patch with `git -C vendor/<repo> diff > patches/<repo>.patch`. For rgb-lib, new files must be added with `git add -N` first. Run `make patches-check`. Mark new code with a `[rgb-arkade]` / `[rgb-arkade: <change>]` comment and update `PATCHES.md`.
- If arkd changes, rebuild the image (`scripts/setup.sh` is idempotent) and restart the stack.
- New behaviour gets a scenario: add a module under `demo/src/`, register it in `SCENARIOS` and the `match` in `main.rs`, add it to `scripts/run-all.sh` and the table above. A scenario prints `<NAME> RESULT: PASS|FAIL` and exits non-zero on failure. Add negative cases where something must be rejected.
- Before a PR: `cd demo && cargo build` without warnings, `make test` (or the affected scenarios), and `make patches-check`.
- Keep measured and estimated numbers apart in docs.

## License

TODO: no license has been chosen yet. Until one is added, all rights are reserved by the authors. The patches modify upstream projects under their own licenses (arkd and rust-sdk: MIT; rgb-lib: MIT). Check them before distributing patched builds.
