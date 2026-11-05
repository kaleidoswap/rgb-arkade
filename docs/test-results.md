# Test results

Everything here was measured on regtest (arkd v0.9.15 + the patch set, rust-sdk v0.12.0, rgb-lib `8852b2e` + patch) on 25–26 Oct 2026, unless marked as an estimate. Scenario names refer to `ark-rgb-demo scenario <name>` / `scripts/run-scenario.sh <name>`. Run directories and raw logs are not kept in the repository.

## basic / basic-onchain-genesis

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

The "issuance on a VTXO" flow also passed (run6, ark tx `30789e00…a21b`, Bob → Carol `7aa8cd5d…e29f`). Logs and consignments were kept locally, outside the repository.

## offchain-transfer

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

## refresh, collab-exit, forfeit-negatives

### refresh: refresh → hop → exit → on-chain

| Step | Txid / value |
|---|---|
| Refresh commitment tx `C1` (mined) | `ef6fd7210843eb3713efc5161e33bd6f1c702d26b88e574cacab97147fb46716` |
| Forfeit `F` (T_f: `V` → `V'`, never mined) | `9e13241a971c0908381ce50608e2b932b2539306e111a24f288849ee6d50bdbe` |
| New VTXO `V'` | `0c68de737f5decc5be82f51db71bd185417f16f83517f1caf715f251b6730545:0` |
| Checkpoint / ark tx `V'` → Bob | `df3530e1…fe88` / `bcb20f4f…0cf2` |
| Bob's unilateral exit | leaf `V'`, checkpoint and ark tx mined; `F` stays unmined |
| Bob → Carol (on-chain) | `75e98bbae798d8a2942878f85315ea98170a63f29276915598f0391655d89175` |

### collab-exit: collaborative exit → on-chain

| Step | Txid / value |
|---|---|
| Commitment tx `C` (mined), output `C:2` = 20 000 sat to Alice | `d0906ea1cfc78bb17f172ddeac65fe8773fd9db5eea1ad8f8dfeae3d669d55c4` |
| Forfeit `F` (T_f: `V` → `C:2`, never mined) | `99b9db410e45982c22f129a134cf3101b238be7033a4f11ec31e68923f314917` |
| Alice spends `C:2` → Dave (on-chain) | `38d1d6e8b9b2aaff73ce0721cf62f7e39af18002c218fe083b2964f94dbdc6f8` |

### forfeit-negatives

| Case | Outcome |
|---|---|
| Forfeit with a 31-byte `OP_RETURN`, two `OP_RETURN`s, `OP_RETURN` at index 1 or worth 1 sat | Rejected by arkd (`INVALID_FORFEIT_TXS`) |
| Checkpoint with a 31-byte `OP_RETURN`, two `OP_RETURN`s or worth 1 sat | Rejected by arkd (`CHECKPOINT_MISMATCH`) |
| Forfeit submitted without `OP_RETURN`: attestation of the version with the commitment | Rejected (not in `GetForfeitTxs`) |
| Attestation with the wrong VTXO or an unmined commitment tx | Rejected |
| Consignment with an unattested forfeit (stock resolver, or Ark-aware without attestation) | `InvalidConsignment` for Bob, Carol and Dave |
| Positive control: forfeit with a well-formed 32-byte `OP_RETURN` | Accepted, attestation OK |

## Offline proofs (`--offline-proof`)

| Proof | Outcome with arkd paused |
|---|---|
| the off-chain transfer scenario: two off-chain hops, exit, on-chain send | Dave `settled: 700` |
| the forfeit-commitment scenarios (a): refresh, hop, exit, on-chain send | Carol `settled: 1000` |
| the forfeit-commitment scenarios (b): collaborative exit, on-chain send | Dave `settled: 1000` |
| Tampered proofs: missing transaction, altered signature or output, missing or invented connector, forfeit without operator signature, wrong operator key, unmined commitment tx | All rejected |

Cost: about 2.5 kB per off-chain hop and about 1.8 kB per unmined forfeit (6–7 kB consignment). When the hops are mined through an exit, the proof empties (39 bytes); an unmined forfeit, instead, stays forever in subsequent proofs, and so does a checkpoint that an exit does not publish (every transfer and renewal in the checkpoint-only lane, see [design.md](design.md#offline-verification-ark-proofs)).

## exit-watchtower

| Test | Outcome |
|---|---|
| Alice watched: the watchtower takes the VTXO on-chain before expiry, Alice spends it on-chain to Gina | Gina `settled: 1000` |
| Erin unwatched (control): the operator sweeps | Asset stuck forever |
| The watchtower tries to redirect an output or to spend the VTXO with its key | Rejected (invalid signature) |
| Frank refreshes in time | The watchtower does nothing |

Fees paid by the watchtower at 2 sat/vB: 640 sat for the tree root and 554 sat for the leaf, 1 194 sat in total for a two-level branch. Each extra level or hop costs about 550–650 sat. After the sweep Erin's RGB wallet still shows `settled: 1000`, but the seal can never be spent. Erin recovers her 100 000 sat in a new VTXO; the asset does not, because the recovery does not go through a forfeit and so there is no transaction that can carry the RGB transition.

## long-offline

| Test (scale: 1 day = 73 s) | Measured outcome |
|---|---|
| Alice, no third parties: starts with 27.8 days of life, stays offline 15.7 days, returns (status: at risk, 12.1 days left), does the collaborative exit and pays Dave on-chain | Dave `settled: 1000` |
| Erin, with watchtower: stays offline 28.2 days, past the expiry. The daemon is stopped and restarted, then arkd is paused near the threshold. The daemon exits anyway, using the expiry written in the registration (554 sat of fees). Erin returns and pays Gina on-chain | Gina `settled: 1000` |
| Frank, with the watchtower down (control) | Sweep: asset lost, flagged as LOST; rgb-lib still shows 1000 |
| The watchtower alters the fee transaction to take the funds | Rejected (invalid signature) |

Honest note: during the negative test bitcoind accepted Erin's tree root at zero fee, which on regtest was mined immediately. In practice this is the case of a malicious watchtower forcing an early on-chain exit: it costs the user fees but takes nothing from them. The wallet now also has a status indicator (healthy, at risk, on-chain, lost) read from the public indexer.

## sweep-recovery / verify-recovery

| Test (scale: 600 s batch = 7 days) | Measured outcome |
|---|---|
| Development run: Alice offline 709 s, after the sweep. She returns, the wallet flags the asset as LOST, recovers it (commitment `0966f28d…`, forfeit `d241138b…`), then exits on-chain and pays Dave (`fb5e07d6…`) | Dave `settled: 1000`, verified with arkd paused |
| Official run: Alice offline 5 570 s, about 64 days (over 2 months). She returns at 09:15, the wallet flags the asset as LOST, recovers it (commitment `be09965e…`, forfeit `ad0e16d0…`, new VTXO `d6d3d604…:0`), then exits on-chain and pays Dave (`16afc92b…`) | Dave `settled: 1000`, verified with arkd paused |
| Mallory signs a recovery intent in Alice's place | Rejected by arkd (invalid signature) |
| Second recovery of the same VTXO | Rejected (VTXO already spent) |
| Recovery proof without the operator signature, or tampered (7 variants) | All rejected |
| The operator rewrites the RGB commitment or signs alone to move the asset | Rejected |

## multi-user, failed-batch, fraud, verify-recovery

| Item | What we did | Measured outcome |
|---|---|---|
| 1. Multi-user batch | First an off-chain send with change: Alice sends 300 units of asset A and 20 000 sat to Dave, and gets back 700 A plus the BTC change (ark tx `efbb46f6…`). Then a single batch (commitment `3651a9cd…`) with four users: Alice refreshes her 700 A, Dave his 300 A, Bob passes 1000 units of asset B to Frank, Carol refreshes BTC only. | Passed: same batch for everyone, each receiver verifies with arkd paused; balances Alice 700 A, Dave 300 A, Frank 1000 B, Bob 0 |
| 2. Failed batch | In the same batch Bob does not sign his forfeit: arkd cancels the round ("missing forfeit tx"). Alice had already colored and signed her forfeit. rgb-lib shows the asset as "in flight"; the new function `ark_rollback_transfer` cancels the never-completed transition and Alice retries. | Passed: second attempt succeeded (commitment `d26d961e…`), Gina receives 1000 offline; rollback of an already-attested forfeit is refused |
| 3. rgb-lib balance | New functions `ark_vtxo_status` and `get_asset_balance_ark`: the VTXO status is derived from the blockchain alone (active, to recover, exited on-chain, in flight). Transfers with custom seals now show up in `list_ark_transfers`. | Passed: after the sweep the balance is "to recover 1000", after recovery "active 1000" (sweep recovery run repeated) |
| 4. Sweep check | A forfeit declared as a recovery is accepted only if the blockchain shows that the VTXO's branch was swept (batch output spent by a tx that is not part of the branch). | Passed: the official sweep recovery proof passes; a normal forfeit declared as a recovery is rejected |
| 5. Fraud | Simulation of a colluding operator and user: the operator edits its own database and co-signs a second forfeit of the same VTXO in another batch. Dave and Erin both receive 1000 units offline. | Detected, not prevented: the fraud proof (5 kB) is verifiable with the blockchain alone; Dave, online, sees that the indexer points to another forfeit |

## Payload size and validation time (bench)

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
| Protobuf proof (the format now in use: full PSBTs as raw bytes, no base64/JSON), 2026-11-03 run | 40.2 kB (gzip 16.3) | 29.6 kB (−26%), gzip 11.0 kB (−33%); see `docs/protobuf.md` |
| gzip of the consignment | 21.7 kB | 11.1 kB (−49%) |
| Receiver cache: only the transactions it has not already verified are sent | 15.2 kB cumulative | about 1 kB per new hop |
| Already-mined transactions removed from the proof (already in the offline-proof work) | – | 39 bytes if everything is mined (never the case for forfeits, nor for checkpoint-only transfers and renewals) |

Not prototyped: history checkpoints signed by the operator (constant-size proof, but its signature must be trusted), periodic on-chain compaction (resets the Ark proof but not the RGB history) and batching.

## Extrapolation for a USDt-like asset (estimate, not measured)

| Transitions in the payment history | Consignment | Binary proof (10 unmined hops) | Cold verification |
|---|---|---|---|
| 10 | 15 kB | 9 kB | 0.3 s |
| 100 | 61 kB | 9 kB | 0.8 s |
| 1 000 | 0.5 MB | 9 kB | 6 s |
| 10 000 | 5 MB | 9 kB | about 1 minute |

For an asset that circulates a lot, with many small payments and payments merging coins from many senders, the history of each coin ends up including almost every transfer ever made. The Ark part grows with every tx that is never mined: about 1 kB (binary) per unmined hop. Going on-chain resets it only for hops the exit publishes; forfeits, and every checkpoint in the checkpoint-only lane, stay in the proof for the life of the asset, so there the Ark part grows with the history too. The real limit is the RGB history, which always grows. The practical path is a KaleidoSwap hub that keeps the already-verified history, with differential consignments and a validation cache: each payment carries only the new part and latency stays under a second. It needs development in rgb-lib, but no consensus change.

## Later runs and fixes

The suite was rerun sequentially after every change set (parallel runs trip the electrum indexer). Defects the runs found, all fixed:

- **Proof verifier:** a witness whose inputs were all key-path signed (an on-chain UTXO spent by a never-mined tx signed by its owner alone) was accepted; every witness now needs an input with a fully signed tapleaf containing the operator key.
- **Exit spend picked the wrong output:** coin selection could spend another exited output of the same contract instead of the colored one; the exit spend now names its exact inputs (`multihop-exit`).
- **Operator forfeit broadcast:** after an unroll of a forfeited output, arkd's stored colored forfeit is now broadcast (`multihop-exit`).
- **Spending an asset received on-chain:** the 2,000-sat output panicked on change; the spend adds a plain UTXO for the fee (`collab-exit`, Dave to Erin).
- **Stock balance after Arkade spends:** rgb-lib kept counting allocations moved by txs it did not broadcast; custom coloring now marks those inputs spent.
- **Fast profile:** with every arkd delay in blocks (exit 10, boarding 20, expiry 500) the quick suite runs in about 16 min instead of 1 h 30; unilateral exits mature in about 100 s.

### arkd master with three changes (28 Oct 2026)

arkd master `f863e48` (v0.9.16 + the btcd v2 migration) with only three changes: data `OP_RETURN` in the checkpoint (with the checkpoint output at vout 1), the same in the forfeit, and a 0-sat `OP_RETURN <32 bytes>` taken as data instead of a sub-dust VTXO. `forfeit-cosign` is dropped (master #1110 signs forfeits at collection time), and so is `recovery-forfeit`. Built from the fork branch [`rgb-opret-data-outputs`](https://github.com/bitwalt/arkd/tree/bee4a07bc9135c08d229c19e7c4b97d3dba82b8b) (not from a patch in this repository). Image `arkd:rgb-master` on `arkd:v0.9.16`, `arkd-wallet:v0.9.16`, fast profile: the quick suite passes, 11 of 11 (`forfeit-negatives` after making its positive control use a fresh wallet: master bans a VTXO after repeated invalid forfeits). `multihop-exit` passes, so the operator broadcasts its stored forfeit without our co-sign patch. Not run on master: `sweep-recovery` (needs `recovery-forfeit`) and the other time-scaled scenarios.

Found while rebasing, fixed in both patches: on finalizing an off-chain tx arkd watched `TxOut[0]` of each checkpoint, which is the data `OP_RETURN` when present; the wallet rejected the script, so that checkpoint went unwatched until the next restart.

### Atomic swap in one Arkade tx (29 Oct 2026, fast profile)

`swap`: Alice holds 1000 units on a VTXO (100,000 sat), Bob a bitcoin-only VTXO (100,000 sat). One Arkade tx spends both: checkpoint(Alice) carries T1, checkpoint(Bob) nothing, the ark tx carries T2 and pays [OP_RETURN, Bob 1,000 sat + 400 units, Alice 129,000 sat + 600 units, Bob 70,000 sat]. arkd accepts an off-chain tx whose inputs belong to two owners.

| Case | Outcome |
|---|---|
| Bob walks away after Alice prepared T1/T2 | Nothing submitted; Alice rolls T2 and T1 back and holds 1000 on her VTXO again |
| Alice's T2 gives Bob 300 instead of 400 | Bob's check before signing (`validate_consignment_offchain` over the unsigned txs) refuses; Alice rolls back |
| Agreed swap | Bob signs his ark tx input and his checkpoint before the submit; Alice submits, adds Bob's signature to the server-signed checkpoint and finalizes alone (arkd accepts the pre-signed checkpoint). Bob validates with arkd paused: `Fungible(400)`; Alice: active 600, new VTXO 129,000 sat |

PASS in about 30 s.

### Stock arkd lane (29 Oct 2026, `REGTEST_PROFILE=stock`: unpatched `arkd v0.9.15`, vtxoMinAmount 1, dust 330)

On stock arkd an RGB opret (`OP_RETURN <32 bytes>`, the only shape RGB accepts) is the sub-dust VTXO format: it must be worth at least vtxoMinAmount and less than dust (code read on v0.9.15 and master f863e48), and checkpoints carry nothing.

| Check | Outcome |
|---|---|
| `basic` (jump-in onto the predicted checkpoint, one hop with a 1-sat opret, unilateral exit, on-chain send) | PASS, Carol `settled: 1000` |
| `stock-lane` swap: Alice's freshly jumped-in asset for Bob's bitcoin in one Arkade tx, commitment only in the ark tx (1-sat opret) | Accepted by stock arkd; Bob checks before signing, signs up front, validates with arkd paused: `Fungible(400)`; Alice's new VTXO 128,999 sat |
| `swap` (patched lane: 0-sat opret, commitment in the checkpoint) on stock arkd | Rejected: `AMOUNT_TOO_LOW: output #0 amount is lower than min vtxo amount` |
| Second hop: Bob's checkpoint with a commitment | Rejected: `CHECKPOINT_MISMATCH` |
| Is the opret output indexed as a VTXO? | Yes: a 1-sat VTXO (marked swept) |

