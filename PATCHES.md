# Patches

One cumulative patch per upstream repository, applied by `scripts/setup.sh` on top of the pinned commits below; `make patches-check` (`scripts/patches-check.sh`) runs `git apply --check` for all of them. In the patched sources every change is marked with a `[rgb-arkade]` or `[rgb-arkade: <change>]` comment.

| Patch | Upstream | Pinned commit |
|---|---|---|
| `patches/arkd.patch` | https://github.com/arkade-os/arkd | `f444de6c336c024f5b03e646feb781eec9a24c00` (`v0.9.15`) |
| `patches/arkd-checkpoint.patch` | https://github.com/arkade-os/arkd | `f863e484719344edbe4a8d10cf5fe994b123f2c0` (master), built by `scripts/setup-cponly.sh` |
| `patches/rust-sdk.patch` | https://github.com/arkade-os/rust-sdk | `f184a5f1aa77924b7fd40eede7cf2fd43854ae76` (`v0.12.0`) |
| `patches/rgb-lib.patch` | https://github.com/kaleidoswap/rgb-lib | `8852b2e55606ebd568f56bdfd9fa9216fbec3435` |
| `patches/arkade-regtest.patch` (+ `arkade-regtest.env`) | https://github.com/ArkLabsHQ/arkade-regtest | `457b9ef6119bea1b0a8f1356204ef2f1044b0c9b` |

The arkd image `arkd:rgb` is upstream `ghcr.io/arkade-os/arkd:v0.9.15` with the binary replaced by the patched build (`arkd-image/Dockerfile`). Its version string reports `v0.9.15-rgb` (set by `scripts/setup.sh`); `arkd:rgb-cponly` reports `v0.9.16-master-f863e48-rgb-cponly`. `scripts/setup.sh` and `setup-cponly.sh` record the patch they applied in `vendor/.stamps/`, so an edited patch is noticed on the next run.

The arkd image `arkd:rgb-cponly` is upstream `ghcr.io/arkade-os/arkd:v0.9.16` with the binary built from arkd master plus `patches/arkd-checkpoint.patch` only (`arkd-image-cponly/Dockerfile`): the `checkpoint-opret` change below, rebased on master, with its script helpers. It is the checkpoint-only lane of [docs/approaches.md](docs/approaches.md).

## arkd

All changes are opt-in: without the new fields, arkd behaves as upstream.

| Change | Files | Purpose |
|---|---|---|
| `checkpoint-opret` | `pkg/ark-lib/offchain/tx.go`, `internal/core/application/service.go`, `sweeper.go`, `indexer.go` | Optional 32-byte user `OP_RETURN` at index 0 of the checkpoint tx, reproduced by the deterministic rebuild (the checkpoint output moves to vout 1). Sweeper, indexer and watch list use the right checkpoint index. Lets the checkpoint close the VTXO's RGB seal, so off-chain transfers can chain (multi-hop). |
| `opret-data-output` | `pkg/ark-lib/script/script.go`, `internal/core/application/service.go`, `utils.go`, `internal/infrastructure/db/service.go` | A 0-sat `OP_RETURN` output is a data output (the RGB opret commitment), not a sub-dust VTXO: no 1-sat burn, nothing indexed as a VTXO. |
| `forfeit-opret` | `pkg/ark-lib/tree/forfeit_tx.go`, `internal/infrastructure/tx-builder/covenantless/builder.go` | Optional 32-byte `OP_RETURN` (0 sat, index 0) in the forfeit tx, accepted by `VerifyForfeitTxs` through the deterministic rebuild. The forfeit closes the seal of a refreshed or collaboratively exited VTXO. |
| `forfeit-cosign` | `internal/core/application/service.go` (`finalizeRound`), `fraud.go` (`broadcastForfeitTx`) | The operator also signs the VTXO input of the forfeit it stores, so a forfeit can be verified offline without arkd and cannot be forged with a fake connector. When reacting to a fraud (a forfeited VTXO unrolled on-chain), the operator drops its own stored signatures before signing again, so the PSBT carries each key once. |
| `recovery-forfeit` | `pkg/ark-lib/intent/message.go`, `internal/core/domain/vtxo.go`, `internal/core/application/service.go` | New signed intent field `rgb_recovery_forfeit`. When it is set, recovering a swept or expired VTXO also requires a (co-signed) forfeit, which can carry the RGB transition to the new VTXO. |

## rust-sdk

