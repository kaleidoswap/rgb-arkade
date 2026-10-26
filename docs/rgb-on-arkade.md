<!-- English translation of the Page "RGB su Arkade" (version of 26 Oct 2026, 12:30 CET). The original Italian version lives in the Page: https://cursor.com/canvas/store-e5dae6c1-22ce-4fc9-be4f-958a0cdf37c7/d7de931f-98e3-4a02-af31-8ccd68868a97 -->

> **Note:** this is a full English translation. The original Italian version is the Page ["RGB su Arkade: come funziona e cosa si può fare"](https://cursor.com/canvas/store-e5dae6c1-22ce-4fc9-be4f-958a0cdf37c7/d7de931f-98e3-4a02-af31-8ccd68868a97).

# RGB on Arkade: how it works and what it can do

_Status of the RGB-on-Arkade prototype (regtest: Stage 0 on stock arkd v0.9.15, Stage 1 multi-hop, Stage 2 refresh / collaborative exit, Stage 3 offline proofs and exit watchtower on patched arkd), why it works, what is missing, and which changes a complete product needs. Facts are verified against source code and real runs; design proposals are marked as untested._

## Summary

> **Stage 0 passed on regtest.** An RGB asset (NIA, 1000 units) is issued on-chain, enters Arkade, moves from Alice to Bob with a preconfirmed Ark transaction, leaves on-chain through Bob's unilateral exit, and reaches Carol, who sees it in `list_assets` with balance `settled: 1000`.

On stock arkd v0.9.15 the following work: deposit via "jump-in" (an on-chain transfer to the predicted checkpoint output), a 1-hop off-chain send, the unilateral exit, and the following on-chain transfer. Without arkd changes the following do not work: multi-hop sends, VTXO refresh, collaborative exit, and direct onboarding of the colored UTXO via boarding. In all these cases the asset would be lost or could not be validated.

With patched arkd: multi-hop tested in Stage 1; refresh of the colored VTXO and collaborative exit to an on-chain address tested in Stage 2 (25 Oct 2026), with the RGB commitment in the forfeit tx and Ark-aware validation on the rgb-lib side. Expiry without refresh and direct onboarding remain out of scope.

Stage 3 (26 Oct 2026): the receiver verifies the whole history with only the blockchain and an attached proof file, even with arkd switched off; a watchtower holding none of the user's keys takes a colored VTXO on-chain before expiry. Without refresh or a watchtower, the sweep destroys the asset.

Stage 4 (26 Oct 2026): with 28-day batches and automatic refresh, 14 days offline need no outside help. Beyond that limit a keyless watchtower steps in, with fees already signed by the user. Both cases are tested. A payment with 16 off-chain hops needs 22 kB of consignment and 15 kB of binary proof, and verifies in about 0.4 s.

Stage 5 (26 Oct 2026): a user offline for months does not lose the asset. After the sweep the wallet recovers the BTC and the asset into a new VTXO with a single small arkd change (patch 6), with no watchtower and no longer expiries. The operator can refuse the recovery but cannot take the asset.

Stage 6 (26 Oct 2026, spike closure): batches shared by several users with two assets and BTC-only VTXOs, retry of a failed batch without losing the asset, correct rgb-lib balance for swept VTXOs, on-chain sweep check, and fraud proof. A double spend with the operator and the user colluding is detected and provable, but not prevented. Further down: spike conclusion and atomic swap theory.

## Spike conclusion

### What works (measured on regtest)

- RGB assets inside Arkade: deposit, multi-hop off-chain sends with BTC and asset change, refresh, collaborative exit, unilateral exit.
- Shared batches: several users, several assets and BTC-only VTXOs in the same batch; a failed batch is retried without losing the asset.
- The receiver verifies everything with only the blockchain and a proof file of a few kB, even with arkd switched off.
- A user offline for months recovers BTC and asset after the sweep; rgb-lib shows the asset as "to recover", not as available.
- The operator alone cannot take or move an asset.

### What does not work or remains a limit

- The operator and the user, colluding, can spend twice: the fraud is detected and provable, but not prevented. An offline receiver cannot notice it.
- The operator can refuse a recovery: the asset stays stuck (not destroyed), like the BTC.
- Direct onboarding of a colored UTXO via boarding is not possible without large arkd changes.
- The standard rgb-lib balance (database) does not see assets in Arkade: the new Ark balance (`get_asset_balance_ark`) is needed. The sweep check works only with Electrum, not with Esplora.
- Every receiver must keep consignment and proof; the cost of a unilateral exit grows with the off-chain chain.

### What is needed to go further

1. Propose the 6 patches to Ark Labs (small: optional OP_RETURN in checkpoint, ark tx and forfeit; forfeit co-signing; mandatory forfeit in recovery).
2. Bring the rgb-lib Ark functions into the KaleidoSwap fork with tests, database persistence (today JSON files in the wallet folder) and Esplora support.
3. Wallet: automatic refresh, automatic recovery on return, automatic rollback of failed batches, `spent_by` check for online users.
4. Fraud deterrence: publication of fraud proofs and economic guarantees from the operator.
5. Atomic swap prototype (section further down), starting from the same-ark-tx variant.

## Basic concepts

### Arkade

- VTXO: virtual output owned by the user, a leaf of a tree of pre-signed transactions anchored on-chain.
- Batch / commitment tx: on-chain transaction built by the operator; output 0 = root of the VTXO tree, output 1 = connector tree, then any requested on-chain outputs and the operator's change.
- Preconfirmed transactions: an off-chain spend is a checkpoint tx (1 input = the VTXO, outputs = checkpoint + P2A anchor) followed by an ark tx spending `checkpoint:0`. arkd rebuilds both and compares the txids.
- Forfeit: when a VTXO is refreshed or withdrawn in a batch, the user signs a forfeit tx (inputs: VTXO + connector; outputs: operator address + anchor) that the operator publishes only if the user tries to unroll the old VTXO.
- Unilateral exit: the user publishes the chain of transactions from the commitment down to their VTXO (bumped via the P2A anchor) and spends after the exit CSV (512 s in our regtest).
- Expiry / sweep: after the tree expiry (`vtxoTreeExpiry`) the operator can spend the batch output through the sweep path; VTXOs that were not refreshed lose their anchoring.

### RGB

- Single-use seal: state is assigned to an outpoint; whoever spends that outpoint must include in the transaction a commitment to the transition that moves the state.
- Opret: the commitment is the first `OP_RETURN` output of the transaction, exactly `OP_RETURN <32 bytes>`. Tapret: commitment hidden in the script tree of a taproot output.
- Consignment: file with the genesis and the history of transitions up to the receiver's outputs, passed out of band.
- Client-side validation: the receiver verifies the consignment and resolves the witness transactions via an indexer (Electrum) or an off-chain resolver.
- Tentative vs mined witnesses: rgb-consensus accepts a tentative (not yet mined) witness and rejects archived or unresolvable ones.

## How RGB on Arkade works today (Stage 0)

### The predicted-checkpoint trick

The txid of the checkpoint tx depends only on the VTXO outpoint and on the server's checkpoint tapscript, not on the outputs of the ark tx. Alice can therefore compute in advance (dry run of `build_offchain_transactions`) the txid of the checkpoint with which she will spend her VTXO `V`, and use `cp(V):0` as the RGB seal. The ark tx, built by the user, spends `cp(V):0` and carries the opret commitment of the transition to Bob's output.

### Two ways to bring the asset to cp(V):0

- **On-chain issuance + jump-in (recommended)**: Standard rgb-lib issuance on a mined UTXO; then an on-chain tx T1, built by the user, transfers the asset to the explicit seal `cp(V):0`. Alice gets `V` by boarding ordinary BTC, separately.
- **Issuance on a VTXO**: The genesis assigns the supply directly to `cp(V):0` (rgb-lib patch `issue_asset_nia_to_outpoint`). Simpler, but the genesis is not on a mined UTXO.

### Flow Alice → Bob → exit → Carol

1. **On-chain genesis**: mined rgb-lib UTXO
2. **Alice's BTC boarding**: settle → VTXO V
3. **cp(V) prediction**: checkpoint dry run
4. **Jump-in T1 (mined)**: opret, asset → cp(V):0
5. **Ark tx Alice → Bob**: opret 1 sat, asset → output 1
6. **Bob accepts the consignment**: tentative off-chain witness
7. **Bob's unilateral exit**: leaf, checkpoint, ark tx mined
8. **Bob → Carol on-chain**: after CSV 512 s, opret
9. **Carol: settled 1000**

### Evidence: run7 (on-chain issuance + jump-in), PASS 25 Oct 2026 13:11 CET

| Step | Txid / value |
|---|---|
| Genesis UTXO (mined) | `829baa0a7710f98f573de910bb8c3e9a44221e7e18c9e36085d44122c2b80da1:1` |
| Asset | `rgb:C7145cqw-WYExAva-uZqG47r-XDC6kaR-Z6U6IsM-ctx2gn8` |
| Predicted checkpoint = server checkpoint | `51932c7a5d7097160c1a9b22cae0173223c3a21feb64654fde04582988736f22` |
| Jump-in T1 (opret `6a208024…8d40`) | `70728ac1484ab0dc00de2d3279aa047be54fab56de8b3792b02c1db0e8deaa27` |
| Ark tx Alice → Bob (local txid = server) | `f6ddbe788cf520fc451fca0e25aea9bce6d08dbfa1bad71657a02a424d553ef3` |
| Exit: batch leaf | `499aaf72ed8a02369bd329dc33e9caed7092e499c4efcbe97548b32b1233a93c` |
| Bob → Carol (on-chain) | `12bfea3f9b334bcabfa52b18a3395316baf7f295e5622cd8b46401047564a731` |
| Carol's balance | `settled: 1000, future: 1000, spendable: 1000` |

The "issuance on a VTXO" flow also passed (run6, ark tx `30789e00…a21b`, Bob → Carol `7aa8cd5d…e29f`). Logs and consignments are in `/workspace/rgb-arkade` on the box.

### Technical details that matter

- Opret only, no tapret: the taproot outputs of VTXOs, checkpoints and the tree have keys and scripts fixed by the Ark protocol (and checked by the server by rebuilding them), so the user cannot add a tapret tweak. In the ark tx the user controls the outputs and can insert an `OP_RETURN`.
- 1 sat on the opret: arkd treats `OP_RETURN <32 bytes>` as a sub-dust VTXO; it must be worth at least `vtxoMinAmount`. arkd v0.9.15 rejects 0 ("vtxo min amount must be greater than 0"), so we use 1 sat.
- `maxburnamount`: bitcoind (v31) by default rejects via RPC a tx with a non-zero-value `OP_RETURN`; during the exit the ark tx must be submitted with a raised `maxburnamount`. It is only an RPC check; the tx is standard.
- rgb-lib quirk: with P2TR outputs `color_psbt` shifts indexes by +1 (opret first); we insert an empty `OP_RETURN` at index 0 and pass index - 1.
- During the exit it was arkd itself that published the checkpoint tx; the client published the leaf and the ark tx.

## Stage 1: multi-hop off-chain A → B → C (tested)

> **Stage 1 passed on regtest, 25 Oct 2026 13:52 CET.** With a patched arkd (`arkd:rgb`, based on v0.9.15) the asset makes two off-chain hops (Alice → Bob → Carol, with 300 units of change to Bob), Carol exits unilaterally and sends on-chain to Dave, who sees `settled: 700` in `list_assets`.

### How each hop works

| Tx | Outputs | RGB transition |
|---|---|---|
| Checkpoint (spends the VTXO) | `[OP_RETURN 32B (0 sat), checkpoint, anchor]` | T1: VTXO seal → `CP:1` |
| Ark tx (spends `CP:1`) | `[OP_RETURN 32B (0 sat), receivers, change, anchor]` | T2: `CP:1` → receiver and change |

- The jump-in now points directly to Alice's VTXO `V`: predicting the checkpoint txid is no longer needed.
- T1 uses a witness-vout seal, so the commitment is computed before the checkpoint txid: no circularity. arkd rebuilds checkpoint and ark tx and the txids match the local ones.
- Finding: the checkpoint `OP_RETURN` must be at index 0. RGB opret1st requires it to be the first DBC-compatible output, and a P2TR output before it makes it rejected (first attempt failed). The checkpoint output therefore moves to vout 1 when there is a payload. Without a payload the upstream layout is unchanged.
- The receiver accepts chains of `Tentative` off-chain witnesses (2 for Bob, 4 for Carol) after a cross-check with arkd's `GetVirtualTxs`.

### Evidence (run-1792932157-stage1)

| Step | Txid / value |
|---|---|
| Jump-in T0 → seal V (mined) | `6eee26425a6d217e31b328113313a8ecbb81e2149a00ca54cf424e479b41c43c` |
| Checkpoint A → B (T1) | `faaedf01478a737a1c538dc1471aec221c9e9a44b86ef958abc822afa5ab580c` |
| Ark tx A → B (T2) | `253ff3e944f2b267d4a96acb46968bbe7d2b4da8461f39a871bd90ce3fc7bfd2` |
| Checkpoint B → C (T1) | `9094516f1836b47253b3911f2dfde95888c97319e742b99fbb12b1ef73762791` |
| Ark tx B → C (T2: Carol 700, Bob 300) | `c55252ab79288676807f8ebf13b139bc64f8abe0545db0776c6ffb6389675de9` |
| Carol's exit | leaf, 2 checkpoints and 2 ark txs all mined (arkd published the checkpoints) |
| Carol → Dave (on-chain) | `06c59cd7e502655c15aa4e84b0dc396f043937757405726c2b1db8f2392f9768` |
| Dave's balance | `settled: 700, future: 700, spendable: 700` |

Regression: the two Stage 0 flows (issuance on a VTXO and on-chain issuance + jump-in) pass again on `arkd:rgb` with `settled: 1000` (14:02 and 14:15 CET).

### Patches used

- arkd (7 files, about +100 lines). Patch 1: optional 32-byte `OP_RETURN` in the checkpoint, included in the deterministic rebuild; the checkpoint index is handled in the sweeper, indexer and watch list. Patch 2: a 0-sat `OP_RETURN` is a data output, not a sub-dust VTXO (sub-dust VTXOs are worth at least `vtxoMinAmount` > 0), so no more 1 sat or `maxburnamount`. Patch 3: restart fix, because stock arkd does not restart if a sub-dust VTXO contains 32 bytes that are not a valid key, like the Stage 0 opret.
- rust-sdk: `VtxoInput::with_checkpoint_op_return` and the same layout as arkd.
- rgb-lib: `MultiOffchainResolver`, `accept_transfer_consignment_offchain` and `color_psbt_and_consume_custom` (seals on exact vouts or external outpoints).

Limits still open after Stage 1: the rgb-lib DB does not see hand-colored transfers (the RGB stock does). Refresh, collaborative exit and negative tests were covered in Stage 2.

## Stage 2: forfeit with RGB commitment (tested)

Passed on regtest on 25 Oct 2026 (22:19–22:32 CET) with `arkd:rgb` version `v0.9.15-rgb2`: refresh of a colored VTXO and collaborative exit no longer lose the asset; all 10 negative tests pass.

### How it works

| Tx | Outputs | RGB transition |
|---|---|---|
| Forfeit (spends connector + VTXO `V`) | `[OP_RETURN 32B (0 sat), operator forfeit, anchor]` | T_f: `V` → `V'` (refresh) or `V` → `C:vout` (exit) |

- The forfeit is the only tx that spends `V` on this path, so it is the only place to close the seal. The new seal is known at the `BatchFinalization` phase: the VTXO tree leaf paying the refresh address, or the commitment tx output paying the exit address. No circularity.
- arkd (patch 3 in the table below): the deterministic rebuild of the forfeit accepts a 32-byte 0-sat `OP_RETURN` at index 0, and the txid comparison covers it. The VTXO signature (SIGHASH_DEFAULT) commits to the `OP_RETURN`, so the operator cannot remove it. Sweeper, indexer and the anti-fraud path needed no changes.
- In the honest case the forfeit is never mined, so for RGB it stays a `Tentative` witness forever. Every downstream validator attests it at the Ark level: commitment tx `C` mined, forfeit present in `GetForfeitTxs(C)`, connector input present in `GetConnectors(C)`, forfeit spending `V` with exactly one correct `OP_RETURN`.
- rgb-lib: `ArkAwareResolver` in all validation paths (including `update_witnesses`, which would otherwise archive the forfeit), `add_ark_attested_witness(tx, anchor)` and a safe-height wait based on the depth of `C`.
- Finding: without the last point the transfer stayed in `WaitingSafeHeight` forever, because RGB flags the unmined forfeit as unsafe history and rgb-lib waited for a confirmation that never comes.

### Evidence (a): refresh → hop → exit → on-chain (run-1792963200-stage2-refresh)

| Step | Txid / value |
|---|---|
| Refresh commitment tx `C1` (mined) | `ef6fd7210843eb3713efc5161e33bd6f1c702d26b88e574cacab97147fb46716` |
| Forfeit `F` (T_f: `V` → `V'`, never mined) | `9e13241a971c0908381ce50608e2b932b2539306e111a24f288849ee6d50bdbe` |
| New VTXO `V'` | `0c68de737f5decc5be82f51db71bd185417f16f83517f1caf715f251b6730545:0` |
| Checkpoint / ark tx `V'` → Bob | `df3530e1…fe88` / `bcb20f4f…0cf2` |
| Bob's unilateral exit | leaf `V'`, checkpoint and ark tx mined; `F` stays unmined |
| Bob → Carol (on-chain) | `75e98bbae798d8a2942878f85315ea98170a63f29276915598f0391655d89175` |
| Carol's balance | `settled: 1000` |

### Evidence (b): collaborative exit → on-chain (run-1792963155-stage2-exit)

| Step | Txid / value |
|---|---|
| Commitment tx `C` (mined), output `C:2` = 20 000 sat to Alice | `d0906ea1cfc78bb17f172ddeac65fe8773fd9db5eea1ad8f8dfeae3d669d55c4` |
| Forfeit `F` (T_f: `V` → `C:2`, never mined) | `99b9db410e45982c22f129a134cf3101b238be7033a4f11ec31e68923f314917` |
| Alice spends `C:2` → Dave (on-chain) | `38d1d6e8b9b2aaff73ce0721cf62f7e39af18002c218fe083b2964f94dbdc6f8` |
| Dave's balance | `settled: 1000` |

### Negative tests (run-1792963787-stage2-neg)

| Case | Outcome |
|---|---|
| Forfeit with a 31-byte `OP_RETURN`, two `OP_RETURN`s, `OP_RETURN` at index 1 or worth 1 sat | Rejected by arkd (`INVALID_FORFEIT_TXS`) |
| Checkpoint with a 31-byte `OP_RETURN`, two `OP_RETURN`s or worth 1 sat | Rejected by arkd (`CHECKPOINT_MISMATCH`) |
| Forfeit submitted without `OP_RETURN`: attestation of the version with the commitment | Rejected (not in `GetForfeitTxs`) |
| Attestation with the wrong VTXO or an unmined commitment tx | Rejected |
| Consignment with an unattested forfeit (stock resolver, or Ark-aware without attestation) | `InvalidConsignment` for Bob, Carol and Dave |
| Positive control: forfeit with a well-formed 32-byte `OP_RETURN` | Accepted, attestation OK |

Regression on `v0.9.15-rgb2`: Stage 1 passed again (Dave `settled: 700`, 22:41 CET) and the two Stage 0 flows passed (Carol `settled: 1000`, 22:50 and 23:00 CET).

## Stage 3: offline proofs and exit watchtower (tested)

Passed on regtest between 25 Oct 2026 23:48 and 26 Oct 00:50 CET with `arkd:rgb` version `v0.9.15-rgb3`. Two results: whoever receives the asset can verify its whole history without asking arkd anything, and a watchtower can secure a colored VTXO on-chain before it expires.

### Part A: the history is verified without the operator

In Stage 2 every receiver had to ask arkd's indexer whether the forfeit was really registered. Now the sender attaches to the consignment a separate file (`.arkproof.json`) with all the unmined Ark transactions needed: forfeits, connectors, tree branches, checkpoints and ark txs of previous hops, down to a mined transaction. The receiver checks the file only against the blockchain.

- Every transaction must really spend the previous output, down to a mined commitment tx (the mempool is not enough).
- Signatures are verified: tree signatures against the output key; checkpoint, ark tx and forfeit signatures against the operator key fixed in the wallet configuration.
- Every RGB witness must have exactly one 32-byte, 0-sat `OP_RETURN` at index 0.
- Why a separate file: the RGB consignment has a fixed format and identifier; adding fields would change them.
- arkd patch 5: the operator also signs the forfeit it stores. Without its signature a dishonest sender could invent a forfeit with a fake connector and spend the asset twice.
- Not verified: connectors (not signed by arkd, checked only as a link), the MuSig signers of tree nodes, and whether the batch has already expired.

| Proof | Outcome with arkd paused |
|---|---|
| Stage 1: two off-chain hops, exit, on-chain send | Dave `settled: 700` |
| Stage 2 (a): refresh, hop, exit, on-chain send | Carol `settled: 1000` |
| Stage 2 (b): collaborative exit, on-chain send | Dave `settled: 1000` |
| Tampered proofs: missing transaction, altered signature or output, missing or invented connector, forfeit without operator signature, wrong operator key, unmined commitment tx | All rejected |

Cost: about 2.5 kB per off-chain hop and about 1.8 kB per unmined forfeit (6–7 kB consignment). When the hops are mined through an exit, the proof empties (39 bytes); an unmined forfeit, instead, stays forever in subsequent proofs.

### Part B: the exit watchtower

The watchtower receives only the VTXO's already-signed exit transactions and a key of its own to pay fees: none of the user's keys. When 7 minutes are left before expiry (batch with a 17-minute expiry for the test) it asks the public indexer whether the VTXO was refreshed; if not, it publishes the exit branch, paying fees through the anchors.

| Test | Outcome |
|---|---|
| Alice watched: the watchtower takes the VTXO on-chain before expiry, Alice spends it on-chain to Gina | Gina `settled: 1000` |
| Erin unwatched (control): the operator sweeps | Asset stuck forever |
| The watchtower tries to redirect an output or to spend the VTXO with its key | Rejected (invalid signature) |
| Frank refreshes in time | The watchtower does nothing |

Fees paid by the watchtower at 2 sat/vB: 640 sat for the tree root and 554 sat for the leaf, 1 194 sat in total for a two-level branch. Each extra level or hop costs about 550–650 sat. After the sweep Erin's RGB wallet still shows `settled: 1000`, but the seal can never be spent. Erin recovers her 100 000 sat in a new VTXO; the asset does not, because the recovery does not go through a forfeit and so there is no transaction that can carry the RGB transition.

### Why expiry is more serious than for BTC

- On BTC-only Arkade an expired VTXO is not a drama: the operator sweeps, but the balance stays recoverable in a new batch. BTC has no history.
- An RGB asset, instead, is tied to a precise output (the seal) and to its history. The sweep spends the tree without an RGB commitment: the VTXO seal can never close, and recovering the BTC does not revive the asset.
- The delegator problem: refreshing while the user is offline would need a delegator. But the forfeit must commit to the RGB transition to the new VTXO, which is known only when the batch is already built, and the user's signature covers that commitment. The user would have to sign before knowing what they sign: circular. Signing without committing to the outputs would leave anyone free to change them.

1. **1. Automatic refresh while the wallet is online**: The normal case: the wallet refreshes colored VTXOs well before expiry (tested in Stage 2).
2. **2. Delegator, later**: Requires solving the circularity with arkd-side support. To be designed, not tested.
3. **3. Exit watchtower as a safety net**: If nobody refreshed, it takes the VTXO on-chain before the sweep. It costs on-chain fees, but the asset is safe (tested in Stage 3).

Regression on `v0.9.15-rgb3` (26 Oct, 00:49–01:20 CET): Stage 0 in both variants, Stage 1, Stage 2 (a) and (b) and the Stage 2 negative tests passed again.

## Stage 4: two weeks offline and data to send (USDt)

Two questions: how does the asset avoid being lost if the user stays offline for two weeks, and how much data and time are needed to receive a payment with a USDt-like asset. Tested on regtest on 26 Oct 2026 between 05:07 and 05:50 CET. In the tables, measured values are kept separate from estimated ones.

### Who keeps the asset alive while you are offline

| Option | Third-party trust for the asset | Cost | Assessment |
|---|---|---|---|
| (a) Longer batches (28 days) and automatic refresh every time the wallet opens if less than 14 days plus a margin are left | None | Paid by the operator, which locks BTC for longer. For a 1 000-sat colored VTXO it is about 4 sat. But the expiry in arkd applies to everyone, BTC-only VTXOs included. | Baseline solution |
| (b) Keyless watchtower, with fees already signed by the user (ladder from 1 to 64 sat/vB) | None: it cannot steal or redirect, at most force an early on-chain exit | Paid by the user with their fee UTXOs: about 550 sat per level at 2 sat/vB | Safety net |
| (c) Delegate that refreshes on the user's behalf | With the signatures possible today the delegate could redirect the asset | Trustless, it requires large arkd changes | Later, with Ark Labs |
| (e) Vacation mode: before leaving, the asset exits on-chain via collaborative exit | None, and no expiry | About 1 000 sat at 10 sat/vB, between exit and re-entry | Manual option |

> **Superseded by Stage 5.** Longer batches are no longer the proposal: with recovery after the sweep the asset survives even months offline.

Initial proposal (Stage 4): (a) as the baseline, (b) active automatically and run by KaleidoSwap, (e) available in the app, (c) to be discussed with Ark Labs. Who runs the watchtower? KaleidoSwap by default. The registration contains no secrets, so the user can also give it to a second watchtower, for example their own node, so that if one is down there is the other.

| Test (scale: 1 day = 73 s) | Measured outcome |
|---|---|
| Alice, no third parties: starts with 27.8 days of life, stays offline 15.7 days, returns (status: at risk, 12.1 days left), does the collaborative exit and pays Dave on-chain | Dave `settled: 1000` |
| Erin, with watchtower: stays offline 28.2 days, past the expiry. The daemon is stopped and restarted, then arkd is paused near the threshold. The daemon exits anyway, using the expiry written in the registration (554 sat of fees). Erin returns and pays Gina on-chain | Gina `settled: 1000` |
| Frank, with the watchtower down (control) | Sweep: asset lost, flagged as LOST; rgb-lib still shows 1000 |
| The watchtower alters the fee transaction to take the funds | Rejected (invalid signature) |

Honest note: during the negative test bitcoind accepted Erin's tree root at zero fee, which on regtest was mined immediately. In practice this is the case of a malicious watchtower forcing an early on-chain exit: it costs the user fees but takes nothing from them. The wallet now also has a status indicator (healthy, at risk, on-chain, lost) read from the public indexer.

### How much data is sent and how long verification takes (measured)

Cold verification: fresh wallet, with arkd paused. It includes importing the proof and validating the consignment.

| Off-chain hops | Consignment (gzip) | JSON proof | Binary proof (gzip) | Cold verification |
|---|---|---|---|---|
| 1 | 6.1 kB (3.3) | 2.9 kB | 1.1 kB (1.0) | 156 ms |
| 2 | 7.1 kB (3.9) | 5.4 kB | 2.1 kB (1.5) | 184 ms |
| 4 | 9.2 kB (4.9) | 10.4 kB | 3.9 kB (2.7) | 180 ms |
| 8 | 13.4 kB (7.0) | 20.4 kB | 7.7 kB (4.9) | 238 ms |
| 16 | 21.7 kB (11.1) | 40.4 kB | 15.2 kB (9.3) | 368 ms |

- Each off-chain hop adds about 1 kB of consignment, 2.5 kB of JSON proof (0.94 kB in binary) and 14.5 ms of verification.
- Each already-mined on-chain transfer adds 0.5 kB and 6 ms, with no Ark proof.
- Each refresh adds 1.8 kB of proof: refresh does not reset the proof, because each forfeit carries its VTXO's branch with it.
- Merging several inputs into one payment: with 1, 4 and 16 inputs the consignment is 5.6, 7.8 and 16.6 kB and verification takes 171, 169 and 319 ms.
- Where the weight comes from: the consignment is RGB history. The JSON proof is almost all PSBT structure: the actual transactions are about 340 bytes per hop.

| Optimization | Before | After (measured) |
|---|---|---|
| Compact binary proof: only the data the verifier reads, then converted back and verified by the same code | 40.4 kB (16 hops) | 15.2 kB (−62%) |
| Binary proof plus gzip | 40.4 kB | 9.3 kB (−77%) |
| gzip of the consignment | 21.7 kB | 11.1 kB (−49%) |
| Receiver cache: only the transactions it has not already verified are sent | 15.2 kB cumulative | about 1 kB per new hop |
| Already-mined transactions removed from the proof (already in Stage 3) | – | 39 bytes if everything is mined |

Not prototyped: history checkpoints signed by the operator (constant-size proof, but its signature must be trusted), periodic on-chain compaction (resets the Ark proof but not the RGB history) and batching.

### Extrapolation for a USDt-like asset (estimate, not measured)

| Transitions in the payment history | Consignment | Binary proof (10 unmined hops) | Cold verification |
|---|---|---|---|
| 10 | 15 kB | 9 kB | 0.3 s |
| 100 | 61 kB | 9 kB | 0.8 s |
| 1 000 | 0.5 MB | 9 kB | 6 s |
| 10 000 | 5 MB | 9 kB | about 1 minute |

For an asset that circulates a lot, with many small payments and payments merging coins from many senders, the history of each coin ends up including almost every transfer ever made. The Ark part stays bounded (about 1 kB per unmined hop, reset when going on-chain). The real limit is the RGB history, which always grows. The practical path is a KaleidoSwap hub that keeps the already-verified history, with differential consignments and a validation cache: each payment carries only the new part and latency stays under a second. It needs development in rgb-lib, but no consensus change.

Regression on `v0.9.15-rgb3` (26 Oct, 05:51–06:38 CET): Stage 0 in both variants, Stage 1, Stage 2 (b), the Stage 2 negative tests and the Stage 3 offline proofs passed again. Stage 2 (a) failed at startup because of a known electrum connection problem when several tests start together. It was relaunched alone and passed (Bob → Carol `64c3eea0…`). A few blocks had to be mined by hand, because nothing was mining anymore and the 512 s timelock could not mature.

## Stage 5: recovery after the sweep (months offline)

> **New recommendation for long absences.** There is no need to lengthen the batch expiry and no need for anyone to act while the user is offline. The batch expires, the operator sweeps; when the user returns, even after months, they recover the BTC and the RGB asset in the same step into a new VTXO.

How it works: Arkade already allows recovering the BTC of a swept VTXO, which stays "recoverable" in the operator's database. We made the asset travel the same road. On return, the wallet asks the operator to treat the swept VTXO like a live VTXO: it signs a forfeit, and inside the forfeit is the RGB commitment that moves the asset to the new VTXO. It is the same mechanism as the Stage 2 refresh, already verifiable offline with the Stage 3 proof.

Why not put the RGB commitment in the recovery request (the intent)? The intent is signed before the new batch exists, so it cannot say where the asset will go. The forfeit, instead, is signed later, when the new VTXO is known. Moreover, arkd discards a free-form OP_RETURN in the intent.

| Change | Detail |
|---|---|
| arkd (patch 6, about 25 lines) | New signed field in the intent, `rgb_recovery_forfeit`. If the VTXO is swept or expired, arkd still requires the forfeit, which patch 5 co-signs and stores. Normal recoveries without the field do not change. |
| Ark client (rust-sdk, about 40 lines) | Adds the field and creates the forfeit for the swept VTXO too. |
| rgb-lib and RGB consensus | No changes |

| Test (scale: 600 s batch = 7 days) | Measured outcome |
|---|---|
| Development run: Alice offline 709 s, after the sweep. She returns, the wallet flags the asset as LOST, recovers it (commitment `0966f28d…`, forfeit `d241138b…`), then exits on-chain and pays Dave (`fb5e07d6…`) | Dave `settled: 1000`, verified with arkd paused |
| Official run: Alice offline 5 570 s, about 64 days (over 2 months). She returns at 09:15 CET, the wallet flags the asset as LOST, recovers it (commitment `be09965e…`, forfeit `ad0e16d0…`, new VTXO `d6d3d604…:0`), then exits on-chain and pays Dave (`16afc92b…`) | Dave `settled: 1000`, verified with arkd paused |
| Mallory signs a recovery intent in Alice's place | Rejected by arkd (invalid signature) |
| Second recovery of the same VTXO | Rejected (VTXO already spent) |
| Recovery proof without the operator signature, or tampered (7 variants) | All rejected |
| The operator rewrites the RGB commitment or signs alone to move the asset | Rejected |

**Pros**

- No action while offline, no watchtower, no longer expiry.
- The operator cannot steal or redirect the asset: the owner's signature is required.
- Safer than a normal refresh: after the sweep the old VTXO can no longer reach the chain, so there is no race with a unilateral exit.
- Costs the same as a refresh; the offline proof is 1.8 kB.

**Cons**

- The operator can refuse the recovery: the asset stays frozen, not destroyed, as BTC is today.
- If the operator and the user collude they can co-sign two forfeits and spend twice. It is the same trust as the Stage 2 refresh, and it leaves public proof of the fraud.
- The operator must keep the swept VTXO's data: arkd v0.9.15 never deletes it. The wallet keeps its own proof anyway.

Updated recommendation: recovery after the sweep (Stage 5) as the baseline, without lengthening the expiry. The Stage 4 watchtower becomes optional, for those who want an on-chain exit that does not depend on the operator's cooperation. Vacation mode remains available. The 28-day batch is no longer needed.

Regression on `v0.9.15-rgb4` (26 Oct, 07:50–08:45 CET): Stage 0 in both variants, Stage 1, Stage 2 (a) and (b), the Stage 2 negative tests and the Stage 3 offline proofs passed again. Stage 2 (a), which had failed at startup in Stage 4, now passes. The Stage 4 regression (two weeks offline with watchtower, 09:16–09:54 CET) also passed: Alice refreshes and pays Dave (`30bc097f…`), the watchtower takes Erin's VTXO on-chain and she then pays Gina (`99150e5d…`), and Frank (watchtower down) ends up LOST as expected.

## Stage 6: spike closure

> **All five items passed on regtest (26 Oct 2026).** Batches with several users and two assets, a failed and retried batch, correct rgb-lib balance for swept VTXOs, on-chain sweep check, fraud proof. arkd did not change: the work is in rgb-lib and in the client.

| Item | What we did | Measured outcome |
|---|---|---|
| 1. Multi-user batch | First an off-chain send with change: Alice sends 300 units of asset A and 20 000 sat to Dave, and gets back 700 A plus the BTC change (ark tx `efbb46f6…`). Then a single batch (commitment `3651a9cd…`) with four users: Alice refreshes her 700 A, Dave his 300 A, Bob passes 1000 units of asset B to Frank, Carol refreshes BTC only. | Passed: same batch for everyone, each receiver verifies with arkd paused; balances Alice 700 A, Dave 300 A, Frank 1000 B, Bob 0 |
| 2. Failed batch | In the same batch Bob does not sign his forfeit: arkd cancels the round ("missing forfeit tx"). Alice had already colored and signed her forfeit. rgb-lib shows the asset as "in flight"; the new function `ark_rollback_transfer` cancels the never-completed transition and Alice retries. | Passed: second attempt succeeded (commitment `d26d961e…`), Gina receives 1000 offline; rollback of an already-attested forfeit is refused |
| 3. rgb-lib balance | New functions `ark_vtxo_status` and `get_asset_balance_ark`: the VTXO status is derived from the blockchain alone (active, to recover, exited on-chain, in flight). Transfers with custom seals now show up in `list_ark_transfers`. | Passed: after the sweep the balance is "to recover 1000", after recovery "active 1000" (Stage 5 run repeated) |
| 4. Sweep check | A forfeit declared as a recovery is accepted only if the blockchain shows that the VTXO's branch was swept (batch output spent by a tx that is not part of the branch). | Passed: the official Stage 5 proof passes; a normal forfeit declared as a recovery is rejected |
| 5. Fraud | Simulation of a colluding operator and user: the operator edits its own database and co-signs a second forfeit of the same VTXO in another batch. Dave and Erin both receive 1000 units offline. | Detected, not prevented: the fraud proof (5 kB) is verifiable with the blockchain alone; Dave, online, sees that the indexer points to another forfeit |

### What is prevented and what is only detected

**Prevented**

- Moving the asset without the owner's signature.
- The operator moving the asset alone.
- A second forfeit of the same VTXO with an honest operator.
- Accepting a "recovery" of a VTXO that was not swept.
- Cancelling an already-attested forfeit.

**Only detected**

- The operator and the owner, colluding, can spend the same VTXO twice: two offline receivers both accept. A public proof remains, with two operator signatures on the same VTXO.
- Online, only the first receiver notices (the indexer points to the other's forfeit); the second sees everything as consistent.
- Rolling back a failed batch is safe as long as the failed round's commitment is never published; if the operator published it, we would fall back into the fraud case.
- Any sanctions (bonds, reputation) are outside Ark and RGB.

Regressions: Stage 5 repeated and passed. On the first attempt Alice had returned before the operator swept the batch, so rgb-lib still saw it as "active", which was correct; the script now waits for the sweep. Stage 0, 1, 2 (refresh, exit, negative tests), 3 and 4 repeated: all passed. Stage 4 failed once at startup because the expiry change was not active yet (same script timing problem, not a code problem); repeated with a 45-second wait, passed.

## Atomic swap on Arkade with RGB (theory only)

> **Untested proposal.** This section describes how an atomic swap could be done. No part has been tried on regtest; it does, however, build on already-tested pieces: commitment in the checkpoint and in the ark tx (Stage 1), commitment in the forfeit (Stage 2), offline proof (Stage 3) and rollback of a failed batch (Stage 6).

### Basic rule

In RGB the receiver must have the consignment, otherwise they cannot spend. So in every variant the two parties first exchange consignments and proofs (built on the not-yet-signed txs, whose txids are already fixed) and verify them. Only then do they sign. This way nobody signs without having already checked what they receive.

### (a) Asset for BTC in a single ark tx

1. **Construction**: A single ark tx with two inputs: Alice's colored VTXO (through her checkpoint) and Bob's BTC VTXO (through his checkpoint). Outputs: 0 = OP_RETURN with the commitment; then Bob's new VTXO with the asset, Alice's new VTXO with the BTC, and any change.
2. **Commitment**: Alice's checkpoint carries transition T1 (asset from Alice's VTXO to the checkpoint output), as in Stage 1. The ark tx carries T2 (from the checkpoint output to Bob's VTXO, plus any asset change to Alice). Bob's checkpoint has no commitment.
3. **Verification**: Alice gives Bob the consignment of T1+T2 and the proof with the unsigned PSBTs. Bob checks that the asset arrives at his VTXO and that the BTC for Alice are the agreed amount.
4. **Signatures**: Each party signs their own checkpoint and their own ark tx input (user leaf + operator). The PSBT passes from one to the other; then the client submits it to arkd, which co-signs.
5. **Outcome**: A single transaction: either both legs hold or neither does. Neither party can exit on-chain with their own leg without also the other's checkpoint.

### (b) Asset A for asset B

Same structure, but both inputs are colored. Each checkpoint carries its owner's T1. The ark tx OP_RETURN carries a single commitment containing two transitions (RGB allows several contracts in the same commitment): asset A to Bob and asset B to Alice. The PSBT is passed between the two parties and each adds its own transition before the final commitment is computed. A small rgb-lib function is needed to add to the PSBT the transition prepared by the other party.

### Same-batch variant

The two parties register a single intent with both parties' inputs (the BIP322 proof has one signature per input, from different owners) and crossed outputs. At finalization each signs the forfeit of their own VTXO with the RGB commitment to the other party's new VTXO (as in Stage 2). The batch is atomic: if a forfeit is missing, arkd cancels the round and nothing happens (measured in Stage 6, item 2). Whoever had already colored their forfeit rolls it back. Here atomicity depends on the operator as for any Ark batch: it must not publish the commitment without all the forfeits.

### HTLC variants (beyond Arkade's boundaries)

| Swap | How | Timing |
|---|---|---|
| Arkade ↔ on-chain | Alice locks the asset in a VHTLC (a VTXO with a "preimage + Bob + operator" path and a refund to Alice after a timeout); the RGB seal is the VHTLC outpoint. Bob locks BTC or another asset in an on-chain HTLC with the same hash. Bob claims the VHTLC by revealing the preimage (the spending tx carries the RGB commitment as in Stage 1); Alice reads the preimage and claims on-chain. | On-chain timeout longer than the VHTLC's, with a margin; the VHTLC must not expire (batch) before the refund: refresh or recovery after the sweep (Stage 5). |
| Arkade ↔ RGB Lightning (KaleidoSwap channels) | From Arkade to the channel: the user locks the asset in a VHTLC to the LSP; the LSP pays the user's RGB-LN invoice with the same hash; the user, claiming on LN, reveals the preimage and the LSP claims the VHTLC. The other way: the LSP locks the asset in a VHTLC to the user and the user pays an RGB-LN hold invoice from the LSP. | The VHTLC timeout must exceed the LN HTLC's, plus the time of a unilateral exit from Arkade (blocks to unroll the branch). |

### What can go wrong

- One party does not sign: the ark tx or the batch does not exist; the prepared transitions must be cancelled with the rollback (Stage 6).
- One party signs the ark tx but does not complete their checkpoint: the tx stays pending for both, because the ark tx spends both checkpoints; the operator can complete it later.
- Consignment not delivered after the swap: avoided by exchanging consignments before signing.
- HTLC: the preimage is revealed but the claim does not get in on time; or the operator censors the VHTLC spend and a unilateral exit is needed, which takes time and fees.
- Batch expiry during the swap: the locked VTXO must be refreshed or recovered after the sweep.

### Changes needed

- arkd: patches 1–6 already cover the commitment in checkpoint, ark tx and forfeit, and recovery after the sweep. It must be verified that the stock code accepts an ark tx with inputs from different owners and an intent with inputs from several owners (it should, but it is not proven).
- Ark client: multi-party signing of the same PSBT (passing the PSBT between the parties) and a VHTLC with an OP_RETURN in the claim tx.
- rgb-lib: adding to the same commitment a transition prepared by another party; automatic rollback if the swap fails.

### Trust and security

In the same ark tx, atomicity is that of a Bitcoin tx: no more trust than Arkade already requires for preconfirmed txs. In the batch variant, atomicity depends on the operator as for every batch. With HTLCs the usual timeout rules apply, plus the Arkade exit times. In all variants the Stage 6 fraud case remains: an operator colluding with one of the parties can co-sign a second spend of the same VTXO; it is detected, not prevented.

## Why direct onboarding of the colored UTXO does not work

Idea: issue on-chain, send the colored UTXO to the boarding address and have the commitment tx close the seal. On arkd v0.9.15 (`f863e48`) it is not possible, for four reasons verified in the source.

1. The commitment tx cannot contain a user `OP_RETURN`: the outputs are [batch, connector, intents' on-chain outputs, operator change] and the intents' on-chain outputs are accepted only as addresses (`getOnchainOutputs`: `DecodeBitcoinAddress` → `PayToAddrScript`).
2. The intents' "ARK" extension (`OP_RETURN "ARK" + TLV`) ends up in the batch leaf, not in the commitment tx, and its format is not a valid RGB commitment: rgb-consensus requires exactly `OP_RETURN <32 bytes>` (34-byte script).
3. Circularity of the new seal: any VTXO outpoint depends on the commitment txid, which would contain the very opret committing to that outpoint. A seal on the batch output would be lost, because the tree txs spend it without a commitment.
4. The VTXO txid is not predictable when the intent is signed: it depends on the operator's inputs and on all the intents of the batch. The client sees it only at the `TreeSigningStarted` event, after the intent is registered (but before signing its own boarding input).

## What can be done today and after the changes

| Feature | Today (stock) | Checkpoint patch | Forfeit patch |
|---|---|---|---|
| Deposit (jump-in) | Yes, tested | Yes | Yes |
| 1-hop send | Yes, tested | Yes | Yes |
| Multi-hop | No | Yes, tested (Stage 1) | Yes, tested after a refresh (Stage 2) |
| VTXO refresh | No, asset lost | No | Yes, tested (Stage 2) |
| Expiry | Asset destroyed | Asset destroyed | Timely refresh or exit watchtower (Stage 3); without, asset destroyed (tested) |
| Collaborative exit | No, asset lost | No | Yes, tested (Stage 2) |
| Unilateral exit | Yes, tested | Yes, linear cost | Yes, tested after a refresh |

Multi-hop today: Bob receives on the ark tx output; his next seal would have to be on the predicted checkpoint of his own VTXO, whose txid depends on the ark tx and therefore on the opret itself: circular. Without the patch Bob's checkpoint tx cannot carry a commitment.

Refresh and collaborative exit today: the commitment tx does not spend the VTXOs (its inputs are operator UTXOs and boarding inputs); the old VTXO is surrendered with the forfeit tx, which arkd rebuilds deterministically (outputs: operator + anchor) and compares by txid. The seal therefore stays on a VTXO that the user can no longer spend and that after the sweep can never exist on-chain: the asset is lost in practice. This corrects an earlier wording ("the commitment tx spends the VTXO without a commitment"): the result is the same, the mechanism is the forfeit.

## Recommended target model

### User experience

1. **Jump-in deposit**: On-chain asset → tx T1 to the predicted checkpoint of the user's VTXO; if the user has no VTXO, they get one via Lightning/Boltz (not tested) or with a BTC boarding.
2. **Instant multi-hop sends**: Every off-chain spend carries the commitment in the checkpoint and in the ark tx (requires patches 1 and 2).
3. **Automatic refresh**: The wallet refreshes colored VTXOs before expiry with forfeits carrying the commitment (requires patch 3).
4. **Collaborative exit**: The asset moves to the batch's on-chain output through the forfeit with commitment (requires patch 3); the unilateral exit remains the fallback.

### arkd patches in order of cost (1 and 2 tested in Stage 1, 3 in Stage 2, 5 in Stage 3; 4 an untested proposal)

| Patch | Cost | What it enables | Status |
|---|---|---|---|
| 1. Checkpoint tx with an optional 32-byte user `OP_RETURN`, included in the deterministic rebuild | Small | Multi-hop: the checkpoint closes the VTXO seal and moves the asset to the checkpoint output (vout 1, because the `OP_RETURN` must be the first output); the ark tx carries it to the receiver | Tested |
| 2. Distinguish the RGB opret from sub-dust VTXOs | Small | No burned sats or indexed pseudo-VTXOs (0-sat opret) | Tested |
| 3. Forfeit tx with a 32-byte `OP_RETURN` (0 sat, index 0), included in the deterministic rebuild | Small (2 files, +62/−1 lines) | Refresh and collaborative exit without losing the asset | Tested |
| 5. The operator also signs the forfeit it stores (`SignTransactionTapscript` in `finalizeRound`) | Minimal (8 lines) | Offline proof of the forfeit: no forfeit invented with a fake connector | Tested |
| 4. Direct onboarding: `OP_RETURN` in the commitment tx + a new batch phase | Large | Boarding of the colored UTXO; to be avoided | Not tested |

Patch 3: when the user signs the forfeit (`BatchFinalization` phase), commitment tx, tree and connectors are already known, so the new outpoint (new VTXO in a refresh, on-chain output in a collaborative exit) is known and does not depend on the forfeit. The forfeit can close the old VTXO's seal and move the asset without circularity. Stock arkd rejects any forfeit different from the rebuilt one (`VerifyForfeitTxs` compares txids); patch 3 extends the rebuild with the optional `OP_RETURN`, and that was enough (Stage 2).

### RGB side

- rgb-lib resolver for chains of off-chain witnesses: tested in Stage 1 (`MultiOffchainResolver`, witnesses from the consignment as `Tentative` and cross-check with `GetVirtualTxs`).
- Ark-aware validation: tested in Stage 2. An unmined forfeit is accepted as a witness only after attestation at the Ark level (`GetForfeitTxs` and `GetConnectors` of the mined commitment); the safe height follows the commitment's depth.
- Offline proof: tested in Stage 3. `import_ark_proof` verifies the attached file with the blockchain alone and records the proven witnesses; after that, normal validation no longer needs arkd.
- Wallet: track colored VTXOs, exclude them from blind refreshes and from BTC coin selection, schedule refreshes and exits before expiry.

## Security trade-offs

**What stays as in pure RGB**

- Asset data stays private: on-chain and towards the operator only a 32-byte commitment appears.
- Unilateral exit always possible: the asset reaches the chain with mined witnesses (demonstrated).
- Preconfirmed txs require trust in the operator as for BTC on Arkade, no more.

**What gets weaker or must be managed**

- With unmined forfeits (patch 3) seal uniqueness depends permanently on Ark: arkd must not accept a second spend of the same VTXO, and must publish the forfeit if the user unrolls the old VTXO. An operator that is offline or colluding before the sweep is the critical scenario; the anti-fraud path with a colored forfeit has not been tested.
- Every future receiver must receive the proof of the unmined forfeit. After Stage 3 arkd's indexer is no longer needed, but the proof file must be kept and passed along together with the consignment (about 1.8 kB per forfeit).
- Liveness: if a colored VTXO expires and is swept, the operator recovers the BTC but the asset is destroyed (demonstrated in Stage 3). Timely refreshes or an exit watchtower are needed, which costs on-chain fees.
- The exit cost grows linearly with the off-chain chain (every checkpoint and ark tx must be published).
- Selective censorship is possible: a 32-byte opret in the ark tx is recognizable.
- Opret only: less on-chain privacy on exit compared to tapret.
- Data availability: the receiver must keep the consignment; without it the asset cannot be spent.

## Next steps

1. **Stage 1 (done, 25 Oct 2026)**: arkd patches 1 and 2, multi-witness rgb-lib resolver, multi-hop test on regtest: passed.
2. **Stage 2 (done, 25 Oct 2026)**: Patch 3 (forfeit with commitment) and Ark-aware validation; refresh, collaborative exit and negative tests passed.
3. **Stage 3 (done, 26 Oct 2026)**: Offline proof attached to the consignment (arkd patch 5) and exit watchtower: all tests passed.
4. **Wallet**: rgb-lib DB bookkeeping for custom seals, automatic refresh before expiry, watchtower as a service, delegator design.
5. **Discussion with Ark Labs**: Share the patches as a proposal and collect answers to the questions below.

### Open questions for Ark Labs

- Would you accept an optional 32-byte user `OP_RETURN` in the deterministic rebuild of checkpoints and forfeits?
- How stable is the checkpoint tapscript (`checkpointTapscript`)? A rotation breaks the prediction.
- Can an opret be excluded from sub-dust VTXOs, or a value of 0 be allowed?
- Can `GetVirtualTxs` or an equivalent API serve off-chain chains to an RGB resolver?
- Would you accept the operator also signing the stored forfeit, so that it is verifiable without arkd (patch 5)?
- How could a delegator refresh a colored VTXO, given that the new VTXO is known only once the batch is built?
- How long does the indexer keep a batch's `GetForfeitTxs` and `GetConnectors`? Colored forfeits remain necessary to all future validators.
- The operator's checkpoint publication policy during exits, and sweep semantics for non-standard outputs.

## References

- [arkd @ f863e48: commitment tx and forfeit verification (builder.go)](https://github.com/arkade-os/arkd/blob/f863e484719344edbe4a8d10cf5fe994b123f2c0/internal/infrastructure/tx-builder/covenantless/builder.go)
- [arkd @ f863e48: on-chain outputs and intent leaves (utils.go)](https://github.com/arkade-os/arkd/blob/f863e484719344edbe4a8d10cf5fe994b123f2c0/internal/infrastructure/tx-builder/covenantless/utils.go)
- [arkd @ f863e48: forfeit tx (forfeit_tx.go)](https://github.com/arkade-os/arkd/blob/f863e484719344edbe4a8d10cf5fe994b123f2c0/pkg/ark-lib/tree/forfeit_tx.go)
- [arkd @ f863e48: ARK extension (extension.go)](https://github.com/arkade-os/arkd/blob/f863e484719344edbe4a8d10cf5fe994b123f2c0/pkg/ark-lib/extension/extension.go)
- [arkd @ f863e48: intent and extension handling (service.go)](https://github.com/arkade-os/arkd/blob/f863e484719344edbe4a8d10cf5fe994b123f2c0/internal/core/application/service.go)
- [rust-sdk @ f184a5f: send.rs (checkpoint and ark tx)](https://github.com/arkade-os/rust-sdk/blob/f184a5f1aa77924b7fd40eede7cf2fd43854ae76/ark-core/src/send.rs)
- [rust-sdk @ f184a5f: batch.rs (batch events)](https://github.com/arkade-os/rust-sdk/blob/f184a5f1aa77924b7fd40eede7cf2fd43854ae76/ark-client/src/batch.rs)
- [rgb-lib (KaleidoSwap fork) @ 8852b2e](https://github.com/kaleidoswap/rgb-lib/tree/8852b2e55606ebd568f56bdfd9fa9216fbec3435)
- [rgb-consensus 0.11.1-rc.11: opret verification (spk.rs)](https://docs.rs/crate/rgb-consensus/0.11.1-rc.11/source/src/dbc/opret/spk.rs)
- [arkade-regtest @ 457b9ef (arkd v0.9.15)](https://github.com/ArkLabsHQ/arkade-regtest/tree/457b9ef6119bea1b0a8f1356204ef2f1044b0c9b)