So on a stock operator an RGB asset can enter Arkade (jump-in) and move once, including as one leg of an atomic swap, then has to leave by unilateral exit before its batch expires.

### One arkd change: checkpoint data OP_RETURN (29 Oct 2026, scenario `checkpoint-renew`)

arkd master `f863e48` plus only the checkpoint commit (image `arkd:rgb-cponly`, profile `cponly`, branch `rgb-checkpoint-opret` of the arkd fork): no forfeit change, no 0-sat OP_RETURN change. Standard RGB (rgb-consensus 0.11.1-rc.11 from crates.io). The commitment sits in the checkpoint, the tx that spends the VTXO, so every transfer is plain RGB; the new seal is a VTXO the receiver already holds (like an RGB invoice on an existing UTXO), so nothing depends on the ark tx.

| Check | Outcome |
|---|---|
| Asset issued on-chain and moved onto Alice's VTXO V (batch 1) | Validated |
| Alice holds G, Bob holds H, both from a batch 240 blocks later | Expire after V |
| Renew: one Arkade tx, the checkpoint of V commits V → G | Accepted |
| Batch 1 past its tree expiry (+270 blocks) | The bitcoin left on it is swept by arkd |
| Send: the checkpoint of G commits G → H | Accepted |
| Bob validates before the checkpoints are attested (negative) | Rejected |
| Bob validates offline (arkd paused): consignment + Ark proof (2 checkpoints) | `1000` received on H |
| arkd restart with these checkpoints stored | Serves again within 90 s |