| Area | Files | Purpose |
|---|---|---|
| Checkpoint payload | `ark-core/src/send.rs`, `ark-client/src/send_vtxo.rs` | `VtxoInput::with_checkpoint_op_return` / `checkpoint_op_return`, `checkpoint_output_index` (same layout as arkd); `resolve_selected_send_inputs`, `submit_built_offchain_send`, `sign_and_finalize_pending_tx` to build, color and then submit an off-chain send. |
| Forfeit hook | `ark-core/src/batch.rs`, `ark-client/src/batch.rs`, `ark-client/src/lib.rs` | `create_and_sign_forfeit_txs_with_hook`, `settle_vtxos_with_forfeit_hook`, `collaborative_redeem_vtxos_with_forfeit_hook`: the caller adds the RGB `OP_RETURN` to each forfeit once the batch outputs are known. |
| Recovery | `ark-core/src/intent.rs`, `ark-client/src/batch.rs` | `rgb_recovery_forfeit` intent field (process-wide switch `RGB_RECOVERY_FORFEIT`), `rgb_settleable_inputs`, `rgb_sign_intent` (tests). |
| Exit | `ark-core/src/unilateral_exit.rs`, `ark-client/src/unilateral_exit.rs` | `create_unilateral_exit_transaction_with_hook` / `create_send_on_chain_transaction_with_hook`: the exit spend can carry an RGB commitment. Its `inputs` argument names the exact outputs to spend (`coin_select_exact_for_onchain` in `ark-client/src/coin_select.rs`): stock coin selection stops at the target amount and could pick another exited output of the same contract (e.g. an earlier change output that the same unroll put on-chain and that matures first) instead of the colored one. `finalize_unilateral_exit_tree` finalizes every input of each virtual tx, not only input 0, so a branch through an Arkade tx with inputs from two owners (a swap) can be broadcast. |
| Indexer | `ark-grpc/src/client.rs` | `get_forfeit_txids`, `get_connector_txids` of a commitment tx (Ark-level attestation of unmined forfeits). |

## rgb-lib

| Area | Files | Purpose |
|---|---|---|
| Custom seals | `src/wallet/rust_only.rs` | `issue_asset_nia_to_outpoint`, `psbt_input_for_utxo`, `color_psbt_and_consume_to_outpoint`, `color_psbt_and_consume_custom`: color a PSBT with seals on exact vouts or external outpoints (VTXOs, checkpoints). |
| Off-chain witnesses | `src/utils.rs`, `src/wallet/online.rs`, `src/wallet/rust_only.rs` | `ArkAwareResolver` (over rgb-consensus' `OffchainResolver`), `accept_transfer_consignment_offchain`, `validate_consignment_ark`, `add_ark_attested_witness`: unmined Ark txs resolve as `Tentative` once attested; the safe height follows the commitment tx. |
| Ark proofs | `src/ark_proof.rs`, `src/wallet/rust_only.rs`, `src/wallet/indexer.rs` | `import_ark_proof`: verifies a self-contained proof (unmined Ark txs down to a mined commitment, operator and tree signatures, opret layout with a 0-sat or sub-dust opret; every witness needs an operator-co-signed input) against the chain only. `import_ark_proof_with_min_lifetime` also verifies each listed VTXO's sweep timelock (tree tx `expiry` field checked against the batch output key: MuSig2 of the cosigners tweaked by the sweep leaf of a pinned operator key) and rejects a VTXO with less than a minimum left (`ArkMinLifetime`). With `recoveries`, it also checks on-chain that the VTXO was swept (`Indexer::spender_of`, Electrum only). |
| Ark registry and balance | `src/ark_proof.rs`, `src/wallet/rust_only.rs`, `src/utils.rs`, `src/lib.rs` | Tracked VTXOs and custom-colored transfers (`ark_vtxos.json`, `ark_transfers.json` in the wallet dir): `ark_vtxo_status` (active / to recover / unrolled / spent, from the chain; an unrolled VTXO whose output is spent on-chain counts as spent, `Indexer::spender_of_outpoint`), `get_asset_balance_ark`, `ark_receive_vtxo_transfer`, `ark_set_vtxo_owned`, `list_ark_transfers`, `ark_rollback_transfer`. |
| Fraud proofs | `src/ark_proof.rs`, `src/wallet/rust_only.rs` | `ArkFraudProof` / `verify_ark_fraud_proof`: two operator-co-signed forfeits of the same VTXO, each tied to a mined commitment. |

## arkade-regtest

`docker/compose.*.yml` use `ARKD_IMAGE` (set to `arkd:rgb`), and the mempool container fetches its pools list from a local `pools-mirror` (the `pools/` directory, served by nginx) because the containers have no internet access. `arkade-regtest.env` is the `.env` used: `vtxoMinAmount` 1, zero fees, 512 s exit delays, and a 604672 s (about 7 days) batch expiry. Scenarios change the expiry at runtime through the admin API. `arkade-regtest.stock.env` (`REGTEST_PROFILE=stock`) is the fast profile on the unpatched `ghcr.io/arkade-os/arkd:v0.9.15` image. `arkade-regtest.fast.env` (`REGTEST_PROFILE=fast`, `make regtest-up-fast`) puts every delay in blocks (exits 10, boarding 20, expiry 500): arkd needs all delays of one type, so the time-scaled scenarios refuse to run on it. It is for scripted runs only: arkd's `Round.ExpiryTimestamp` adds the expiry as seconds even when it is in blocks (also on arkd master), so VTXOs report an expiry 500 s after their batch and look expired to clients long before the block-based sweep.
