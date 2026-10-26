# rgb-arkade: RGB assets on Arkade (regtest MVP, Stages 0–6)

This repository contains what is needed to understand and reproduce the RGB-on-Arkade spike. It holds the patches against pinned upstream commits, the demo client, the run scripts, and the result summaries. Upstream checkouts, build outputs, wallets, and raw run logs are not committed: `scripts/setup.sh` recreates the sources and builds.

Plain-language write-up: [`docs/rgb-on-arkade.md`](docs/rgb-on-arkade.md), an English translation of the Italian Page ["RGB su Arkade: come funziona e cosa si può fare"](https://cursor.com/canvas/store-e5dae6c1-22ce-4fc9-be4f-958a0cdf37c7/d7de931f-98e3-4a02-af31-8ccd68868a97), which holds the original.

## MVP conclusion

**What works (measured on regtest):**
- RGB assets live inside Arkade VTXOs: issuance or jump-in, multi-hop off-chain sends with BTC and asset change, refresh into new batches, collaborative exit, and unilateral exit.
- Shared batches work: several users, two assets, and BTC-only VTXOs in one batch.
- A failed batch is rolled back and retried without the asset getting stuck.
- Receivers validate the whole history offline (arkd paused) from the consignment, a few-kB Ark proof, and the chain.
- A user offline for months recovers BTC and asset after the sweep, with no watchtower and no longer expiry. rgb-lib reports the asset as "to recover" until then.
- The operator alone cannot move or take an asset.

**What does not work, or is only detected:**
- If the operator and the owner collude, they can co-sign two forfeits of the same VTXO. The double spend is detected and provable (fraud proof), not prevented. An offline receiver cannot notice it.
- The operator can refuse service, which leaves the asset stuck (not lost).
- Direct onboarding of a colored UTXO through boarding is not possible without large arkd changes.
- The stock rgb-lib DB balance does not see Ark state; use `get_asset_balance_ark`.
- The sweep check and `spent_by` need Electrum (no Esplora support).

**To go further:**
- Upstream the six small arkd patches.
- Move the rgb-lib Ark API into the KaleidoSwap fork, with DB persistence and Esplora support.
- Wallet automation: refresh, recovery on return, rollback of failed rounds, and `spent_by` checks.
- Deterrence for operator fraud.
- An atomic swap prototype (design in the docs).

## Repo layout

| path | what |
|---|---|
| `README.md` | this file, followed by the full stage-by-stage engineering log (Stages 0–6) |
| `docs/rgb-on-arkade.md` | English explanation (translated from the Italian Page), with results per stage, the atomic swap design (theory), and the conclusion |
| `patches/arkd.patch` | cumulative arkd patch (patches 1–6) against arkade-os/arkd `v0.9.15` |
| `patches/rust-sdk.patch` | cumulative patch against arkade-os/rust-sdk `v0.12.0` |
| `patches/rgb-lib.patch` | cumulative patch against kaleidoswap/rgb-lib `8852b2e`, including the Stage 6 Ark API (`src/ark_proof.rs`, status/balance/rollback/fraud verification) |
| `patches/arkade-regtest.patch`, `patches/arkade-regtest.env` | regtest stack compose changes and the `.env` (image `arkd:rgb`, min amount 1, offline pools mirror) |
| `arkd-image/Dockerfile` | `arkd:rgb` = upstream `ghcr.io/arkade-os/arkd:v0.9.15` with the patched binary (built by `setup.sh`) |
| `demo/` | `ark-rgb-demo` client: every stage flow, negatives, proofs, watchtower, benches (Rust 1.96.0) |
| `tools/arkdb-sim/main.go` | Stage 6 test tool: simulates a colluding operator editing its own arkd sqlite DB |
| `scripts/setup.sh` | clones the pinned upstream commits, applies the patches, and builds `arkd:rgb`, `tools/arkdb`, and the demo |
| `scripts/run-*.sh` | scripted long runs: Stage 4 offline-2-weeks, Stage 4 benches, Stage 5 sweep recovery, and the Stage 6 full regression suite |
| `bench/` | Stage 4 size/latency analysis and the USDt extrapolation |
| `pools/` | offline mirror of mempool `pools-v2.json` (the regtest mempool container needs it without internet) |
| `evidence/stage*-summary.json` | measured results per stage (txids, sizes, timings) |

## Pinned upstream commits

| component | repo | commit |
|---|---|---|
| arkd | https://github.com/arkade-os/arkd | `f444de6c336c024f5b03e646feb781eec9a24c00` (tag `v0.9.15`) |
| arkd-wallet image | ghcr.io/arkade-os/arkd-wallet | `v0.9.15` |
| rust-sdk | https://github.com/arkade-os/rust-sdk | `f184a5f1aa77924b7fd40eede7cf2fd43854ae76` (tag `v0.12.0`) |
| rgb-lib | https://github.com/kaleidoswap/rgb-lib | `8852b2e55606ebd568f56bdfd9fa9216fbec3435` |
| arkade-regtest | https://github.com/ArkLabsHQ/arkade-regtest | `457b9ef6119bea1b0a8f1356204ef2f1044b0c9b` |

All four patches were checked with `git apply --check` against these commits.

## Reproduce from scratch

Requirements: Linux, docker, Go (for arkd), rustup (the demo pins 1.96.0), node ≥ 20 (arkade-regtest).

The demo and scripts expect the repo at `/workspace/rgb-arkade`. Clone or symlink it there; the scripts accept `R=...`, but `demo/src` has the path hardcoded.

```bash
git clone <this repo> /workspace/rgb-arkade && cd /workspace/rgb-arkade
scripts/setup.sh                                   # vendor/ + arkade-regtest/ + arkd:rgb + tools/arkdb + demo build
cd arkade-regtest && node regtest.mjs start --profile ark &   # then start the pools mirror:
docker run -d --name rgbarkade-pools --network arkade-regtest_default --network-alias pools-mirror \
  -v /workspace/rgb-arkade/pools:/usr/share/nginx/html:ro nginx:alpine
wait; curl -s localhost:7070/v1/info               # arkd v0.9.15(-rgb4), vtxoMinAmount "1"
cd ../demo
./target/debug/ark-rgb-demo                        # Stage 0 variant A   (--onchain-genesis = variant B)
./target/debug/ark-rgb-demo --stage1               # Stage 1 multi-hop
./target/debug/ark-rgb-demo --stage2-refresh       # Stage 2 (--stage2-exit, --stage2-neg)
./target/debug/ark-rgb-demo --stage1 --offline-proof   # Stage 3 offline proofs (pauses arkd)
../scripts/run-stage4-offline2w.sh                 # Stage 4 (~40 min, scaled time)
../scripts/run-stage5.sh 720                       # Stage 5 sweep recovery (~20 min)
./target/debug/ark-rgb-demo --stage6-multi         # Stage 6 items 1, 2, 5 (--stage6-fail, --stage6-fraud)
./target/debug/ark-rgb-demo --stage6-verify-recovery ../state/<stage5 run dir>   # item 4
../scripts/run-stage6-regressions.sh               # everything, sequential (~2 h)
```

Every run writes `state/<run-id>/` (wallets, consignments, proofs, summary) and prints `RESULT: PASS`. The detailed notes below (box setup: dockerd, iptables, etc.) come from the original working directory. The logs and run directories they cite are not in this repo; the measured numbers are in `evidence/`.

---

# RGB on Arkade: Stage 0 regtest MVP + Stage 1 multi-hop + Stage 2 forfeit commitment + Stage 3 offline proofs & exit watchtower

Everything lives under `/workspace/rgb-arkade` on the shared box. Nothing was pushed to any remote.

**Result: PASS.** Both variants finish with Carol's `list_assets` / `get_asset_balance` showing `settled: 1000`:

- **Variant A (VTXO genesis):** issue straight to the predicted checkpoint output.
- **Variant B (on-chain genesis + jump-in):** issue with stock rgb-lib on a mined UTXO, then move the asset into Arkade.

**Stage 1 (multi-hop A→B→C off-chain): PASS** with a patched arkd (`arkd:rgb`): Dave's `list_assets` shows `settled: 700` after jump-in → Alice→Bob → Bob→Carol (700 + 300 change) → Carol unilateral exit → Carol→Dave on-chain. See [Stage 1](#stage-1-multi-hop-off-chain-abc).

**Stage 2 (RGB commitment in the forfeit tx): PASS** with `arkd:rgb` `v0.9.15-rgb2`. (a) refresh of a colored VTXO into a new batch → off-chain hop → unilateral exit → on-chain send: Carol `settled: 1000`. (b) collaborative exit of a colored VTXO to an on-chain address → on-chain send: Dave `settled: 1000`. 10 negative tests pass. See [Stage 2](#stage-2-rgb-commitment-in-the-forfeit-tx-refresh--collaborative-exit).

**Stage 3: PASS** with `arkd:rgb` `v0.9.15-rgb3` (arkd patch 5: the operator co-signs the stored forfeit). (A) Receivers validate the whole history from the consignment plus a sidecar Ark proof plus the chain, with arkd paused (`/v1/info` unreachable): Stage 1 (Dave `settled: 700`), Stage 2 (a) (Carol `settled: 1000`), Stage 2 (b) (Dave `settled: 1000`); 9 kinds of mutated proofs are rejected. (B) An exit watchtower with no user keys pushes a colored VTXO on-chain before batch expiry (Gina `settled: 1000`); the unwatched control VTXO is swept and its asset is stuck. See [Stage 3](#stage-3-self-contained-ark-proofs--exit-watchtower).

## Layout

| path | what |
|---|---|
| `arkade-regtest/` | ArkLabsHQ/arkade-regtest @ `457b9ef` (arkd/arkd-wallet `v0.9.15`), with local compose patches |
| `vendor/arkd/` | git worktree of arkade-os/arkd at tag `v0.9.15` (`f444de6`), with the Stage 1 + Stage 2 patches (`patches/arkd.patch`) |
| `arkd-image/` | `Dockerfile` for `arkd:rgb` (FROM `ghcr.io/arkade-os/arkd:v0.9.15`, binary replaced) + the host-built `arkd` binary |
| `vendor/rust-sdk/` | arkade-os/rust-sdk @ `f184a5f`, with a local patch |
| `vendor/rgb-lib/` | kaleidoswap/rgb-lib @ `8852b2e`, with a local patch (feature `electrum`) |
| `demo/` | `ark-rgb-demo` binary (`src/main.rs` Stage 0 flow, `src/stage1.rs` Stage 1 flow, `src/stage2.rs` Stage 2 flows, `src/proof.rs` Stage 3 offline-proof builder/negatives, `src/stage3.rs` Stage 3 watchtower, `src/regtest.rs` chain backend). Toolchain is pinned to 1.96.0 in `rust-toolchain.toml` |
| `patches/` | `git diff` of every local patch (`arkd.patch`, `rust-sdk.patch`, `rgb-lib.patch`, `arkade-regtest.patch`; cumulative, Stage 0+1+2), `arkd-patch4-forfeit-only.patch` (the Stage 2 arkd delta alone), `stage1/` and `stage2/` (the Stage 1 / Stage 2 versions), `arkd-patch5-forfeit-cosign.patch` (the Stage 3 arkd delta alone), `arkd-rgb.Dockerfile`, and the `.env` used (`ARKD_IMAGE=arkd:rgb`) |
| `pools/` | offline mempool pools-v2.json mirror (see deviations) |
| `state/<run-id>/` | per-run wallets, seeds, consignments (`*.rgbc`), `summary.json` |
| `logs/` | run logs (`run6-vtxo-genesis.log`, `run7-onchain-genesis.log` = the Stage 0 passes; `stage1-pass.log` = Stage 1 pass; `stage1-regress-stage0-variant{A,B}.log` = regression on `arkd:rgb`; `stage1-run1.log` = first, failed attempt, with the OP_RETURN after the checkpoint output; `stage2-refresh-pass.log`, `stage2-exit-pass.log`, `stage2-neg-run1.log` = Stage 2; `stage2-exit-run1.log` = first Stage 2 (b) attempt, stuck in `WaitingSafeHeight`; `stage2-regress-*.log` = Stage 1/0 regressions on `v0.9.15-rgb2`) |

## Reproduce

```bash
# 0. docker (box has no systemd): already installed; if dockerd is not running:
sudo sh -c 'nohup /usr/sbin/dockerd > /var/log/dockerd.log 2>&1 &'; sudo chmod 666 /var/run/docker.sock
# containers need to talk to each other (box iptables FORWARD policy is DROP):
sudo iptables-legacy -C FORWARD -s 172.16.0.0/12 -d 172.16.0.0/12 -j ACCEPT 2>/dev/null || \
  sudo iptables-legacy -I FORWARD 1 -s 172.16.0.0/12 -d 172.16.0.0/12 -j ACCEPT

# 1. regtest stack (M1)
cd /workspace/rgb-arkade/arkade-regtest
cp ../patches/arkade-regtest.env .env          # if missing; the compose patch is applied in-tree
node regtest.mjs clean                          # optional, fresh chain
node regtest.mjs start --profile ark &          # wait until it prints "Waiting for mempool Esplora API"
# once the compose network exists, start the offline pools mirror (mempool_api auto-restarts and picks it up):
docker rm -f rgbarkade-pools 2>/dev/null; docker run -d --name rgbarkade-pools \
  --network arkade-regtest_default --network-alias pools-mirror \
  -v /workspace/rgb-arkade/pools:/usr/share/nginx/html:ro nginx:alpine
wait                                            # ends with "Regtest environment ready"
curl -s localhost:7070/v1/info                  # arkd v0.9.15, vtxoMinAmount "1"
curl -s localhost:3000/api/blocks/tip/height    # esplora

# 2. demo (M2..M7), about 15 min each because of the 512 s exit delay
cd /workspace/rgb-arkade/demo && cargo build
RUST_LOG=warn,ark_client=info ./target/debug/ark-rgb-demo                    # variant A
RUST_LOG=warn,ark_client=info ./target/debug/ark-rgb-demo --onchain-genesis  # variant B
# ends with "RESULT: PASS"; evidence is in state/<run-id>/summary.json
```

## Flow implemented

**Common to both variants:**

1. **M2: board.** Alice and Bob each board (faucet to the boarding address, then `settle`). Alice's VTXO `V` is printed.
2. **M3: predict.** Alice dry-runs `ark_core::send::build_offchain_transactions(V → Bob 20 000 sat, change → Alice)`. The checkpoint txid `cp` depends only on `V` and the checkpoint tapscript.
3. **M3, the asset lands on the seal `cp:0`:**
   - **A:** patched `issue_asset_nia_to_outpoint(cp:0)` makes the genesis seal `cp:0`.
   - **B:** stock `create_utxos` + `issue_asset_nia` put a mined genesis on an rgb-lib UTXO. Then a user-built on-chain tx `T1` (colored UTXO → `[OP_RETURN opret, BTC change]`) moves the asset to the **explicit external seal** `cp:0`. Uses the patched `color_psbt_and_consume_to_outpoint`. `T1` is mined.
4. **M4: color and submit the ark tx.**
   - Rebuild the same ark tx. Insert a bare `OP_RETURN` at output 0 carrying **1 sat**, and take 1 sat off Alice's change.
   - Color a clone with `color_psbt_and_consume` (NIA 1000 → Bob's output 1, static blinding). Copy only the `OP_RETURN <32B>` commitment script back.
   - Save the consignment, then `submit_built_offchain_send`. Check that the server ark txid equals the local one and the server checkpoint txid equals the prediction. Then `sign_and_finalize_pending_tx`.
5. **M5: Bob accepts.** `accept_transfer_consignment(ark_txid, vout 1, blinding)`. The ark tx is resolved by `OffchainResolver`; it is still offchain at this point.
6. **M6: Bob exits.**
   - Fund Bob's on-chain wallet for anchor bumps.
   - `build_unilateral_exit_trees` → pick the branch ending in the ark tx → `broadcast_next_unilateral_exit_node` + mine until done. arkd itself broadcast the checkpoint tx.
   - Then `update_witnesses`.
7. **M7: Bob → Carol on-chain.**
   - Carol: `witness_receive(None, Fungible(1000), …, [] transports)`.
   - Bob: after the CSV 512 s, the patched `create_send_on_chain_transaction_with_hook` builds `[OP_RETURN, Carol 2000 sat, change]` from the exited VTXO (exit leaf). The hook colors it before signing, and the tx is broadcast.
   - Carol: `provide_out_of_band_consignment` → mine → `refresh` → `list_assets`.

## Pass log (Europe/Zurich, CET)

- **M1, 12:40:** stack ready (`regtest-start5.log`, `EXIT 0`).
  - arkd `/v1/info`: `version v0.9.15`, `vtxoMinAmount "1"`, `unilateralExitDelay "512"`, `dust "330"`, `maxOpReturnOutputs "3"`. Saved in `logs/m1-arkd-info.json`.
  - bitcoind v31.0.0 RPC OK; Fulcrum 2.1.1 on :50001; Esplora on :3000/api.
- **Variant A: `logs/run6-vtxo-genesis.log`, `state/run-1792929606/summary.json`, PASS 13:09**

  | step | evidence |
  |---|---|
  | M2 Alice VTXO | `8f6207d8…ebaf:0` (100 000 sat) |
  | M3 predicted checkpoint | `f4de72ca5032c314ba8ab8d5dc181200849bd717302fe4796dc1f448b7a80480` |
  | M3 asset | `rgb:BrSUVQbY-G5eymAg-~zbjSGy-NhzpvrW-Zo2OrHN-oCjWiSI` (NIA ARKT, 1000) |
  | M4 ark tx (local == server) | `30789e00c049de472eeaaa0918a6b189640572676bd71f28fb6e4cd7fe05a21b`; server checkpoint == prediction |
  | M4 ark tx outputs | `[1 sat OP_RETURN 974b4898…9354, 20000 Bob, 79999 Alice, P2A]` |
  | M5 Bob accept | assignments `[Fungible(1000)]` |
  | M6 exit branch | leaf `8f6207d8…` → checkpoint `f4de72ca…` → ark tx `30789e00…` (all mined; v3 ark tx carries the opret on chain) |
  | M7 Bob→Carol | `7aa8cd5d0f495583427dbe6f47a519fff089c043a6ad6a85422417892dbfe29f` |
  | M7 Carol | `refresh` → `Settled`; `list_assets` ARKT `Balance { settled: 1000, future: 1000, spendable: 1000 }` |

- **Variant B: `logs/run7-onchain-genesis.log`, `state/run-1792929651-onchain-genesis/summary.json`, PASS 13:11**

  | step | evidence |
  |---|---|
  | genesis UTXO (mined) | `829baa0a7710f98f573de910bb8c3e9a44221e7e18c9e36085d44122c2b80da1:1` |
  | predicted checkpoint | `51932c7a5d7097160c1a9b22cae0173223c3a21feb64654fde04582988736f22` |
  | jump-in T1 (mined, opret `6a208024…8d40`) | `70728ac1484ab0dc00de2d3279aa047be54fab56de8b3792b02c1db0e8deaa27` |
  | asset | `rgb:C7145cqw-WYExAva-uZqG47r-XDC6kaR-Z6U6IsM-ctx2gn8` |
  | ark tx (local == server; checkpoint == prediction) | `f6ddbe788cf520fc451fca0e25aea9bce6d08dbfa1bad71657a02a424d553ef3` |
  | Bob→Carol | `12bfea3f9b334bcabfa52b18a3395316baf7f295e5622cd8b46401047564a731` |
  | Carol | `Settled`, `settled: 1000` |

## Deviations from the plan / local patches (all in `patches/`)

1. **`ARKD_VTXO_MIN_AMOUNT=1` instead of 0.** arkd v0.9.15 rejects 0 with `invalid app config: vtxo min amount must be greater than 0`. The compose file hardcoded 1; it was patched to read the env var. As a result the RGB opret output in the ark tx carries **1 sat**: arkd treats `OP_RETURN <32B>` as a sub-dust VTXO, which needs value ≥ min.
2. **Tree expiry and exit delays.** `ARKD_VTXO_TREE_EXPIRY=604672` s; unilateral, public and checkpoint exit delays 512 s; boarding exit delay 1024 s.
3. **Box networking.** Containers have no DNS or egress on this box, and the pre-existing iptables FORWARD policy is DROP.
   - Added `iptables-legacy -I FORWARD 1 -s 172.16.0.0/12 -d 172.16.0.0/12 -j ACCEPT`. This is a box-level change.
   - mempool backend v3.3.1 aborts without GitHub pools data. Fixes:
     - env passthrough for `MEMPOOL_INDEXING_BLOCKS_AMOUNT=0` and `MEMPOOL_AUTOMATIC_POOLS_UPDATE=false`;
     - `MEMPOOL_POOLS_JSON(_TREE)_URL` pointed at a local nginx sidecar `rgbarkade-pools`, which serves `pools/`.
   - mempool frontend: `extra_hosts: mempool.space:127.0.0.1`.
   - `LND_P2P_PORT=19735`, because 9735 is used by another agent's service.
4. **rust-sdk patch:**
   - `resolve_selected_send_inputs`, `submit_built_offchain_send` and `sign_and_finalize_pending_tx` made `pub`.
   - New `ark_core::unilateral_exit::create_unilateral_exit_transaction_with_hook`: extra leading outputs, an explicit fee, and a pre-sign PSBT hook. The stock function delegates to it unchanged.
   - New `Client::create_send_on_chain_transaction_with_hook`.
   - Side note: the stock `create_unilateral_exit_transaction` sets change = inputs − to_amount, i.e. **zero fee**, so its tx would not relay. The hook variant subtracts an explicit fee.
5. **rgb-lib patch** (`src/wallet/rust_only.rs`, additive only):
   - `issue_asset_nia_to_outpoint`: variant A genesis on an arbitrary outpoint.
   - `color_psbt_and_consume_to_outpoint`: variant B, a transition to an explicit external seal.
   - `psbt_input_for_utxo`: fills a PSBT input for a wallet UTXO so `sign_psbt` can sign a caller-built tx.
6. **rgb-lib `color_psbt` vout shift.** With any P2TR output, `color_psbt` shifts `output_map` vouts by +1 (`opreturn_first`). The demo pre-inserts a *bare* `OP_RETURN` at index 0 and passes `actual_vout - 1`. The opret embed requires a bare `OP_RETURN` (len 1), and verification requires exactly `OP_RETURN <32B>` (len 34).
7. **Burn-amount guard.** bitcoind's `sendrawtransaction` / `submitpackage` default `maxburnamount=0` rejects the ark tx's **1-sat OP_RETURN** during unilateral exit (`Unspendable output exceeds maximum configured by user`). The demo passes `maxburnamount=0.0001`. The tx is standard; this is only an RPC-side guard. Any real exit tooling must do the same.
8. **Toolchain and lockfile.** Rust 1.96.0 is pinned only in `demo/` (shared box). `Cargo.lock` was seeded from the rgb-lib fork, to avoid the yanked `secp256k1 0.32.0-beta.2`. `rustls` ring provider is installed explicitly.
9. **Single-process flow.** The whole flow runs in one process. ark-client keys are BIP32 with in-memory discovery, so no resume across processes. Seeds and mnemonics are saved in `state/<run>/` for inspection.

## Boarding-path analysis (the user's preferred model)

The question was whether to issue on-chain, board *that* colored UTXO, and let the commitment tx carry the RGB transition. Short answer: **not possible on stock arkd v0.9.15 (`f863e48`)**. Variant B gets the same user-facing result (mined genesis, then asset on Arkade) without touching arkd.

Findings from source:

- **The commitment tx has no user-controlled OP_RETURN.**
  - `internal/infrastructure/tx-builder/covenantless/builder.go` `createCommitmentTx` builds outputs `[batch output, connector output, intent on-chain outputs…, operator change]`.
  - Intent on-chain outputs go through `getOnchainOutputs` (`utils.go`), which only accepts **addresses**: `arklib.DecodeBitcoinAddress` → `PayToAddrScript`. So an OP_RETURN cannot be requested.
  - Inputs are boarding inputs plus operator wallet UTXOs chosen at batch time (`SelectUtxos`).
- **The intent "extension" OP_RETURN does not help.** It is OP_RETURN `"ARK"` + TLV (`pkg/ark-lib/extension`; `service.go` ~L1921-2137).
  - arkd copies it into the **batch leaf tx** (`LeafTxExtension` → `getOutputVtxosLeaves`), not into the commitment tx.
  - Its script is `OP_RETURN <"ARK"…>`. RGB `OpretFirst` verification requires exactly `OP_RETURN <32-byte commitment>` (rgb-consensus `dbc/opret/spk.rs`: len must be 34). So it cannot be an RGB commitment.
  - The leaf tx also does not spend the boarding seal.
- **The new seal is circular even if an opret were allowed.** The transition closing the boarding seal must assign to some output.
  - Any VTXO or leaf outpoint depends on the commitment txid, which would contain the opret committing to that outpoint. Explicit seals commit to the txid, and blinding does not remove that.
  - That leaves a witness-vout seal on a commitment-tx output. Batch output 0 is then spent by operator-built tree txs with no commitment, so the asset would be lost. Unless **every** tree node tx becomes RGB-aware.
- **Is the VTXO / leaf txid predictable when the intent is signed?** No. It depends on the commitment txid, which depends on operator-selected inputs and all intents in the batch. The client first learns it in `TreeSigningStarted` (`unsigned_commitment_tx` + tree; rust-sdk `batch.rs`). That is before it signs its boarding input in `BatchFinalization`, but after the intent is registered.

**Minimal arkd patch for a true boarding path** (not done; design only):

1. Allow one user OP_RETURN (exactly 34 bytes) per intent in the commitment tx. For example, accept an intent output with an `OP_RETURN` script in `getOnchainOutputs`, and place it as the **first** OP_RETURN of the commitment tx. Validate the size and the count against `maxOpReturnOutputs`.
2. Either:
   - (a) make tree txs RGB-carrying: a per-node OP_RETURN aggregating the MPC of all RGB transitions on that path. This is a large change to tree building, signing and sweeping. Or:
   - (b) assign the boarding transition to a dedicated **on-chain** commitment-tx output owned by the user, and re-enter Arkade later via the checkpoint trick. That is just Variant B with an extra hop.
3. Plus a protocol step so the client builds the transition *after* seeing the commitment template but *before* the txid is fixed. That needs a new round phase, because today the commitment tx is fixed before tree signing.

Recommendation: keep Variant B ("on-chain genesis + jump-in to predicted checkpoint") as the onboarding path on stock arkd. It is one extra mined user tx and no operator changes.

## Stage 1: multi-hop off-chain A→B→C

### Design

Every off-chain hop carries **two** RGB transitions, so each Arkade virtual tx that spends a sealed output commits to the transition closing that seal:

| tx | outputs | RGB transition |
|---|---|---|
| checkpoint tx (spends the VTXO) | `[OP_RETURN <32B> (0 sat), checkpoint P2TR, anchor]` | **T1**: VTXO seal → witness vout 1 (the checkpoint output `CP:1`) |
| ark tx (spends `CP:1`) | `[OP_RETURN <32B> (0 sat), receiver…, change, anchor]` | **T2**: `CP:1` → receiver / change outputs (witness vouts) |

- The jump-in (Variant B) now targets **Alice's VTXO outpoint V** directly: no prediction of the checkpoint txid is needed any more.
- T1 is built first with a placeholder payload to fix the layout. Its commitment becomes the checkpoint OP_RETURN payload, and the checkpoint txid is computed only after that (witness-vout seal, so there is no circularity). T2 is then colored on the ark tx.
- Both txids are re-derived by arkd and must match the local ones (checked in the demo).
- **Why the checkpoint OP_RETURN is at index 0:** the first attempt put it after the checkpoint output. RGB rejected the consignment: `first DBC-compatible output of witness transaction … doesn't match the provided proof type (opret1st)`. P2TR outputs are DBC-compatible (tapret), so an opret commitment must come **before** any P2TR output. The checkpoint output therefore moves to vout 1 when a payload is present. Without a payload, the upstream layout `[checkpoint, anchor]` is unchanged.

### Patches

1. **arkd** (`patches/arkd.patch`, 7 files, about +99/−10). Applied on tag `v0.9.15` so it matches the running `arkd-wallet`.
   - **Patch 1, checkpoint OP_RETURN** (`pkg/ark-lib/offchain/tx.go`, `internal/core/application/service.go`):
     - New optional field `VtxoInput.CheckpointOpReturn []byte`. If set, it must be exactly 32 bytes, and `buildCheckpointTx` emits `[OP_RETURN <32B> 0 sat, checkpoint, anchor]`.
     - `CheckpointOutputIndex(tx)` gives the checkpoint vout: 1 with a payload, 0 without. It is used by the ark-tx input built in `buildCheckpointTx`, the tap-tree bookkeeping in `service.go`, the sweeper (`scheduleCheckpointSweep`), `checkpointOutputScripts` (restart watch list) and the indexer chain (`indexer.go`).
     - `ExtractCheckpointOpReturn(tx)`: `SubmitOffchainTx` extracts the payload from each submitted checkpoint. It accepts at most one OP_RETURN, which must be 0 sat and `OP_RETURN PUSH32`; anything else gets `CHECKPOINT_MISMATCH`. The payload feeds the deterministic `BuildTxs` rebuild, so the existing checkpoint/ark txid comparison covers it.
   - **Patch 2, 0-value RGB opret is data, not a VTXO** (`pkg/ark-lib/script/script.go`, `service.go` `validateOffchainTxOutputs`, `utils.go` `decodeTx`, `infrastructure/db/service.go` projection):
     - New `script.IsDataOpReturn(value, script)`: value == 0 and the script starts with OP_RETURN.
     - Mechanism: **the value**. A sub-dust VTXO must carry at least `vtxoMinAmount` (> 0, enforced by config), so a 0-value `OP_RETURN <32B>` is unambiguous.
     - Validation accepts it as a data output (it still counts towards `maxOpReturnOutputs`). Both VTXO-creation paths skip it, and vouts are preserved.
     - Stock arkd rejects it with `AMOUNT_TOO_LOW`, because it has the sub-dust script shape. The 1-sat workaround is **no longer needed** in Stage 1, and the ark tx no longer needs `maxburnamount` to relay.
   - **Patch 3, restart robustness** (`service.go` `restoreWatchingVtxos`): skip stored "pubkeys" that are not valid x-only points.
     - Found while restarting the Stage 0 stack with the new image: the Stage 0 **1-sat sub-dust VTXO** stores the RGB commitment as its pubkey. Its P2TR address is invalid, and the wallet rejects the whole `WatchScripts` call, so **stock arkd v0.9.15 cannot start** (`failed to restore watching vtxos: … invalid-address`).
     - This affects any sub-dust VTXO whose payload is not a curve point. That makes it an upstream bug independent of RGB.
   - Build: host Go (`go1.27.2`, the module cache downloads from the host), then `CGO_ENABLED=0 go build ./cmd/arkd`. The image is `FROM ghcr.io/arkade-os/arkd:v0.9.15`, with `/app/arkd` replaced (`patches/arkd-rgb.Dockerfile`). Containers have no egress, so building with the upstream Dockerfile would fail.
2. **rust-sdk** (`ark-core/src/send.rs`, about +40):
   - `VtxoInput::with_checkpoint_op_return(Option<[u8;32]>)`.
   - `build_checkpoint_psbt` emits the same layout as arkd.
   - New `checkpoint_output_index`, used for the ark-tx input vout and `witness_utxo`.
3. **rgb-lib** (`src/utils.rs`, `src/wallet/rust_only.rs`, `src/lib.rs`, about +200):
   - `MultiOffchainResolver`: for every txid in a caller-supplied set, a mined chain status wins. Otherwise the tx bundled in the consignment is returned as `Tentative`. All other txids go to the blockchain resolver.
   - `Wallet::accept_transfer_consignment_offchain(online, path, offchain_txids, txid, vout, blinding)`: validates and accepts with that resolver.
   - `Wallet::color_psbt_and_consume_custom(psbt, contract_id, Vec<(SealTarget, amount, blinding)>)`: moves all of the contract's state on the PSBT inputs to `SealTarget::WitnessVout(exact vout, no opreturn_first shift)` or `SealTarget::Explicit(outpoint)`. Amounts must balance. It consumes the fascia and returns one consignment for all beneficiaries.
4. **demo** `src/stage1.rs`:
   - `rgb_hop()` builds T1 then T2, submits, and checks both txids against the server.
   - `arkd_virtual_txs_known()` cross-checks every offchain witness with arkd `GetVirtualTxs` before the receiver accepts.

### Reproduce

```bash
# arkd:rgb (once): worktree at v0.9.15 + patch, host build, thin image
cd /workspace/ark/arkd && git fetch origin tag v0.9.15 && git worktree add /workspace/rgb-arkade/vendor/arkd v0.9.15
cd /workspace/rgb-arkade/vendor/arkd && git apply ../../patches/arkd.patch
CGO_ENABLED=0 go build -ldflags="-X 'main.Version=v0.9.15-rgb'" -o ../../arkd-image/arkd ./cmd/arkd
cp ../../patches/arkd-rgb.Dockerfile ../../arkd-image/Dockerfile && docker build -t arkd:rgb ../../arkd-image
# stack: .env has ARKD_IMAGE=arkd:rgb (patches/arkade-regtest.env)
cd /workspace/rgb-arkade/arkade-regtest && node regtest.mjs start --profile ark
curl -s localhost:7070/v1/info   # "version":"v0.9.15-rgb"
cd ../demo && cargo build && ./target/debug/ark-rgb-demo --stage1   # about 10 min, ends "STAGE 1 RESULT: PASS"
```

### Stage 1 pass log (25 Oct 2026, about 13:42–13:52 CET), `logs/stage1-pass.log`, `state/run-1792932157-stage1/summary.json`

| step | evidence |
|---|---|
| Alice VTXO V | `cea6eb53d2c76b6e5e5c359a9955afcc37852c95e0d60c84e2f02e201f53a9b8:0` |
| genesis UTXO (stock rgb-lib, mined) | `b5f324528469233bb0dbe107f4b7e7c8c1437a7de5df60cb463422c5406fd6a5:0` |
| jump-in T0 → explicit seal V (mined) | `6eee26425a6d217e31b328113313a8ecbb81e2149a00ca54cf424e479b41c43c` |
| asset | `rgb:~92taDnN-RKPlyWv-HHrwtUQ-z2twOtz-FX0fNBl-Jmev8K0` (1000) |
| A→B checkpoint (T1, opret `6a20f211…fee2` at vout 0) | `faaedf01478a737a1c538dc1471aec221c9e9a44b86ef958abc822afa5ab580c` |
| A→B ark tx (T2, 0-sat opret at vout 0; Bob 20 000 sat at vout 1) | `253ff3e944f2b267d4a96acb46968bbe7d2b4da8461f39a871bd90ce3fc7bfd2` |
| Bob accept (2 offchain witnesses, confirmed by arkd GetVirtualTxs) | `[Fungible(1000)]` |
| B→C checkpoint (T1) | `9094516f1836b47253b3911f2dfde95888c97319e742b99fbb12b1ef73762791` |
| B→C ark tx (T2: Carol 700 at vout 1, Bob 300 at vout 2) | `c55252ab79288676807f8ebf13b139bc64f8abe0545db0776c6ffb6389675de9` |
| Carol accept (4 offchain witnesses, Tentative) | `[Fungible(700)]` |
| Carol exit branch, all mined | leaf `cea6…a9b8` → `faae…580c` → `253f…bfd2` → `9094…2791` → `c552…5de9`. Carol broadcast the leaf and both ark txs; arkd broadcast both checkpoints itself. |
| Carol→Dave (on-chain, after the 512 s CSV) | `06c59cd7e502655c15aa4e84b0dc396f043937757405726c2b1db8f2392f9768` |
| Dave | `Settled`, `settled: 700` → **PASS** |

On chain, checkpoint `faae…580c` has outputs `[(0, 6a20f211…fee2), (100000 sat, 5120…), (0, 51024e73)]`.

### Stage 0 regression on `arkd:rgb` (same demo code, still with the 1-sat sub-dust opret and a checkpoint without payload)

| run | log | ark tx | Bob→Carol | Carol |
|---|---|---|---|---|
| Variant A (VTXO genesis), 13:52–14:02 CET | `logs/stage1-regress-stage0-variantA.log` | `dd4ba7e01b98c433cf87b617b9b35b8c3ffba6fdfd613d17ffcc0db9d00b9b06` (checkpoint == prediction `6b779d82…e793`) | `d1f9ccd091dd1bb4a8cb600866ad9516e6707dff49d21c1cfabf87ee45db9fc7` | `settled: 1000`, **PASS** |
| Variant B (on-chain genesis + jump-in), 14:02–14:15 CET | `logs/stage1-regress-stage0-variantB.log` | `a4093088ed0377a075ca20605af9bdd69a5ed7a0f31993edb27f0dc43e03afd3` | `81ed25395f36aed47604d931adba6c810d48ade4d668e7d26d9c732a80fb375b` | `settled: 1000`, **PASS** |

So the upstream layout and the sub-dust path are unchanged by the patch.

## Stage 2: RGB commitment in the forfeit tx (refresh + collaborative exit)

**Problem.** In Stage 0/1 a colored VTXO `V` could only be spent through its offchain path (checkpoint). Refreshing `V` into a new batch, or exiting it collaboratively, spends it with the **forfeit tx** `F`, and the asset was lost.

**Design: the forfeit tx carries the commitment.** `F = [connector, V] → [OP_RETURN <32B> (0 sat), forfeit P2WPKH, anchor P2A]` commits a transition `T_f`: `V` → the new seal.
- Refresh: the seal is `V'`, the leaf output of the new VTXO tree that pays the refresh address.
- Collaborative exit: the seal is `C:vout`, the commitment-tx output that pays the exit address.

Both seals are known at `BatchFinalization` (commitment tx + VTXO tree are final before forfeits are signed), so there is no circularity. The seal is an **explicit** outpoint; `F` is the witness.

- **Why the forfeit and not something else.** `F` is the *only* tx that spends `V` in this path, so it is the only place a seal on `V` can be closed. No arkd output shape has to change for the users (the batch outputs are untouched).
  - Alternatives:
    - Committing in the commitment tx would need an arkd-built, multi-user OP_RETURN/MPC aggregation.
    - A "pre-refresh" offchain hop to self (checkpoint + ark tx, Stage 1 machinery) leaves the asset on an ark-tx output that is then forfeited, which is the same problem one level down.
- **Trade-off.** `F` is broadcast only if the operator must punish a double spend, so in the honest case it is **never mined**. RGB then sees it as a permanently Tentative witness. Every validator downstream therefore needs **Ark-aware attestation**:
  - `C` is mined (esplora);
  - `F.txid ∈ GetForfeitTxs(C)` (arkd indexer);
  - `F`'s connector input `∈ GetConnectors(C)`;
  - `F` spends `V` and has exactly one 0-sat `OP_RETURN <32B>` at index 0.

  The attested tx is then registered in rgb-lib, which resolves it as Tentative. The safe-height wait uses the confirmations of `C`.
- The VTXO owner signs `F` with SIGHASH_DEFAULT, so the signature commits to the OP_RETURN. arkd rebuilds the forfeit deterministically and compares txids, so it cannot strip or alter the commitment without invalidating the user's signature.

### Patches (cumulative files in `patches/`; Stage 1 versions in `patches/stage1/`)

1. **arkd patch 4, forfeit OP_RETURN** (`patches/arkd-patch4-forfeit-only.patch`, 2 files, +62/−1; whole `arkd.patch` now 9 files, +168/−11). Version `v0.9.15-rgb2`.
   - `pkg/ark-lib/tree/forfeit_tx.go`:
     - `BuildForfeitTxWithOpReturn(…, opReturn)`: with a payload it outputs `[OP_RETURN <32B> 0 sat, forfeit, anchor]`; without one the tx is identical to `BuildForfeitTx`.
     - `ExtractForfeitOpReturn(tx)`: at most one OP_RETURN, which must be 0 sat and `OP_RETURN PUSH32`.
   - `internal/infrastructure/tx-builder/covenantless/builder.go` `VerifyForfeitTxs`: extracts the payload and feeds it to the deterministic rebuild. The existing txid comparison covers the position, value and payload.
   - **No other change was needed.**
     - The fraud path (`fraud.go` `broadcastForfeitTx`) finds the anchor by script (`txutils.FindAnchorOutpoint`) and works on inputs.
     - The sweeper and indexer never index forfeit outputs.
     - `GetForfeitTxs` returns the txids unchanged.
2. **rust-sdk** (whole patch 8 files, +341/−17; Stage 2 part ≈ +170):
   - `ark-core`: `create_and_sign_forfeit_txs_with_hook(…, hook(vtxo_outpoint, &mut Psbt))`. The hook runs on each unsigned forfeit before `witness_utxo`/sighash. The old function delegates with a no-op.
   - `ark-client`:
     - `ForfeitHookContext { vtxo, commitment_tx, vtxo_tree_leaves }` (re-exported);
     - `join_next_batch_with_hook`;
     - `settle_vtxos_with_forfeit_hook(rng, vtxos, to: ArkAddress, hook)` (refresh);
     - `collaborative_redeem_vtxos_with_forfeit_hook(rng, vtxos, to: Address, amount, hook)` (collaborative exit).
   - `ark-grpc`: `get_forfeit_txids(commitment)` and `get_connector_txids(commitment)` (indexer).
3. **rgb-lib** (whole patch 4 files, +514/−8; Stage 2 part ≈ +150):
   - `ArkAwareResolver` (`utils.rs`, replaces Stage 1's `MultiOffchainResolver`): a mined status wins; otherwise attested and bundled offchain witnesses resolve as `Tentative`; everything else goes to the blockchain resolver.
   - It is used in **every** validation path: `update_witnesses`, the receive/`refresh` validation (`online.rs`), `accept_transfer`, and `accept_transfer_consignment_offchain`. Without it, `update_witnesses` would Archive the forfeit witness and invalidate the asset.
   - `Wallet::add_ark_attested_witness(tx_hex, anchor_txid)` persists to `<wallet_dir>/ark_attested_witnesses.json`.
   - `wait_safe_height` uses the anchor's height for an attested, unmined witness (see Problem solving below).
   - `Wallet::validate_consignment_ark(online, path, ark_aware)` is test-only: it validates without accepting, with the stock or the Ark-aware resolver.
4. **demo** `src/stage2.rs`, three modes:
   - `--stage2-refresh` and `--stage2-exit`. The forfeit hook inserts a bare OP_RETURN at index 0, colors a **clone** with `color_psbt_and_consume_custom` (`SealTarget::Explicit(V' | C:vout)`), and copies back only the commitment script, so no RGB PSBT fields reach arkd.
   - `attest_forfeit()` implements the checks above.
   - `--stage2-neg` runs the negative tests.

### Problem solving

- **`WaitingSafeHeight` forever (first (b) run, `logs/stage2-exit-run1.log`).** The receiver validated the consignment with the attested forfeit, but the RGB validator raises an `UnsafeHistory` warning for the Tentative ancestor `F`. rgb-lib then waits for `F` to reach the safe height, which never happens because `F` is never mined.
  - Fix: the attestation stores the anchor (`C`), and `wait_safe_height` checks `C`'s height instead. `F` can only be replaced if `C` is reorged out, because its connector input exists only in `C`'s connector tree.
  - Second run: `WaitingSafeHeight → Settled` one block later.

### Reproduce

```bash
cd /workspace/rgb-arkade/vendor/arkd && git apply ../../patches/arkd.patch   # on a clean v0.9.15 worktree
CGO_ENABLED=0 go build -ldflags="-X 'main.Version=v0.9.15-rgb2'" -o ../../arkd-image/arkd ./cmd/arkd
docker build -t arkd:rgb ../../arkd-image
cd /workspace/rgb-arkade/arkade-regtest && node regtest.mjs start --profile ark
curl -s localhost:7070/v1/info                     # "version":"v0.9.15-rgb2"
cd ../demo && cargo build
./target/debug/ark-rgb-demo --stage2-exit          # ~1 min,  "STAGE 2 (b) COLLABORATIVE EXIT RESULT: PASS"
./target/debug/ark-rgb-demo --stage2-refresh       # ~10 min (512 s exit delay), "STAGE 2 (a) REFRESH RESULT: PASS"
./target/debug/ark-rgb-demo --stage2-neg           # ~2.5 min, "STAGE 2 NEGATIVE TESTS RESULT: PASS"
```

### (a) Refresh → hop → unilateral exit → on-chain, 25 Oct 2026 22:20–22:29 CET (`logs/stage2-refresh-pass.log`, `state/run-1792963200-stage2-refresh/summary.json`)

| step | evidence |
|---|---|
| Alice VTXO V (after jump-in `da7ecbea…5592`, mined) | `d9ac1f0dcd1bd93f61322886eb1615a46fd9cfe4e94c0b5b87771a1042b69be6:0` |
| asset | `rgb:G4r6Bqc2-BrP2tL~-17aTq82-y7dXnXt-VQ8XaLq-5oXG1Vc` (1000) |
| refresh batch, commitment tx C1 (mined) | `ef6fd7210843eb3713efc5161e33bd6f1c702d26b88e574cacab97147fb46716` |
| forfeit F (T_f: V → V'), outputs `[(0, 6a20acee…80b5), (100330, P2WPKH forfeit), (0, P2A)]`, **never mined** | `9e13241a971c0908381ce50608e2b932b2539306e111a24f288849ee6d50bdbe` |
| V' (leaf of C1's VTXO tree) | `0c68de737f5decc5be82f51db71bd185417f16f83517f1caf715f251b6730545:0` |
| Alice/Bob attestation | F ∈ `GetForfeitTxs(C1)` (1 forfeit), connector ∈ `GetConnectors(C1)`, C1 mined |
| Bob, before attestation | `validate_consignment_ark` → `InvalidConsignment` |
| hop V' → Bob: checkpoint (T1) / ark tx (T2) | `df3530e1775b22a4285edc04bd4b8c167716d9c801caef7e86b5ed630dd8fe88` / `bcb20f4f0425ee7c0cf4f952a056d7728054ba4ce5892318669aac4a458d0cf2` |
| Bob accept | `[Fungible(1000)]` |
| Bob exit branch, all mined | `0c68…0545` (V' leaf) → `df35…fe88` → `bcb2…0cf2`; F still unmined |
| Bob → Carol on-chain (after CSV) | `75e98bbae798d8a2942878f85315ea98170a63f29276915598f0391655d89175` |
| Carol | stock resolver: rejects; Ark-aware before attestation: rejects; after attestation: valid → `Settled`, `settled: 1000` → **PASS** |

### (b) Collaborative exit → on-chain, 25 Oct 2026 22:19 CET (`logs/stage2-exit-pass.log`, `state/run-1792963155-stage2-exit/summary.json`)

| step | evidence |
|---|---|
| Alice VTXO V (after jump-in `ed97542b…3dc0`, mined) | `0ebecc48f9121b3998a8a43d0b7febacb31ca364c2d9b8a3c7255770a9da643a:0` |
| asset | `rgb:Wi8XwFQL-qBPoD~Z-BspciZc-JhsZsxw-NiJwNAP-AnFC_I4` (1000) |
| exit batch, commitment tx C (mined); O = C:2 pays 20 000 sat to Alice's rgb-lib address | `d0906ea1cfc78bb17f172ddeac65fe8773fd9db5eea1ad8f8dfeae3d669d55c4` |
| forfeit F (T_f: V → C:2), outputs `[(0, 6a200288…34e6), (100330, forfeit), (0, P2A)]`, **never mined** | `99b9db410e45982c22f129a134cf3101b238be7033a4f11ec31e68923f314917` |
| Alice spends O → Dave (`[opret, Dave 2000 sat, change]`, mined) | `38d1d6e8b9b2aaff73ce0721cf62f7e39af18002c218fe083b2964f94dbdc6f8` |
| Dave | stock: rejects; unattested: rejects; attested: valid → `Settled`, `settled: 1000` → **PASS** |

### Negative tests, 22:29–22:32 CET (`logs/stage2-neg-run1.log`, `state/run-1792963787-stage2-neg/summary.json`): all PASS

| case | result |
|---|---|
| forfeit OP_RETURN 31 B | arkd `INVALID_FORFEIT_TXS: invalid forfeit OP_RETURN output #0` |
| forfeit with 2 OP_RETURNs | arkd `INVALID_FORFEIT_TXS: forfeit tx has more than one OP_RETURN output` |
| forfeit OP_RETURN at index 1 | arkd `INVALID_FORFEIT_TXS: invalid forfeit tx: expected txid …, got …` (rebuild puts it at 0) |
| forfeit OP_RETURN with 1 sat | arkd `INVALID_FORFEIT_TXS: invalid forfeit OP_RETURN output #0` |
| control: well-formed 32 B forfeit OP_RETURN | accepted (C `0cfc4225…d1c8`, F `e2aa3d82…533f`), attestation OK |
| attestation claiming the wrong VTXO | rejected |
| forfeit submitted **without** OP_RETURN, attestation of the would-be committed F | rejected: `not among the 1 forfeits of batch 892deabc…0758` |
| attestation against an unmined "commitment" | rejected |
| checkpoint OP_RETURN 31 B / 2 OP_RETURNs / 1 sat (ark tx input re-pointed at the tampered checkpoint) | arkd `CHECKPOINT_MISMATCH: invalid checkpoint OP_RETURN …` / `… more than one OP_RETURN output` |
| RGB validator (in (a) and (b)): history with an unmined forfeit, stock resolver or not attested | `InvalidConsignment` (Bob, Carol, Dave) |

### Regressions on `v0.9.15-rgb2` (same binary as the Stage 2 runs)

| run | time (CET) | log | key txids | result |
|---|---|---|---|---|
| Stage 1 (`--stage1`) | 22:32–22:41 | `logs/stage2-regress-stage1.log`, `state/run-1792963932-stage1/` | A→B ark tx `d0e16f11…6d66`, Carol→Dave `9e7f7c5af525640643c469ccc40383736f041ad2a7790ec9a503496a90b0cac9` | Dave `settled: 700`, **PASS** |
| Stage 0 Variant A (VTXO genesis) | 22:41–22:50 | `logs/stage2-regress-stage0-variantA.log`, `state/run-1792964499/` | ark tx `d520f8b6…3641` (checkpoint == prediction `63fdfd0d…4455`), Bob→Carol `25163d9af85efae9911adc7d6b3669766a1421d1815449c67e9c82bfcf1ed40b` | Carol `settled: 1000`, **PASS** |
| Stage 0 Variant B (on-chain genesis + jump-in) | 22:50–23:00 | `logs/stage2-regress-stage0-variantB.log`, `state/run-1792965049-onchain-genesis/` | ark tx `b6dfa6e3…d0a5`, Bob→Carol `1aaceb5fc0216644df89668affc8f71820c2be088d491bd62d775305bc7b338d` | Carol `settled: 1000`, **PASS** |

The Stage 0/1 paths therefore work unchanged with patch 4 and with the Ark-aware resolver now used in every rgb-lib validation path.

## Stage 3: self-contained Ark proofs + exit watchtower

### Part A: offline Ark proofs (no arkd needed to validate)

**What the receiver gets.** The consignment stays byte-identical. Next to it the sender ships a sidecar `<consignment>.arkproof.json`:

```json
{ "version": 1, "witnesses": ["<txid of every unmined RGB witness>"], "psbts": ["<base64 PSBT>", ...] }
```

`psbts` holds every unmined witness (checkpoint, ark tx, forfeit) and every unmined ancestor back to a mined tx: VTXO-tree branch txs, connector branch, earlier hops. The sender collects them with `GetVirtualTxs` (`demo/src/proof.rs::build_proof`) and carries the proofs it received itself. For example, Bob forwards Alice's forfeit and hop txs to Carol, so later senders don't need arkd's history either.

**Why a sidecar.** RGB consignments are strict-encoded and commitment-identified, so adding a field changes the format and the consignment id. The sidecar is transport-agnostic, optional for fully mined histories (the proof is then empty, 39 B), and easy to drop once Ark proofs get a standard container.

**Receiver** (`rgb-lib` `src/ark_proof.rs`, `Wallet::import_ark_proof(online, path, operator_pubkeys)`): it verifies the proof using only the chain (electrum). It then registers each proven witness in the Ark-attested store, with the highest mined ancestor as its anchor. After that, the normal validation (`accept_transfer_consignment_offchain` with an **empty** off-chain txid list, `validate_consignment_ark`, `provide_out_of_band_consignment` + `refresh`) runs unchanged.

What the verifier checks for every tx it is asked to trust:

| check | how |
|---|---|
| linkage | each input's parent is either in the bundle (checked recursively, and the referenced output must exist) or mined (`resolve_witness` → `Mined`); otherwise: *"neither in the proof nor mined (missing branch tx or unmined commitment tx)"* |
| on-chain anchor | only the commitment tx (or other mined ancestor) is looked up on-chain; mempool does not count |
| OP_RETURN layout | every listed witness: exactly one OP_RETURN, 0 sat, 32-byte push, at output 0 |
| key-path sigs (VTXO-tree txs) | BIP341 key-spend sighash against the prevout's P2TR key (`tap_key_sig`); sighash DEFAULT/ALL only |
| script-path sigs (checkpoint, ark tx, forfeit VTXO input) | a fully signed tapleaf whose script contains a **pinned operator key** (`signer_pk` / `forfeit_pk`, wallet config); control block checked against the prevout (`verify_taproot_commitment`); every signature verified |
| forfeit | needs the operator's co-signature on the VTXO input (arkd **patch 5**). Without it a sender could fabricate an `F'` with an invented unsigned connector and double-spend offline |

**Not checked (documented limits).** Connector txs are unsigned in arkd's store, so they are checked by linkage only. They are vouched for by the operator-signed forfeit that spends them, and they end in the mined commitment tx. MuSig membership of VTXO-tree nodes is not checked: the root's signature against the commitment output authenticates the chain inductively. Expiry/sweep state of the batch output is not checked: a proof says "this was valid in Ark", not "this is still exitable". Freshness against a double-spend is the usual Ark operator assumption (Stage 2 caveat).

**arkd patch 5** (`patches/arkd-patch5-forfeit-cosign.patch`, image `v0.9.15-rgb3`): in `finalizeRound`, the operator co-signs every stored forfeit (`SignTransactionTapscript`), so `GetVirtualTxs(F)` returns an `F` with user + operator signatures on the VTXO input. (It also carries the operator's key-path signature on the connector input; the connector tx itself stays unsigned.) The signed `F` is broadcastable once the connector branch and V are on-chain, which only ever benefits the operator. Before the patch, `F` carried just the user's signature.

**Proof size** (JSON + base64 PSBTs; raw PSBT bytes are about ¾ of this):

| hop | witnesses | PSBTs | proof | consignment |
|---|---|---|---|---|
| Stage 1 A→B (off-chain) | 2 (cp, ark) | 3 (+ tree leaf) | 2 932 B | 6 092 B |
| Stage 1 B→C (off-chain, carries A→B) | 4 | 5 | 5 432 B | 7 153 B |
| Stage 1 C→D (on-chain after exit) | 0 | 0 | 39 B | – |
| Stage 2a A→B (forfeit + hop) | 3 (F, cp, ark) | 6 (+ connector, tree) | 4 720 B | 6 656 B |
| Stage 2a B→C (on-chain; F still unmined) | 1 (F) | 3 | 1 825 B | 7 182 B |
| Stage 2b A→D (collab. exit, on-chain) | 1 (F) | 3 | 1 825 B | 6 144 B |

So each off-chain hop adds about 2.5 kB, and an unmined forfeit with its connector and tree leaf about 1.8 kB. The proof stops growing once the hops get mined (exit), but an unmined forfeit stays in every future proof.

**Runs, arkd paused (`docker pause arkd`, `curl /v1/info` → unreachable, recorded as `*_arkd_reachable_during_validation = false`) for every receiver validation:**

| run | time (CET) | log / state | key txids | result |
|---|---|---|---|---|
| Stage 1 `--stage1 --offline-proof` | 25 Oct 23:48–23:58 | `logs/stage3-stage1-offline-run2.log`, `state/run-1792968526-stage1/` | A→B cp `97292fe3…ab32` ark `c6485d48…020f`; B→C cp `2c55edc4…99f2` ark `05241651…27cd`; Carol→Dave `174e40a5194ee118de68f7b9b88ba4953c51131967ae2f0309e8cc27bb6c48b6` | Bob, Carol accept offline; Dave `settled: 700`, **PASS** |
| Stage 2a `--stage2-refresh --offline-proof` | 26 Oct 00:00–00:09 | `logs/stage3-stage2-refresh-offline-run3.log`, `state/run-1792969212-stage2-refresh/` | C1 `87b604da…cb2c`, F `7022c754…2bec`, hop cp `e6b620f3…2493` ark `836f000d…eb74`, Bob→Carol `572d33e522a29008bcb2140b105a50099534cc312c208bd59c72771d16085342` | Bob accepts offline; Carol `settled: 1000` (arkd paused through settle), **PASS** |
| Stage 2b `--stage2-exit --offline-proof` | 26 Oct 00:10–00:11 | `logs/stage3-stage2-exit-offline-run2.log`, `state/run-1792969857-stage2-exit/` | C `e614b677…dadc`, F `00514b7c…1bc9`, Alice→Dave `e47e036f9204e93ac93cfdc2003c596a8b8e1c586ab9cd058a3d005361e7d058` | Dave `settled: 1000` (arkd paused), **PASS** |

**Negative proofs** (each run imports a mutated copy, `*.arkproof.neg-<case>.json` in the state dir; all **rejected**):

| case | rejected with |
|---|---|
| missing branch tx (tree/connector PSBT removed) | `tx … is neither in the proof nor mined` |
| tampered branch, signature bit flipped | `input 0 key-path: invalid signature` |
| tampered branch, output value changed | txid changes → child links to a missing tx |
| forfeit not linked to its connector (connector removed, or replaced by a fabricated one) | `tx <connector> is neither in the proof nor mined` |
| forfeit without operator co-signature | `input 1: no fully signed tapleaf containing the operator key` |
| forfeit PSBT missing | `tx … missing from the proof` |
| wrong pinned operator key | `no fully signed tapleaf containing the operator key` |
| unmined commitment tx (2b: proof built and imported before `C` was mined) | `tx e614b677… is neither in the proof nor mined` |

Bob (Stage 1) ran the tree cases; Bob (2a) and Dave (2b) ran all cases.

Fix found on the way: `Regtest::tx_confirmations` returned `Some(0)` for mempool txs. As a result `mine_until` and the proof builder treated a mempool commitment tx as mined, and the verifier then correctly refused it (run2 of 2a). It now returns `None` for unconfirmed txs.

### Part B: exit watchtower

**Design** (`demo/src/stage3.rs`, `--watchtower`):

- **Registration.** The user hands the watchtower the already fully signed exit branch of each colored VTXO (`watchtower-registration.json`: VTXO outpoint + raw txs). These are public data; the watchtower gets no user key.
- **Watching.** The watchtower has its own P2WPKH fee key. Every 5 s it asks arkd's **public** indexer (`GetVtxos` by outpoint) for `expires_at` and refresh status (`is_spent` / `spent_by`).
- **Trigger.** At `expires_at − 420 s`:
  - if the VTXO was refreshed (`spent_by` = the forfeit), it does nothing;
  - otherwise it broadcasts the branch tx by tx. Each tx goes out as a v3 package `[branch tx (0 fee), CPFP child]`, where the child spends the P2A anchor plus the watchtower's UTXO, at 2 sat/vB (`submitpackage`), and the next tx follows after confirmation.
- **When the indexer is unreachable** it keeps watching. A production watchtower should then fall back to broadcasting at the threshold.
- **Short expiry for the test.** arkd `v0.9.15` keeps its settings in the DB, so `ARKD_VTXO_TREE_EXPIRY` in `.env` only seeds them. The expiry was switched with the local admin API: `POST localhost:7071/v1/admin/settings {"settings":{"vtxoTreeExpiry":"1024"}}`. It was restored to `604672` afterwards and `.env` is unchanged.

**Run** 26 Oct 00:20–00:50 CET, `logs/stage3-watchtower-run2.log`, `state/run-1792970449-stage3-watchtower/` (`summary.json` + `post-run-evidence.json`). Alice and Erin boarded into the same batch `ebc51bfd…3c9f` (tree root `e0130bc9…16a3`, leaves `6ff43d27…c5d6` Alice and `3f192abe…ce48` Erin), with expiry 1019 s. Frank was in batch `5709e18f…4d94`. Each got its own asset via on-chain genesis + jump-in.

| test | result | evidence |
|---|---|---|
| (1) watchtower exits Alice's V before expiry, Alice spends on-chain, receiver settled | **PASS** | trigger at 420 s left (00:30:57 CET). Root `e0130bc9…16a3` + CPFP `01575ecf…46e1`, leaf `6ff43d27…c5d6` + CPFP `5894303b…b94d`, both confirmed about 5 min before expiry (00:37:57 CET). After the 512 s CSV, Alice→Gina `b44ef81f3c12680f16272d4cf8a87686b3e4cc163dbd8e9a3604209099ec9aab`; Gina `settled: 1000` |
| (2) control: Erin not watched → swept, asset stuck | **PASS** (asset unrecoverable) | arkd swept Erin's part of the tree: sweep tx `6886cca82d871a4a9be4ef01fe1fa6ef8a3e0d40f5effa017532a14e7ef200c9` (block 2550, 00:48:20 CET) spends `e0130bc9…:0`. Erin's leaf tx is now `missing-inputs`. **RGB still reports `Balance { settled: 1000, future: 1000, spendable: 1000 }`** (stale: the seal `3f192abe…:0` can never be spent). Colored refresh of the swept VTXO: the BTC is recovered (new VTXO `5c217119…88a6:0`, 100 000 sat, batch `40e572de…2ec3`) but `forfeit hook never ran`: a swept VTXO is recovered without a forfeit, so there is no tx to carry an RGB transition and the asset stays on the dead seal |
| (3) watchtower cannot move the asset | **PASS** | (a) branch root with its output redirected to the watchtower: `mempool-script-verify-flag-failed (Invalid Schnorr signature)`, because the branch signatures commit to the outputs. (b) Spending Alice's on-chain V with the watchtower key: `testmempoolaccept` → `Invalid Schnorr signature`. Both V leaves need Alice's key (exit: Alice + CSV; forfeit: Alice + operator) |
| (4) Frank refreshes in time → watchtower does nothing | **PASS** | colored refresh: C `9e26661a…4490`, F `5ffa5093…6ce0` (T_f committed). At 416 s left the indexer showed `spent_by = 5ffa5093…`, so no broadcast; Frank's branch `95cca8fe…21d4` never hit the chain |

**Watchtower fees** (2 sat/vB package rate; branch txs pay 0, the CPFP child pays all):

| hop | branch tx vsize | child vsize | fee |
|---|---|---|---|
| tree root `e0130bc9…` | 167 vB | 151 vB | 640 sat |
| leaf (V) `6ff43d27…` | 124 vB | 151 vB | 554 sat |
| **total for a depth-2 branch** | | | **1 194 sat** |

Each extra tree level or off-chain hop (checkpoint + ark tx) adds one package of roughly 550–650 sat at this rate. The owner then pays the normal on-chain spend (600 sat in the demo).

**Side effect observed.** Broadcasting a shared branch moves the expiry of the sibling outputs. Erin's subtree became sweepable only 1024 s after the root confirmed (not after the original batch expiry), because each tree output's sweep leaf is a relative timelock.

### Reproduce Stage 3

```sh
cd /workspace/rgb-arkade/demo && cargo build
# Part A (arkd is paused/unpaused by the demo itself with `docker pause arkd`)
./target/debug/ark-rgb-demo --stage1 --offline-proof
./target/debug/ark-rgb-demo --stage2-refresh --offline-proof
./target/debug/ark-rgb-demo --stage2-exit --offline-proof
# Part B: short batch expiry (DB setting, the .env value only seeds it), run, restore
curl -s -X POST -H 'Content-Type: application/json' localhost:7071/v1/admin/settings -d '{"settings":{"vtxoTreeExpiry":"1024"}}'
./target/debug/ark-rgb-demo --watchtower          # ~30 min (expiry + 512 s CSV + sweep)
curl -s -X POST -H 'Content-Type: application/json' localhost:7071/v1/admin/settings -d '{"settings":{"vtxoTreeExpiry":"604672"}}'
```

The image is `arkd:rgb` = `v0.9.15-rgb3` (patches 1–5). `arkd:rgb-stage2` (rgb2) and `arkd:rgb-stage1` are kept for comparison.

### Regressions on `v0.9.15-rgb3` (7-day expiry restored)

| run | time (CET, 26 Oct) | log / state | key txids | result |
|---|---|---|---|---|
| Stage 1 (`--stage1`) | 01:10–01:20 | `logs/stage3-regress-stage1.log`, `state/run-1792973439-stage1/` | Carol→Dave `abf54ea4e9772cedcfb857249d4799ab4213ac3c83c53e5e2323b6d5e088c0ed` | Dave `settled: 700`, **PASS** |
| Stage 0 Variant A | 00:49–00:58 | `logs/stage3-regress-stage0-variantA.log`, `state/run-1792972177/` | Bob→Carol `b0bc2aa6bd70409fbb875271cdf7a56a32273e639c4700f8f9d456cdb61946cc` | Carol `settled: 1000`, **PASS** |
| Stage 0 Variant B | 00:58–01:08 | `logs/stage3-regress-stage0-variantB.log`, `state/run-1792972727-onchain-genesis/` | Bob→Carol `84a6952a8478895d02bcccb878fd664ed287c2c44e6ec409e6e5d7d259289f7b` | Carol `settled: 1000`, **PASS** |
| Stage 2 (a) refresh | 00:49–00:59 | `logs/stage3-regress-stage2-refresh.log`, `state/run-1792972168-stage2-refresh/` | C1 `afdf9665…5021`, Bob→Carol `b5cf9c253213ae38dd39e84c111fa5ead237e7efd7a357175e50fcd9c5697402` | Carol `settled: 1000`, **PASS** |
| Stage 2 (b) collaborative exit | 00:59–01:00 | `logs/stage3-regress-stage2-exit.log`, `state/run-1792972747-stage2-exit/` | C `073620dc…e6ba`, Alice→Dave `795798dc4636b6e857dbbb60afc28a3805e4571ceecc4a025f328ba582fa6b60` | Dave `settled: 1000`, **PASS** |
| Stage 2 negatives | 01:08–01:10 | `logs/stage3-regress-stage2-neg.log`, `state/run-1792973296-stage2-neg/` | – | all **PASS** |

The first Stage 1 attempt (`logs/stage3-regress-stage1-run1-electrum-fail.log`) died at wallet start with `Invalid indexer: not a valid electrum nor esplora server`. Two other runs were starting against Fulcrum at the same moment, and the same binary passed when re-run alone. These regressions ran in the default (arkd-online) mode on the final binary, which includes the `tx_confirmations` fix.

## Stage 4: 2 weeks offline without losing the asset + out-of-band size/latency (USDt-like)

### A. Design: who keeps the asset alive while the user is offline

A colored VTXO dies with its batch: after the expiry the operator sweeps the tree output, the RGB seal can never be closed and the asset is gone, while rgb-lib keeps showing it as settled. Options:

| Option | Third-party trust for the asset | Who pays / cost | If the helper is offline | Verdict |
|---|---|---|---|---|
| (a) long batch expiry (E ≥ 14 d + margin, e.g. 28 d) + wallet policy "refresh on every open if less than 14 d + margin is left" | none | operator capital: VTXO value × rate × E. Colored VTXOs carry little BTC: 1 000 sat × 5 %/yr × 28/365 ≈ 4 sat; but `vtxoTreeExpiry` is **global** in arkd, so BTC users' VTXOs are locked 4× longer too | nothing to be offline | **default** (needs a KaleidoSwap arkd or a per-batch/per-asset expiry from Ark Labs) |
| (b) keyless exit watchtower with a user-presigned fee ladder | none: it holds only the public exit branch + CPFP children signed by the user's own fee key (rates 1–64 sat/vB). It cannot redirect or steal; worst case it griefs (unrolls early, burns the user's fee UTXOs) | the user: d prefunded fee UTXOs (d = branch depth), exit fees ≈ Σ(parent + 151 vB child) × feerate (measured 554 sat/level at 2 sat/vB) | that watchtower does nothing; mitigation: register with N independent watchtowers (registration has no secrets) | **safety net** for users who stay away longer than (a) allows |
| (c) delegated refresh | SIGHASH_NONE/ANYONECANPAY on the forfeit lets the delegate (or operator) redirect the asset → violates "no trust". A pre-built future batch (operator presigns C' now so V' is known and the RGB transition can commit to it) needs no RGB consensus change but large arkd changes and costs the same liquidity as (a) | operator | – | **later**, only with operator support (Ark Labs) |
| (d) combo | – | – | – | (a) + (b) now, (c) later |
| (e) on-chain fallback ("vacation mode": collaborative exit to an on-chain UTXO before leaving) | none, no expiry at all | one batch output (~43 vB) + one input to board back (~58 vB): ≈ 1 000 sat at 10 sat/vB | – | **manual option** for planned absences |

Recommendation for KaleidoSwap: (a) as default (28-day batches for the KaleidoSwap operator; the wallet refreshes automatically whenever it is opened with less than 14 d + margin left, so 14 days offline need nobody), (b) as an automatic safety net run by KaleidoSwap (anyone else, including the user's own node, can run more copies), (e) offered in the UI, (c) to be discussed with Ark Labs.

### A. Prototype: `watchtowerd` + 2-weeks-offline simulation (26 Oct 2026, 05:07–05:46 CET), all PASS

- `demo/src/wtd.rs`: standalone daemon `ark-rgb-demo --watchtowerd <dir>`; registrations are files in `<dir>/registrations/*.json`, status persisted in `<dir>/status/`, log `<dir>/daemon.log`. Decides at `expires_at - threshold`: refreshed (indexer `spent_by`) → nothing; swept → too late; otherwise submits `[branch tx, presigned child]` level by level, climbing the fee ladder on rejection. Indexer calls have 5 s timeouts + reconnect; if the indexer is unreachable it fails safe and exits using the registration's own `expires_at`.
- `demo/src/offline2w.rs`: `--offline2w-setup <dir>` / `--offline2w-return <user> <dir>`; wallets are reloaded from disk (`ark_client_load`: seed + `restore_contracts`; `rgb_wallet_load`: mnemonic + existing data dir) in a new process. `colored_health()` = the cheap "flag swept assets" fix: HEALTHY / AT RISK / ON-CHAIN / LOST from the public indexer.
- Orchestrator `logs/run-stage4-offline2w.sh` (nohup), state `state/run-1792987633-stage4-offline2w/`, summary `state/stage4-summary.json`. Scale: expiry 2048 s = 28 d, window 1024 s = 14 d (1 d = 73 s); expiry set via admin API only during setup and restored to 604672 at 05:08:12.

| User | What happened | Result |
|---|---|---|
| alice (no third party) | 2037 s (27.8 d) left at departure → policy: no refresh needed. Offline 1149 s (15.7 d). Returned: health "AT RISK, 12.1 d left", collaborative exit `e7740f88…` (forfeit `af3e8ac0…`), on-chain pay to Dave `7b0f0f70…`, Dave validated with the offline proof (1825 B) | Dave settled 1000, PASS |
| erin (watchtower A) | registration 8641 B (2 levels × 7 rungs). Daemon killed/restarted 05:09/05:10 (resumed). arkd paused 05:34:44–05:36:14: at 05:35:10 "indexer UNREACHABLE → fail-safe exit"; leaf `42ce337d…` + presigned child `47ad2167…` (554 sat, 2 sat/vB) confirmed 05:35:18. Offline 2066 s (28.2 d, past expiry). Returned, `spend_exited` → Gina `17a80998…` | Gina settled 1000, PASS |
| frank (watchtower B killed at 05:10:42) | batch swept `6e7f4a5b…` | health LOST, rgb-lib still says settled 1000 (stale); control PASS |

Negatives: a watchtower that rewrites the presigned child's output is rejected by bitcoind (`Signature must be zero for failed CHECK(MULTI)SIG`); the registration contains no secret (checked against the fee key and the Ark seed). Side effect worth noting: that negative test's package made bitcoind accept Erin's 0-fee root `7a334ae0…` (TRUC parent) and regtest mined it at 05:07:53 — a live example of the griefing case (early unroll, not theft); the daemon then only had to push the leaf.

### B. Out-of-band payload and validation time (measured on regtest, 26 Oct 2026 05:11–05:50 CET)

Data: `state/bench-stage4/{hops,refresh,merge,onchain}.jsonl`, `fits.json`; `bench/analyze.py`; code `demo/src/bench.rs` (`--bench <dir> hops|refresh|merge|onchain <n> [--pause]`). Hops and refresh were validated with arkd **paused**. Cold = fresh verifier wallet (proof import + `validate_consignment_ark`), debug build, local electrum.

| Off-chain hops | Consignment raw / gz | Ark proof JSON (v1) | Compact binary / gz | Cold validation | Real receiver accept |
|---|---|---|---|---|---|
| 1 | 6 092 / 3 329 B | 2 932 B | 1 120 / 954 B | 156 ms | 219 ms |
| 2 | 7 130 / 3 859 | 5 432 | 2 061 / 1 536 | 184 | 306 |
| 4 | 9 206 / 4 903 | 10 432 | 3 943 / 2 653 | 180 | 355 |
| 8 | 13 358 / 6 982 | 20 432 | 7 707 / 4 875 | 238 | 734 |
| 16 | 21 662 / 11 112 | 40 432 | 15 235 / 9 340 | 368 | 827 |

Linear fits (measured range): consignment 5 054 + 1 038·h B; proof JSON 432 + 2 500·h; binary 179 + 941·h; binary+gzip 418 + 558·h; cold validation 140 + 14.5·h ms. Refreshes (r = 1..8): consignment 5 054 + 564·r, proof JSON +1 788·r (binary +827·r): a refresh does **not** reset the Ark proof (each forfeit needs its VTXO's branch). Mined on-chain chain (n = 1..16): consignment 5 062 + 526·n, no proof, 120 + 6.2·n ms. Merges of k on-chain inputs from k senders: k = 1/4/16 → 5 588 / 7 784 / 16 568 B, 171 / 169 / 319 ms (≈ +732 B and +10.6 ms per merged input of depth 1).

Where it comes from: the consignment is RGB history (≈ 0.5 kB per mined transition, ≈ 1 kB per off-chain hop = 2 transitions with their witness txs); the Ark proof is mostly PSBT overhead (unsigned txs are only ~340 B per hop of the 2 500 B JSON).

Optimizations (before → after, measured): compact binary proof (only unsigned tx + key-path sig or the one signed leaf, control block and sigs; decoded back and verified by the unchanged rgb-lib verifier at every hop) 40 432 → 15 235 B at 16 hops (−62 %), with gzip 9 340 B (−77 %); gzip of the consignment −49 %; receiver cache (send only txs the receiver has not verified) → ~1 kB binary per new hop instead of cumulative (2.8 kB at hop 16 vs 15.2 kB); decode cost is within noise (cold 336 vs 368 ms at 16 hops). Pruning of mined txs is already in v1 (a fully mined history costs 39 B). Not prototyped: operator-signed history checkpoints (would make the Ark proof O(1) but the receiver trusts the operator's signature that the history is valid), periodic on-chain compaction (collaborative exit + re-board resets the Ark proof to 0 but not the RGB history), batching.

USDt-like extrapolation (ESTIMATE from the fits, `bench/usdt_extrapolate.py`): a payment whose ancestry has A transitions, of which 10 are unmined off-chain: A = 10 → 15 kB + 9 kB binary proof, 0.27 s; A = 100 → 61 kB, 0.8 s; A = 1 000 → 0.5 MB, 6.4 s; A = 10 000 → 5 MB, ~1 min. With merges from many senders the ancestry is the union of the senders' histories, so for a high-velocity stablecoin A grows towards all transfers ever made. The Ark part stays bounded (≈ 1 kB per unmined hop, reset by going on-chain); the RGB history is the real limit and needs receiver-side caching of already validated history (delta consignments) or issuer-level resets — not an Ark problem.

### Reproduce Stage 4
```
logs/run-stage4-offline2w.sh            # ~40 min, sets/restores vtxoTreeExpiry itself
demo/target/debug/ark-rgb-demo --bench state/bench-stage4 hops 16 --pause   # also: refresh 8 --pause, merge 1|4|16, onchain 16
python3 bench/analyze.py; python3 bench/usdt_extrapolate.py
logs/run-stage4-regressions.sh          # Stage 0–3 regressions
```

### Stage 4 regressions on `v0.9.15-rgb3` (7-day expiry restored), 26 Oct 2026 05:51–06:38 CET (`logs/stage4-regress-*.log`)
| Run | Result | Final settled transfer |
|---|---|---|
| Stage 1 | PASS | Carol→Dave `6027bb3222f1859c999c3704a8cc24d6936f4671671a0cab39270bb266ebbab3` |
| Stage 0 A | PASS | Bob→Carol `4bf1570c96fb9fad6768ad553df25d19c4b88d8a4f4422e5750e92602d59510b` |
| Stage 0 B | PASS | Bob→Carol `4bee7b29b398ff7dde5d505e906210025e02834d18515bcd71c9af1a6d0e1bfc` |
| Stage 2 (b) | PASS | Alice→Dave `5c19608ac2259e8e40420ee5acf627baa0ae30455674b2664a06b2db1efa204a` |
| Stage 2 negatives | PASS | – |
| Stage 3 offline proof (Stage 1 with `--offline-proof`, arkd paused) | PASS | Carol→Dave `52d5765daab7ed2a920795fe60ecf3ae56c4d641b19d7c7de943f2fc6e857b3f` |
| Stage 2 (a) | run 1 failed at wallet start with the known electrum "Invalid indexer" flake on concurrent start (`stage4-regress-stage2-refresh-run1-electrum-fail.log`); rerun alone from 06:38:49: PASS (`stage4-regress-stage2-refresh-rerun.log`). It sat at "non-BIP68-final" until 07:20 only because nothing was mining blocks any more (the 512 s CSV needs median-time to advance); 12 blocks mined by hand, then it finished | Bob→Carol `64c3eea012aeb8d6bec0382f39aa8e440bdfb42524cec636710d9482d8547b7a` |

## Stage 5: sweep recovery: asset survives months offline, no watchtower, no longer expiry

Goal: a user offline for 2+ months, nobody acted, the batch expired and was swept. On return, the user recovers **both the BTC and the RGB asset** into a new VTXO with a normal refresh-like batch.

### How stock arkd v0.9.15 handles swept VTXOs (read in `vendor/arkd`)
- `domain.Vtxo.RequiresForfeit()` = `!Swept && !IsExpired() && !IsNote() && !Unrolled`. A swept or expired, unspent VTXO is "recoverable" (indexer: `!RequiresForfeit() && !Spent && !Unrolled`). `RegisterIntent` accepts it as a normal input of a BIP322-style intent proof (input 0 is the fake `toSpend`, inputs 1.. are the VTXOs, outputs are the receivers). It skips the tapscript validation and requires **no forfeit**. `Intents.CountSpentVtxos` also skips it, so it gets no connector. rust-sdk sets `is_swept` on the intent input and skips forfeit creation.
- Intent outputs: every output is a receiver, except at most one 0-sat "extension" `OP_RETURN` that starts with the Arkade magic. A plain `OP_RETURN <32B>` would be parsed as a receiver and rejected (amount below dust).
- Retention: there is no pruning of VTXO rows in v0.9.15 (`grep` for delete/prune/retention in core and db finds none). A swept VTXO stays recoverable for as long as the operator keeps its DB.

### Why the asset rides on a forfeit and not on the intent proof
The intent is signed **before** the batch exists, so it cannot name the new VTXO outpoint (leaf txid:vout). A `WitnessVout` seal on the intent tx would point at `(intent_txid, k)`, which never exists, so the next spend of V' could not close it. This is the same circularity as the Stage 3 delegator. The forfeit is signed **after** the tree is built, when V' is known. It already carries `T: V → V'` (Stage 2 refresh), and the operator already co-signs and stores it (patch 5). So the recovery only has to make arkd require a forfeit for the swept input.

### Patch 6 (arkd ≈ 25 lines, rust-sdk ≈ 40 lines; cumulative `patches/arkd.patch`, `patches/rust-sdk.patch`; Stage 4 versions in `patches/stage4/`; image `arkd:rgb` = `v0.9.15-rgb4`, previous image kept as `arkd:rgb-stage4`)
- `intent.RegisterMessage.RgbRecoveryForfeit` (`json:"rgb_recovery_forfeit,omitempty"`): opt-in, part of the signed message.
- `RegisterIntent`: if the flag is set and the input is swept or expired, then `vtxo.ForceForfeit = true`. `RequiresForfeit()` returns `!IsNote() && !Unrolled` for such VTXOs, so the input gets a connector and a mandatory forfeit, which patch 5 co-signs and stores. Stock recoveries without the flag are unchanged.
- rust-sdk: `ark_core::intent::RGB_RECOVERY_FORFEIT`. When it is set, register messages get the field appended last, to match arkd's re-encoding, and swept inputs are forfeited. There are also two test helpers (`rgb_settleable_inputs`, `rgb_sign_intent`).
- rgb-lib: **no change**. The recovery forfeit is just an operator-attested forfeit, verified by the Stage 3 `import_ark_proof` like a refresh forfeit.

### End-to-end (demo `--stage5-setup/--stage5-return`, `logs/run-stage5.sh <offline_s>`; expiry 600 s, arkd's minimum is 512 s, stands for a 7-day batch; restored to 604672 right after setup)
| Run | Offline | Steps | Result |
|---|---|---|---|
| dev, `state/run-1792996135-stage5-recovery` (07:29–07:41 CET) | 709 s, after the sweep | On return: health `LOST`, indexer `swept=true spent=false`, rgb-lib still says settled 1000. Recovery: commitment `0966f28d4c95503e3e49c73c80d85735632531236e227a289621fabf07a38618`, forfeit `d241138b8864a4364e77ecd78ec3c8ae88344fe2d729e104b3da68913e6ef75d` → V' `a9499dd4…:0`. Indexer for V afterwards: `spent_by` = forfeit, `settled_by` = commitment. Own proof 1 825 B (3 PSBTs, built from arkd after the sweep). Exit forfeit `ea1f55fd…` → Alice→Dave `fb5e07d6fd66d670b67f356ad3729f5d65dae0b41888fbbe95c2288686178a05`; proof 3 613 B (6 PSBTs). Dave validated with arkd **paused** | Dave settled 1000, PASS |
| official, `state/run-1792996944-stage5-recovery` (setup 07:42 CET) | 5 400 s ≈ 63 "days" | same | **PASS** (return 09:15 CET, actually offline 5 570 s ≈ 64 "days"). On return: `LOST`, `swept=true spent=false`. Recovery: commitment `be09965e89ba8982af7367f6436f97309ba4b42ad3515c32c0fdbe307080f727`, forfeit `ad0e16d0ff5d02c392eec3edd54947c1c48d5b3bcaa6b962202b55a604731525`, V' `d6d3d604b96d9bdc41ab38d85b36b583f35d8042674b850fd759ea4f95581486:0` (HEALTHY, a fresh 7-day expiry). Recovery proof 1825 B / 3 PSBTs. Exit commitment `0bcec52a578e3a68d126623e4114eea57b7cbf28ce6290493ab20637318bd7ca`, exit forfeit `6a63d4d39c42a49975106af2ed94913ca765d8912180d152a56b408365de1a95`. Alice→Dave `16afc92b8a74dcb282c5820820b0d534788df6994cafadf006271fa717ce7461`; proof 3613 B / 6 PSBTs. Dave validated it with arkd unreachable → settled 1000. Every negative was rejected (same set as the dev run). |

Negatives (dev and official runs, all PASS):
- Mallory's forged recovery intent: `INVALID_INTENT_PROOF … invalid sig`.
- Second, genuinely signed intent for the same V: `VTXO_ALREADY_SPENT`.
- The 7 Stage 3 proof mutations on the recovery proof are rejected, including "forfeit without operator sig" and "missing forfeit/connector/branch". A wrong operator key is rejected too.
- The operator rewrites the forfeit's RGB commitment: invalid signature.
- An operator-only signature on V's input: "no fully signed tapleaf".


### Security
- **The operator cannot steal or redirect.** The forfeit's VTXO input needs the owner's signature (SIGHASH_DEFAULT, so it covers the RGB `OP_RETURN`). Measured: rewriting the commitment and an operator-only signature are both rejected by the receiver, and a forged intent is rejected by arkd.
- **The operator can refuse recovery.** The asset is then frozen, not destroyed: the owner can retry while arkd keeps the row. This is the same liveness trust as stock BTC recovery. There is no on-chain fallback after a sweep, because the parent tree output is gone. The Stage 4 watchtower stays useful as an optional trustless path *before* the sweep.
- **Double spend.**
  - Two intents for the same V: arkd rejects the second (`VTXO_ALREADY_SPENT`, measured).
  - Operator and user colluding could co-sign two forfeits of V in two batches. Each receiver would accept its proof offline. This is the same trust as Stage 2 refresh forfeits (resolver-level, operator-attested). Both forfeits carry the operator's signature on conflicting spends of V, which is publishable fraud evidence but not prevention. Receivers who are online can check `spent_by` on the indexer.
  - Unilateral exit vs recovery: arkd allows recovery only for swept, non-unrolled VTXOs. Once swept, V's parent output has been spent by the sweep tx, so V can never appear on-chain. Recovery is therefore strictly safer than a refresh, where the user could still race the forfeit with an exit. Hardening, not built: the verifier could check from the chain alone that V's parent output was spent by a non-branch tx, i.e. swept.
- **Data availability.** The recovering user must present V's branch (proof) to receivers. After the sweep arkd still served it (`GetVirtualTxs`, proof 1 825 B, measured). Wallets should also keep the proofs they received, which already contain V's lineage.
- **How long.** Indefinitely in v0.9.15, while the operator keeps the DB and the user keeps the seed and RGB data. An operator retention policy would be a product/ToS term.
- **RGB consensus: no change.** The trust model is the Stage 2/3 resolver-level one: an unmined, operator-co-signed forfeit is accepted as the witness closing V.

Alternative considered: have the sweep tx commit RGB transitions back to users. It would not close V's seal, because the sweep spends the tree node, not V; seals would have to live on tree outputs, and the operator would choose the destination. Not pursued.


### Stage 5 regressions (arkd v0.9.15-rgb4, patch 6)

| Stage | Result | Key txid |
|---|---|---|
| Stage 0 A | PASS | `f70591e859d1a19241…` |
| Stage 0 B | PASS | `9f88262db6c7e2aa85…` |
| Stage 1 Carol→Dave | PASS | `0a2110dc8ccedbcadf…` |
| Stage 2a refresh | PASS (on the rerun; run 1 hit the electrum "Invalid indexer" flake when runs started concurrently) | `5611698c4bcaaf6cd3…` |
| Stage 2b | PASS | `31d6840d907edc1bf1…` |
| Stage 2 negatives | PASS | – |
| Stage 3 offline proof | PASS | `f341921ca1b8049bca…` |
| Stage 4 offline 2 weeks (watchtower) | PASS (09:16–09:54 CET, `state/run-1793002591-stage4-offline2w`): alice refresh → Dave settled, erin unrolled by the watchtower → Gina, frank (control, watchtower down) LOST as expected | alice→Dave `30bc097fe0590516…`, erin→Gina `99150e5dd954e35d…` |

Logs: `logs/stage5-regress-*.log`, `logs/stage5-regress-progress.log`.

## Stage 6: closing the spike (multi-user batches, failed batches, rgb-lib status, on-chain sweep check, fraud evidence)

26 Oct 2026, 10:10–12:30 CET. arkd is unchanged (`v0.9.15-rgb4`, patches 1–6). Stage 6 is client side (rgb-lib + demo), plus one test tool that simulates a colluding operator.

### Code changes
- **rgb-lib** (`vendor/rgb-lib`, cumulative `patches/rgb-lib.patch`):
  - `ark_proof.rs`: the proof format gets two optional fields. `recoveries` lists forfeits claimed to be sweep recoveries (item 4). `vtxos` lists VTXOs whose branch is bundled, so they can be tracked (item 3). New `verify_ark_proof_ext`, `anchor_sweep`, `ArkVtxoStatus`, `ArkAssetBalance`, `ArkVtxoRecord`, `ArkTransferRecord`, `ArkFraudProof`, `verify_ark_fraud`.
  - `wallet/indexer.rs`: `Indexer::spender_of(outpoint, spk)` (electrum `script_get_history` + tx lookup). Not implemented for esplora.
  - `wallet/rust_only.rs`:
    - `import_ark_proof` runs the sweep check and tracks the listed VTXOs (`ark_vtxos.json` in the wallet dir).
    - `color_psbt_and_consume_custom` logs every custom-colored transfer (`ark_transfers.json`) and marks the tracked VTXOs it consumes.
    - New: `ark_vtxo_status`, `get_asset_balance_ark`, `ark_set_vtxo_owned`, `ark_receive_vtxo_transfer` (accepts an explicit VTXO seal), `list_ark_transfers`, `ark_rollback_transfer`, `verify_ark_fraud_proof`. `accept_transfer_consignment_offchain` marks the received VTXO as owned.
- **demo**: `demo/src/stage6.rs` (`--stage6-multi`, `--stage6-fail`, `--stage6-fraud`, `--stage6-verify-recovery <stage5 dir>`). `proof.rs` adds `mark_recovery` and `add_vtxos`, and carries `recoveries` forward. `stage5.rs` now checks the rgb-lib status and balance and declares the recovery.
- **test tool** `tools/arkdb` (source `vendor/arkd/cmd/arkdb-sim`, not part of the arkd patch): a static Go sqlite client run in a `busybox` container on the arkd data volume. It plays the colluding operator editing its own DB.

### Results (regtest, all measured)
| Item | Result | Evidence (txids full; run dir) |
|---|---|---|
| 1. Multi-user colored batch | **PASS** | `state/run-1793006932-stage6-multi`. Off-chain hop with BTC and asset change: checkpoint `be70a70d4ede5a34e4ec1ef6bdcdffcda8a60d294a5243b19fd643ea532de7df`, ark tx `efbb46f631ae5a57d3adb66c26de2eb80d355a5e890b85f365ae9b89ccbbfdbf` (outputs [0 OP_RETURN, 20000 Dave, 80000 Alice change, 0 anchor]); Dave accepts 300 A offline. One batch for 4 users, commitment `3651a9cd919286225427507ad674e4d7058e87c1f65b7b8677a561bada57d04d`: Alice 700 A forfeit `65190374fa5e657fb8395764028b0b4aee7238f0075b781c20a1d13e18bdf868`, Dave 300 A forfeit `28091e963189c8e93d96770d79315004df115c09ccefc1caa636c3544bf66a13`, Bob → Frank 1000 B forfeit `62c98b6f301c9b1c35626f8c19fdaf52ad1a5c9fb88adbee1983ce9bafb00768`, Carol BTC-only VTXO 0.0005 BTC. Proofs 6234 B / 6368 B / 3868 B, each verified with arkd paused. Balances (`get_asset_balance_ark`): Alice 700 A, Dave 300 A, Frank 1000 B, Bob 0, in_flight 0. Negatives: 7 proof mutations + wrong key + unattested forfeit, all rejected; Bob's normal forfeit declared as a recovery rejected. |
| 2. Failed colored batch | **PASS** | `state/run-1793007030-stage6-fail`. Attempt 1 failed after 33 s: arkd "failed to finalize round: missing forfeit tx" (Bob's hook refused). Alice's colored forfeit F1 `2b8c6838b9c57f6d325f5e77ef3248e7ff8c264573c81b3bac35e04fc4c16917` was submitted but never co-signed; balance in_flight 1000, indexer unspent. `ark_rollback_transfer(F1)` → active 1000. Retry commitment `d26d961e3e0fb4de8d7c6a48382a4388a03a1bd91abe7308ebc51e4068e826ea`, forfeit F2 `e03043a553929cc5e0821f9d072d3b2a4d04fd55c9d06c6e620afc40c618acda`; indexer `spent_by` = F2; Gina accepts 1000 offline; Alice 0. Negative: rollback of attested F2 refused. |
| 3. rgb-lib balance | **PASS** | Stage 5 rerun `state/run-1793007378-stage5-recovery`: before offline `Active`, active 1000; on return `ToRecover{sweep_txid: bf0b347964fb56c2b0089cd528f2e9feb5b411e66e747f1d3b910727f4bfc74e}`, active 0 / to_recover 1000; recovery forfeit `5803eaf40a3743b8ec255382839e01eedc27a76fa40c83ed64028c42ee6b7019` → V' `d922c946745b2b0423ec4d07840c038a6d602556c6c6883784b8da0d720d59c9:0`, active 1000 / to_recover 0. Custom-colored transfers now listed by `list_ark_transfers`. The stock DB `get_asset_balance` still reports settled 1000 throughout (stale; not fixed). |
| 4. On-chain sweep check | **PASS** | On the official Stage 5 dir: recovery forfeit `ad0e16d0ff5d02c392eec3edd54947c1c48d5b3bcaa6b962202b55a604731525` declared as a recovery accepted with arkd paused. Negative: the exit forfeit `6a63d4d39c42a49975106af2ed94913ca765d8912180d152a56b408365de1a95` declared as a recovery rejected ("VTXO d6d3d604…:0 is not swept on-chain"); in the multi run, Bob's forfeit declared as a recovery rejected. Stage 5 rerun also carries `recoveries` in the Alice→Dave proof. |
| 5. Fraud path | **PASS (detected, not prevented)** | `state/run-1793007195-stage6-fraud`. Batch 1 commitment `1d341b47bc43714af4815510921048c1c7e4e2002bc1f157b0f0f151704c1634`, F1 `4733f5695205a6e780c2b0e1f05b8f10dac44f1441bacf834577cb03997bac22` → Dave (1000 offline). Honest second attempt refused (client side "not all requested VTXOs are settleable"). Operator DB edit (1 row) via `tools/arkdb`, then batch 2 commitment `73b5dc97d42f22182742f1389274a7c72e2ee0b0b56eeb7059ecc7996fd6544e`, F2 `6ed0d249c5ce377710601af988ef11202b1e071ca302fe6b610e6c71ee3bb541` → Erin (1000 offline). Fraud proof 5060 B verified with arkd paused. Negatives: same forfeit twice, operator signature stripped, VTXO mismatch: all rejected. Online: indexer `spent_by` = F2, so Dave reports CONFLICT; Erin sees consistent data. |

### What is prevented and what is only detected
- **Prevented:**
  - A non-owner moving an asset: Stage 2/5 negatives.
  - The operator redirecting an asset without the owner: Stage 5.
  - A second forfeit of the same VTXO by an *honest* operator: Stage 5 `VTXO_ALREADY_SPENT`, plus the client-side "not settleable" check here.
  - A recovery forfeit accepted for a VTXO that is not swept: item 4, from chain data only.
  - Rolling back a forfeit that was attested: item 2.
- **Only detected:** if the operator *and* the owner collude, arkd co-signs a second forfeit of the same VTXO, and both receivers validate offline (Dave and Erin each hold 1000 of a 1000 supply). Nothing in RGB or in the offline proof can stop this. The evidence is publishable:
  - two forfeits, both carrying the operator's signature on the same VTXO input, each linked to a mined commitment;
  - `verify_ark_fraud_proof` checks it with the chain only.
  - An online receiver detects it from the indexer: `spent_by` is not its own forfeit. Only the *earlier* receiver sees it. The later one (Erin) sees a consistent indexer.
  - Penalties (slashing, bonds, reputation) would be outside Ark/RGB.
- **Rollback caveat (item 2):** in the measured failure Alice had already *submitted* her signed forfeit F1. The round failed ("missing forfeit tx" from Bob), so F1's connector comes from a commitment tx that was never broadcast. Rolling back is safe as long as that commitment can never appear. An operator that later broadcast the failed round would turn F1 into a second operator-co-signed forfeit, which is the fraud case above: detectable, not prevented. `ark_rollback_transfer` refuses mined, in-mempool or attested witnesses.

### Stage 6 caveats / open problems
- The Ark registry (`ark_vtxos.json`, `ark_transfers.json`) is JSON in the wallet dir, not rgb-lib's DB; the stock `get_asset_balance` stays stale (reports settled 1000 for swept or forwarded VTXOs). Use `get_asset_balance_ark`.
- The sweep check and `spent_by` need an Electrum indexer; with Esplora `spender_of` returns an error, so a recovery claim cannot be accepted.
- `ark_rollback_transfer` is manual; a wallet must call it after a failed round (see the rollback caveat).
- Item 5's honest refusal was measured client-side here; arkd's own `VTXO_ALREADY_SPENT` refusal was measured in Stage 5.
- Measured vs estimated: everything in the item table is measured on regtest. The fraud deterrent (bonds/publishing) and atomic swaps are design only (see the Page).
- Stage 5 rerun note: the first `--stage5-return` ran at expiry + 307 s, before arkd had swept the batch (indexer `isSwept=false`), so rgb-lib correctly reported `Active` and the test failed its to-recover assertion. Rerunning the return step after the sweep passed. `logs/run-stage5.sh` now waits for `isSwept` plus 30 s before the return.

### Stage 6 regressions (arkd `v0.9.15-rgb4`, rgb-lib with the Stage 6 API)
| Regression | Result | Log |
|---|---|---|
| Stage 5 sweep recovery (720 s offline, scaled) | PASS on rerun of the return step (attempt 1 returned before arkd swept; see caveats) | `logs/stage6-regress-stage5.log`, `state/run-1793007378-stage5-recovery/return.log` (+ `return-attempt1.log`) |
| Stage 1 jump-in / off-chain / exit | PASS | `logs/stage6-regress-stage1.log` |
| Stage 0 variant A | PASS | `logs/stage6-regress-stage0-variantA.log` |
| Stage 0 variant B | PASS | `logs/stage6-regress-stage0-variantB.log` |
| Stage 2 (a) refresh | PASS | `logs/stage6-regress-stage2-refresh.log` |
| Stage 2 (b) collaborative exit | PASS | `logs/stage6-regress-stage2-exit.log` |
| Stage 2 negatives | PASS | `logs/stage6-regress-stage2-neg.log` |
| Stage 3 offline proof (arkd paused) | PASS | `logs/stage6-regress-stage3-offline-proof.log` |
| Stage 4 offline 2 weeks (watchtower) | PASS on rerun (11:47–12:25 CET, `state/run-1793011661-stage4-offline2w`): alice refresh → Dave settled 1000; erin unrolled by the watchtower → Gina 1000; frank (control, watchtower down) flagged LOST as in Stage 4 (no recovery attempted in that scenario; Stage 5 covers recovery) | `logs/stage6-regress-stage4-offline2w.log` (attempt 1 setup failed on the same expiry race: `-attempt1.log`) |

Both timing failures (Stage 5 return and Stage 4 setup) were harness races with arkd's expiry setting and sweeper, not code failures: `run-stage5.sh` now waits for `isSwept`, and both scripts sleep 45 s after changing `vtxoTreeExpiry`.

## Open issues / caveats

- **Asset loss on renewal (Stage 0).** The asset sits on `cp(V):0`, which is reachable only through V's offchain spend. If Alice renews or settles V in a batch (forfeit path) or lets it expire, the asset is gone. *Stage 2:* refresh and collaborative exit now keep the asset (forfeit commitment). **Expiry (Stage 3):** covered only by refreshing in time or by the exit watchtower (tested); otherwise: after the batch expiry the operator sweeps V with its own key, no user-signed tx closes the seal, and the asset is lost. The wallet must refresh colored VTXOs before expiry. The wallet must pin V. If arkd rotates the checkpoint tapscript between prediction and send, the prediction breaks; it is checked before submit.
- **Variant B wallet DB.** After the jump-in `T1`, Alice's rgb-lib DB still reports the old allocation (`settled: 1000`) because `T1` is not a native rgb-lib transfer. The RGB stock is correct; the DB is not.
- **One offchain witness only (Stage 0).** Stage 0's `OffchainResolver` resolves a single offchain witness. *Solved in Stage 1* by `MultiOffchainResolver`, tested with 4 offchain witnesses.
- **Stage 1: rgb-lib DB vs stock.** Custom-colored transfers are consumed into the RGB stock only, so the rgb-lib DB does not see them: Bob's `list_assets` is empty after he receives 1000 and keeps a 300 change. The stock is correct (Bob built T1/T2 from it). A real wallet needs DB bookkeeping for the custom seals.
- **Stage 1: trust in Tentative witnesses.** A receiver of an off-chain chain accepts on the arkd `GetVirtualTxs` cross-check plus the operator co-signature: it is preconfirmed, not mined. Double-spend safety is the usual Ark preconfirmation trust until the receiver settles or exits. RGB-aware *settlement/renewal* (the forfeit path) is handled in Stage 2.
- **Stage 1: exit cost grows with hops.** Each hop adds 2 virtual txs (checkpoint + ark tx) to the exit branch. Carol needed on-chain funds for the anchors, and the CPFP retries showed TRUC descendant-limit conflicts while arkd raced to broadcast the checkpoints (resolved by retry).
- **Negative tests:** run in Stage 2 (`--stage2-neg`): malformed checkpoint and forfeit OP_RETURNs are rejected by arkd, and missing attestation is rejected by the RGB validator.
- **Operator broadcasts the checkpoint.** During the exit, arkd itself broadcast the checkpoint tx; the client then only broadcast the leaf and the ark tx.
- **Ark tx standardness (Stage 0).** The ark tx is v3 (TRUC) with a 1-sat OP_RETURN. It relays on Core 31 only with `maxburnamount` raised in the RPC call. With arkd patch 2 (Stage 1), the opret is 0 sat and this no longer applies.
- The arkade-explorer container restart-loops (nginx placeholder image); this is harmless.
- *Stage 3 update:* the Ark dependency below is removed for validation (offline proof sidecar); what remains is that the proof file must travel with the consignment forever while the forfeit stays unmined.
- **Stage 2: permanent Ark dependency for history validation.** The forfeit witness is never mined in the honest case. Every future receiver of the asset, not only the next one, must attest `F` against the Ark layer, and today that means asking arkd's indexer. If the operator prunes or loses that data, new receivers cannot validate the history.
  - Mitigation, not implemented: ship a self-contained proof in the consignment, i.e. the signed `F`, the connector branch from `F` to `C`, and `C`'s block inclusion. Then validation needs no live arkd.
- **Stage 2: single-use-seal trust.** Two txs spending V (an off-chain checkpoint and `F`) would both look like valid Tentative closings to RGB. Only Ark prevents the second one:
  - arkd refuses to forfeit a spent VTXO.
  - If the user unilaterally exits V after forfeiting it, the operator broadcasts `F` within the CSV window, so `F`, and with it `T_f`, gets mined.
  
  This is the usual Ark operator assumption. With an unmined forfeit, though, it becomes permanent for the RGB history rather than lasting "until settlement". The fraud path (arkd broadcasting a colored `F`) was not exercised on regtest. It uses the same code as stock (the anchor is found by script), and the OP_RETURN is 0 sat and standard.
- **Stage 2: safe height from the anchor.** An attested, unmined witness counts as buried at the depth of its commitment tx `C`. That is sound only if the attestation is correct (`F` registered in batch `C`).
- **Stage 2: colored batch is single-attempt.** The forfeit hook consumes `T_f` into the local RGB stock before arkd signs. If the batch then fails, the stock holds a never-signed transition spending V, so retries would need a stock rollback. The demo does not retry colored batches.
- **Stage 2: one colored VTXO per batch in the demo.** The hook handles any VTXO, but the demo moves a single asset/VTXO. Several colored VTXOs in one refresh would mean one forfeit per VTXO, each with its own `T_f`, which works the same way but is untested.
- **Stage 3: proof trust limits.** Connector txs are checked by linkage only. Tree-node MuSig membership is not checked: authenticity is inductive from the root's signature over the commitment output. The proof does not say whether the batch has since expired or been swept. The operator keys are pinned in wallet config, so a key rotation needs a config update.
- **Stage 3: proof size.** It grows by about 2.5 kB per off-chain hop and about 1.8 kB per unmined forfeit, and it shrinks again only when hops get mined. An unmined forfeit stays in every later proof.
- **Stage 3: watchtower.** This is a demo mode inside the test process, not a daemon. It trusts arkd's public indexer for "refreshed?". When the indexer is unreachable it keeps waiting; a production version should broadcast at the threshold instead. It needs its own fee UTXOs (about 1.2 kSat per depth-2 branch at 2 sat/vB, growing with depth and hops). After the exit the owner still has to wait the 512 s CSV and spend V on-chain. Broadcasting a shared branch also delays the sweep of sibling subtrees by one more expiry period.
- **Stage 3: stale RGB state after sweep.** rgb-lib keeps reporting a swept colored VTXO as `settled`, because RGB has no notion of the Ark sweep. A wallet must flag such allocations as lost.
- **Stage 3: delegated refresh is unsolved.** The forfeit signature must commit to `T_f`, which commits to the new VTXO, which is known only once the batch is built. A delegate cannot refresh a colored VTXO for an offline user without new arkd support.
- **Stage 3: regtest helper bug fixed.** `Regtest::tx_confirmations` counted mempool txs as mined. Earlier Stage 2 "commitment mined" checks could therefore pass on a mempool tx; the Stage 2 runs still passed, and the regressions on rgb3 were re-run with the fix.

- **Stage 4: offline survival.** The watchtower daemon is now standalone (`--watchtowerd`), fails safe when the indexer is down and pays fees only from user-presigned ladders. It still relies on (i) someone being up near expiry (run several; registrations hold no secrets), (ii) the ladder topping out at 64 sat/vB (a fee spike above that delays the exit), (iii) d prefunded fee UTXOs per registered VTXO, re-registered after every refresh/hop. A watchtower can grief (unroll early); this actually happened in the run, see Stage 4 A. Option (a) needs a per-operator (or per-batch) 28-day expiry; arkd's expiry is global. `colored_health()` only flags a swept VTXO; rgb-lib itself still reports it as settled.
- **Stage 4: wallet reload.** Reloading a wallet whose custom PSBTs spent its own UTXOs trips rgb-lib's consistency check ("spent bitcoins with another wallet"); the reload skips the check. The on-chain bench uses a fresh wallet per hop because a second receive into a wallet that had already sent was not found by BDK (`psbt_input_for_utxo`), even after a colored full sync. Not investigated.
- **Stage 4: payload growth.** Measured: the RGB consignment grows by about 1 kB per off-chain hop, 0.5 kB per mined transfer and 0.7 kB per merged input. Refreshes do not reset the Ark proof. For a high-velocity, merge-heavy asset the RGB history, not Ark, dominates; this is an estimate, see Stage 4 B. Not built: delta consignments, a validation cache, operator-signed checkpoints.
- **Stage 4: measurement limits.** Debug build, local electrum, single machine, so validation time is dominated by resolver round-trips (~120–140 ms floor). The USDt figures are linear extrapolations of these fits, not measurements.
- **Stage 5: sweep recovery.**
  - Recovery depends on the operator's cooperation and on its keeping the swept VTXO row (it does so indefinitely in v0.9.15).
  - Operator and user colluding could produce two forfeits for the same V (Stage 2 trust model).
  - The verifier does not yet check on-chain that V was really swept.
  - `RGB_RECOVERY_FORFEIT` is a process-global switch in the patched SDK (prototype).
  - The minimum arkd expiry is 512 s, so the 2-month run takes about 1.5 h of wall clock.