So with this single opt-in change an RGB asset can move any number of times and live past its batch without the forfeit: the owner (or a delegate) renews it onto a fresher VTXO before expiry. A colored VTXO must still never be settled in a batch.

### Swap and send to a new receiver with the same single change (30 Oct 2026, scenario `checkpoint-swap`)

Same stack as `checkpoint-renew` (arkd master + checkpoint data OP_RETURN only, standard RGB). Every transition sits in the seller's checkpoint and pays VTXOs that already exist (explicit seals), so the ark tx carries no commitment. rgb-lib's `validate_consignment_offchain` now also reports what a consignment puts on an explicit seal, so a buyer can check it before signing.

| Check | Outcome |
|---|---|
| Alice holds 1000 units on S and a spare VTXO C; Bob holds bitcoin on B and a spare VTXO H | Set up |
| Alice's checkpoint gives Bob 300 instead of the agreed 400 (negative) | Bob refuses before signing; Alice rolls back |
| Swap in one Arkade tx: Alice's checkpoint commits S → H (400) + C (600), the ark tx pays Alice 30,000 sat from B | Accepted; Alice's new VTXO holds 130,000 sat |
| Bob validates offline (arkd paused): consignment + Ark proof (1 checkpoint) | `400` received on H |
| Alice's change | `600` on C |
| Bob pays Carol, who has no VTXO, 1000 sat of plain bitcoin | Creates Carol's VTXO R |
| Bob's checkpoint of H commits H → R | Accepted |
| Carol validates offline (arkd paused): consignment + Ark proof (2 checkpoints) | `400` received on R |

So with only the checkpoint change, atomic swaps work, and a receiver with no VTXO can be paid in two steps: bitcoin first, which creates the VTXO, then the asset onto it. A 0-sat data OP_RETURN in the ark tx would only save that first step.

### Offline users with the same single change (30 Oct 2026, scenario `checkpoint-offline`)

Same stack (arkd master + checkpoint data OP_RETURN only, standard RGB, tree expiry 500 blocks). Alice holds 1000 units on V (batch 1) and a fresher VTXO G (batch 2, 246 blocks later), then goes offline.

| Check | Outcome |
|---|---|
| Alice pre-signs the renewal V → G (her ark tx input and her checkpoint) and G's exit branch, hands them over, goes offline | Nothing secret handed over |
| A delegate with no Alice key changes the ark tx to pay itself (negative) | Rejected by arkd: `INVALID_SIGNATURE` |
| The delegate submits the pre-signed renewal 224 blocks after Alice signed, shortly before batch 1 expires | Accepted and finalized |
| Batch 1 past its expiry | Swept; the asset is safe on G |
| Shortly before batch 2 expires, a keyless watchtower broadcasts G's branch (1 tx, CPFP through the P2A anchor, 554 sat) | G on-chain, not swept |
| Alice reloads her wallet from its seed and spends G on-chain to Gina, the commitment in her exit spend | Confirmed |
| Gina validates with plain RGB (negative) | Rejected: the renewal checkpoint never reached the chain |
| Gina imports the Ark proof of that checkpoint | `1000` settled |

So with only the checkpoint change:

- **Delegated renewal works with nothing new.** A renewal hop is an ordinary off-chain tx: the owner signs it ahead of time, anyone can submit it later, and the signature stops the delegate from changing where the bitcoin goes. The asset itself is bound to the destination VTXO by the checkpoint commitment, so a delegate can only submit the hop or not. Limit: the destination VTXO must exist when the owner signs, so a pre-signed hop can only reach the expiry of the freshest VTXO the owner holds at that moment, about one batch period more. Chaining hops does not go further (every destination already exists and expires with its own batch), and letting Arkade's delegator refresh the destination spends it and invalidates the hop. Past that point only the watchtower exit, or a delegated refresh that arkd supports for colored VTXOs, keeps the asset.
- **A keyless watchtower saves a user who stays offline past the last expiry**, by putting the colored VTXO on-chain from its pre-signed branch. The user can spend it on-chain at any later time.
- **After an exit, the history still contains off-chain checkpoints**, because a renewal moves the asset onto a VTXO from another batch: unrolling that VTXO does not bring the checkpoint on-chain. On-chain receivers then need the Ark proof (operator co-signed checkpoints anchored in mined batch commitments). A plain RGB wallet rejects the transfer.

### Unilateral exit through a swap (30 Oct 2026, CLI)

A swap's Arkade tx spends two checkpoints (seller and buyer). The rust-sdk exit finalizer filled in only input 0 of each virtual tx, so broadcasting a branch through a swap failed (`Witness program was passed an empty witness`, input 1), and in a wallet holding both a transfer output and a swap output the exit could also stop with an index out of range. It now finalizes every input of each virtual tx with its own parent output.

| Check | Outcome |
|---|---|
| Alice sends Bob 300, then sells him 200 for 20,000 sat; Bob exits the 300 and sends them on-chain to Alice | Alice `settled: 300` |
| Bob exits the 200 bought in the swap | Whole branch on-chain: both batch branches, both checkpoints and the swap tx |
| Bob sends those 200 on-chain | Not possible from the 1,000-sat carrier alone (needs a funding input) |

### Rerun on the review fixes (1 Nov 2026, fast profile)

Commit `d3e2a14`. The images and the demo were built from a fresh clone by `scripts/setup.sh` and `scripts/setup-cponly.sh` at `eeb8656`; `d3e2a14` changes only a message in `setup-cponly.sh`. `make patches-check` passed for all five patches. The stack ran in its own Docker daemon (docker:dind, no published ports), separate from any other stack on the host.

| Image | ID | `arkd --version` |
|---|---|---|
| `arkd:rgb-mvp` (full patch set, used as `arkd:rgb`) | `sha256:1410f715618f4e9bf98e643245cd5ee408107d548752a60f1db086f6d327ae7a` | `v0.9.15-rgb` |
| `arkd:rgb-mvp-cponly` (used as `arkd:rgb-cponly`) | `sha256:c8d649e42701f4251b55a43a93e6b0955f1499b6f01beb942e454e7ba92947ca` | `v0.9.16-master-f863e48-rgb-cponly` |
| `ghcr.io/arkade-os/arkd:v0.9.15` (stock lane) | `sha256:baeb57c10a0597207c700aaff8844ddaa0d50a557c30bd07ef62ae967ef06f21` (repo digest) | `v0.9.15` |

| `scripts/run-all.sh` mode | Scenarios | Result |
|---|---|---|
| `quick` | basic, basic-onchain-genesis, offchain-transfer, refresh, collab-exit (each also with `--offline-proof`), forfeit-negatives, multi-user, failed-batch, fraud, multihop-exit, swap | 16 of 16 PASS in 27 min |
| `cponly` | checkpoint-renew, checkpoint-swap, checkpoint-offline | 3 of 3 PASS |
| `stock` | basic, opret-probe, stock-lane | 3 of 3 PASS |

The time-scaled scenarios (`sweep-recovery`, `long-offline`, `exit-watchtower`) were run separately (next section).

### Time-scaled scenarios and bench on merged main (1 Nov 2026, seconds profile)

Commit `279d084`: main `eee1a0e` plus a separate log file for `--offline-proof` runs. These are the same images as the rerun above (`arkd:rgb-mvp` `sha256:1410f715618f…`, built from the same patches). The stack ran in its own Docker daemon. Times are Europe/Rome (CET).

| Scenario | Run | Result | Key numbers |
|---|---|---|---|
| `sweep-recovery 720` (expiry 600 s ≈ a 7-day batch) | 15:34–15:51, 1022 s | PASS, and `verify-recovery` PASS | Alice is offline 720 s past the start and the batch is swept. On return she is flagged LOST until recovery, and rgb-lib reports `to_recover 1000`. One intent forfeits the swept VTXO into a new batch with the RGB transition in the forfeit; a second recovery intent is refused (`VTXO_ALREADY_SPENT`). Proof sizes: 1825 B (3 PSBTs) for the recovery, 3695 B (6 PSBTs) for Alice → Dave, who ends with `settled: 1000`. Tampered or incomplete proofs and an operator-only signature are rejected. |
| `long-offline` (expiry 2048 s = 28 days, 1 day ≈ 73 s) | 15:51–16:36, 2663 s | PASS | Alice is offline 15.2 days and returns with 12.6 days left; Dave ends with `settled: 1000`. Erin is offline 28.3 days: the watchtower survives a kill and restart and an arkd pause, exits her 292 s before expiry for 554 sat in fees, and Gina ends with `settled: 1000`. A fee child altered by the watchtower is rejected (invalid signature). Frank (control, watchtower down) is swept and flagged LOST. The sweep tx was mined at 16:35:54, about 8.5 min after his expiry, which the scenario's retries cover. |
| `exit-watchtower` (expiry 1024 s) | 16:36–17:07, 1900 s | PASS | Alice, Erin and Frank board the same batch. The watchtower unrolls Alice's 3-level branch 230 s before expiry, each level as a zero-fee parent plus a pre-signed child at 2.01 sat/vB: 640 + 640 + 554 = 1834 sat. Alice then pays Gina on-chain (`settled: 1000`). Erin (control, unwatched) is swept. |
| `scripts/bench.sh` | 17:07–17:15, 444 s | done | 16 off-chain hops: consignment 21.7 kB (11.1 gz), JSON proof 40.4 kB, binary proof 15.2 kB (9.3 gz), cold verification 340 ms. Fits per hop: +1038 B consignment, +941 B binary proof, +12.6 ms. Refresh: +1788 B JSON proof per refresh. Mined transfer: +526 B, +5.3 ms. Merge: +732 B per input (16 inputs: 16.6 kB, 307 ms). Extrapolation (estimate): 100 transitions with 10 unmined off-chain gives 61 kB consignment, 9.2 kB binary proof, 0.72 s. |

These match the earlier figures in [Payload size and validation time](#payload-size-and-validation-time-bench) to within run-to-run noise.

### Delegate renewal, O2 + O1 (2 Nov 2026, scenario `delegate-renewal`, profile `cponly-seconds`)

arkd master f863e48 + patch B only (`arkd:rgb-cponly`), arkd-wallet v0.9.16, batch expiry 1024 s = 7 simulated days (1 day ≈ 146.3 s), renewal at half life, towers 150 s before expiry. Isolated docker:dind stack. Design and table per renewal: [delegate-renewal.md](delegate-renewal.md#results).

Smoke `delegate-renewal 12` (run-1793616742): **PASS**.

- Negatives: N1 `{D}`-only leaf rejected (`INVALID_VTXO_SCRIPT`, owner spend of that VTXO also rejected); N2 delegate before T rejected (`FORFEIT_CLOSURE_LOCKED`); N3 delegate after T **accepted** by arkd, Mallory's stock check `Ok(1000)`, policy check 1 violation; N4 #961 guard refuses.
- Olivia: 4 renewals over 14.1 days, 3–9 s each. Proof 4642 → 10117 B (+1825 B, +2 PSBTs per renewal; gz 2525 → 4325), consignment 5585 → 7169 B (+528 B). Import 11–22 ms, validate 178–231 ms, policy 9–30 ms. Discovery: 49 addresses, found the handoff VTXO. Sent to Bob: plain RGB rejected, with proof received 1000 (proof 11958 B, 12 PSBTs; import 20 ms, validate 213 ms).
- Rita (operator refusal after renewal 1): towers A and B `done-onchain`, F1 broadcast 146 s before expiry, fee 554 sat; guard refused re-board; CSV spend to Gina; Gina settled 1000.
- First smoke attempt hit a transient Electrum "tx can't be located" when Gina imported; `import_retry` and a resumable Rita return were added.

Long run `delegate-renewal 60` (run-1793621378, 2 Nov 13:09–15:44, 9279 s, no clock gaps): **PASS**, on a stack rebuilt after the box was re-provisioned (same arkd commit + B, binary rebuilt; images `arkd:rgb-delegate-cponly` / `arkd:rgb-delegate-full`, tagged `arkd:rgb-cponly` / `arkd:rgb` only inside the dind daemon).

- Negatives N1–N4: same results as the smoke run (N3 accepted by arkd, Mallory `Ok(1000)`, policy 1 violation).
- Olivia: 18 renewals over 62.8 simulated days, 3–14 s each. Proof 4642 → 35667 B (gz 2469 → 12665, 5 → 39 PSBTs), +1825 B per renewal throughout; consignment 5585 → 14561 B (+528). Import 19 → 85 ms, validate 251 → 378 ms, policy 16 → 393 ms. 11 failed attempts in total: 10 `FORFEIT_CLOSURE_LOCKED` (block time behind T, retried after 10–20 s) and 1 metrics-check `Stock error` after renewal 10 had completed (transient indexer lookup, since retried).
- Olivia's return (day 62.9): discovery 160 addresses, found the handoff VTXO with 6.8 days left; policy 19 witnesses / 18 delegate spends / 0 violations; validate+accept 518 ms. To Bob: plain RGB "Invalid consignment"; with proof received 1000 (proof 37508 B / 13462 gz, 40 PSBTs; consignment 15089 B; import 79 ms, validate+accept 393 ms).
- Rita: operator refusal at 13:20; 26 refused attempts; both towers broadcast at 13:34 (554 sat at 2 sat/vB); guard refused re-board; CSV spend to Gina, Gina settled 1000 (one import retry).
- Extrapolation: 60 real days at 7-day expiry and renewal at half life = 18 renewals, about 35.7 kB proof (12.7 kB gz); one year about 194 kB (65 kB gz). Why it is linear and how to compact it: [delegate-renewal.md](delegate-renewal.md#why-proof-growth-is-linear-and-what-60-real-days-costs).

Regressions on the same rebuilt isolated stack after the long run (2 Nov 2026):

| `scripts/run-all.sh` mode | Stack | Scenarios | Result |
|---|---|---|---|
| `quick` | `REGTEST_PROFILE=fast`, `arkd:rgb` (v0.9.15 + full patch set, rebuilt from `f444de6` + `patches/arkd.patch`) | basic, basic-onchain-genesis, offchain-transfer, refresh, collab-exit (each also with `--offline-proof`), forfeit-negatives, multi-user, failed-batch, fraud, multihop-exit, swap | 16 of 16 PASS in 21.6 min |
| `cponly` | `REGTEST_PROFILE=cponly`, `arkd:rgb-cponly` (master f863e48 + B) | checkpoint-renew, checkpoint-swap, checkpoint-offline | 3 of 3 PASS in 2.4 min |

### Threshold delegate, 2-of-3 (2 Nov 2026, scenario `threshold-delegate`, profile `cponly-seconds`)

Same stack as the delegate renewal (arkd master f863e48 + B only, `arkd:rgb-cponly`; isolated docker:dind). D1 coordinates (the `delegated` daemon); D2 and D3 are `cosignd` co-signers on localhost. Design and code citations: [threshold-delegate.md](threshold-delegate.md).

Run 1 `threshold-delegate 14` (run-1793634510, 16:48–17:24 CET): **PASS**.

- Olivia: 4 renewals over 14.1 days (subsets D1+D2, D1+D2, then D1+D3 after D2 went offline). Proof 5386 → 13093 B (+2569 B per renewal, against +1825 B for the single delegate; gz 2854 → 5283), consignment 5585 → 7169 B (unchanged growth). 0.65–0.94 s per renewal spent waiting for co-signers (4–6 signing requests). Wall time 4.4–14.3 s. Validate 217–252 ms.
- Per co-signer request (D2 and D3 logs): intent 90 ms on average (max 108), forfeit 119 (151), ark tx 227 (245), checkpoint 255 (341).
- Rita: D2 and D3 offline after renewal 1. 25 refusals ("only 1 of 3 delegates online"). Tower A exited 148 s before expiry. Rita validated 1000 `Unrolled`, the guard refused re-board, and Gina settled 1000.
- Olivia's return: discovery 49 addresses, policy 5 witnesses / 4 delegate spends / 0 violations, Bob received 1000 (proof 15278 B, 12 PSBTs, validate 244 ms). The scripted return hit a transient Electrum resolver error after the policy check; a manual rerun of the same command passed. A validate retry was added.
- Negatives: N3t-a (one delegate signs alone) is rejected at submit with `INVALID_SIGNATURE`, and the VTXO is not locked (the owner's spend afterwards is accepted). N3t-b: the honest D2 refuses ("ark tx output 0 pays a foreign script"). N3c: two colluding delegates are **accepted** by arkd, Mallory gets `Ok(1000)`, and the policy check gives 1 violation. N4: the guard refuses.

Run 2 `threshold-delegate 7` (run-1793637146, 17:32–18:08 CET): **PASS**, with no manual step. Olivia again had 4 renewals over 14.1 days (she returns after Rita), with the same proof sizes as run 1. Waiting for co-signers took 661–1063 ms per renewal, wall time 2.8–14.6 s. Rita's towers exited and Gina settled 1000. Bob received 1000. New negative N3t-c: M_D asks D2 to co-sign a hop that returns the bitcoin to Nora's spare but seals the asset on Mallory's VTXO. **D2 refuses on the seal** ("not a colored template of this owner and delegate set", 280 ms). N3t-a/b, N3c and N4 gave the same results as in run 1.

Regressions after the threshold-delegate work (same driver and binary, 2 Nov 2026): `quick` on the fast profile: **16/16 PASS** (18:09–18:30 CET, 1278 s). `cponly` on the checkpoint-only profile: **3/3 PASS** (`checkpoint-renew`, `checkpoint-swap`, `checkpoint-offline`; 18:32–18:35 CET, 153 s).
