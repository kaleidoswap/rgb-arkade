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

Cost: about 2.5 kB per off-chain hop and about 1.8 kB per unmined forfeit (6–7 kB consignment). When the hops are mined through an exit, the proof empties (39 bytes); an unmined forfeit, instead, stays forever in subsequent proofs.

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
| Official run: Alice offline 5 570 s, about 64 days (over 2 months). She returns at 09:15 CET, the wallet flags the asset as LOST, recovers it (commitment `be09965e…`, forfeit `ad0e16d0…`, new VTXO `d6d3d604…:0`), then exits on-chain and pays Dave (`16afc92b…`) | Dave `settled: 1000`, verified with arkd paused |
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
| gzip of the consignment | 21.7 kB | 11.1 kB (−49%) |
| Receiver cache: only the transactions it has not already verified are sent | 15.2 kB cumulative | about 1 kB per new hop |
| Already-mined transactions removed from the proof (already in the offline-proof work) | – | 39 bytes if everything is mined |

Not prototyped: history checkpoints signed by the operator (constant-size proof, but its signature must be trusted), periodic on-chain compaction (resets the Ark proof but not the RGB history) and batching.

## Extrapolation for a USDt-like asset (estimate, not measured)

| Transitions in the payment history | Consignment | Binary proof (10 unmined hops) | Cold verification |
|---|---|---|---|
| 10 | 15 kB | 9 kB | 0.3 s |
| 100 | 61 kB | 9 kB | 0.8 s |
| 1 000 | 0.5 MB | 9 kB | 6 s |
| 10 000 | 5 MB | 9 kB | about 1 minute |

For an asset that circulates a lot, with many small payments and payments merging coins from many senders, the history of each coin ends up including almost every transfer ever made. The Ark part stays bounded (about 1 kB per unmined hop, reset when going on-chain). The real limit is the RGB history, which always grows. The practical path is a KaleidoSwap hub that keeps the already-verified history, with differential consignments and a validation cache: each payment carries only the new part and latency stays under a second. It needs development in rgb-lib, but no consensus change.

## Regression history

The full suite was rerun after every arkd/rgb-lib change set (sequentially; the electrum indexer flakes with "Invalid indexer" when scenarios run in parallel). Last full run, 26 Oct 2026 10:36–12:25 CET: basic, basic-onchain-genesis, offchain-transfer, offchain-transfer `--offline-proof`, refresh, collab-exit, forfeit-negatives, sweep-recovery and long-offline all PASS. Two harness timing issues were found and fixed in the scripts (not in the code): a scenario that changes `vtxoTreeExpiry` must wait for rounds started under the old value (45 s), and the sweep-recovery return must wait until arkd reports the VTXO swept.

### Cleanup branch verification

Rerun on 2026-10-26 (CET) with the renamed CLI, against the live regtest stack (`arkd:rgb`, v0.9.15-rgb4):

| Command | Result | Key check |
|---|---|---|
| `ark-rgb-demo scenario offchain-transfer` | PASS (13:31–13:41) | Dave `settled: 700` after 2 hops, unilateral exit and on-chain send |
| `ark-rgb-demo scenario collab-exit --offline-proof` | PASS (13:41) | Dave `settled: 1000`; validation with arkd paused; tampered proofs rejected |
| `make scenario SCENARIO=forfeit-negatives` (via `scripts/run-scenario.sh`) | PASS (13:45) | All negative cases rejected |
| `make patches-check` | OK | All 4 patches apply on their pinned commits |

Not rerun after the rename: `basic`, `basic-onchain-genesis`, `refresh`, `multi-user`, `failed-batch`, `fraud`, and the orchestrated `sweep-recovery`, `long-offline` and `exit-watchtower`, plus `bench.sh`. Their results above come from the same code before the rename.

### Consolidation (26 Oct 2026, CET)

- **multihop-exit** (new regression: board, issue, send 300, send 100 back, collaborative exit of the 700 change, unilateral exit of the 100, on-chain send). Before the exact-input fix: FAIL (16:52–17:01, "spend does not include the RGB output"; the forfeited change output of the same Arkade tx matured first and was picked alone). After: PASS (17:01–17:11 and in run-all, 19:35–19:45).
- **App persistence** (API-driven, 17:12–17:25): three app restarts mid-flow reloaded identical state (holdings, unrolled exit, exited BTC); on-chain sends both ways, BTC sweep and reset OK; Ark `unrolled` back to 0 after the exit spend.
- **Fresh stack** (after a host reboot lost Docker): `scripts/setup.sh` rebuilt `arkd:rgb`; `scripts/app.sh` brought regtest up from a stopped stack (18:39–18:41) and the app boarded a wallet at 18:42.
- **run-all quick** (18:42–19:45): basic, basic-onchain-genesis, offchain-transfer, offchain-transfer `--offline-proof`, collab-exit, forfeit-negatives, multi-user, failed-batch, fraud, multihop-exit PASS; refresh FAIL (19:30), FAIL again alone (19:56): the receiver validated right after the spend was mined and Fulcrum had not indexed it yet. With a retry on the resolver's network error (as the app already does): PASS (19:58–20:08).
- **Operator forfeit broadcast** (arkd `forfeit-cosign` fix, image `v0.9.15-rgb`). multihop-exit now also requires the forfeited change, unrolled with the exit branch, to be spent on-chain by its stored colored forfeit. On the old image it FAILs (20:13–20:23): arkd logs "failed to react to fraud … failed to finalize forfeit tx: Invalid Psbt due to duplicate key" and the output stays unspent. On the fixed image it PASSes (20:28–20:37), with the forfeit `2d1d745c…` confirmed. After the fix these PASS one at a time: fraud (20:41), forfeit-negatives (20:43–20:45), refresh (20:45–20:55) and sweep-recovery with its sweep check (20:55–21:10).

### Operator co-signature on every witness (28 Oct 2026, CET)

`import_ark_proof` accepted a witness whose inputs were all key-path signed: an on-chain P2TR UTXO spent by a never-mined tx with a well-formed opret, signed by its owner alone (who could sign any number of conflicting ones). Every witness now needs an input whose satisfied leaf contains a pinned operator key. rgb-lib unit test `key_path_only_witness_is_rejected` fails before the fix and passes after; `offchain-transfer --offline-proof` PASS (10:31–10:41), with the new `key_path_witness` case rejected ("no input co-signed by the operator") and every other tampered proof still rejected.

### Fast regtest profile (28 Oct 2026, CET, macOS host)

`make regtest-up-fast` (every arkd delay in blocks: exits 10, boarding 20, expiry 500), then `scripts/run-all.sh quick`: basic, basic-onchain-genesis, offchain-transfer, offchain-transfer `--offline-proof`, refresh, collab-exit, forfeit-negatives, multi-user, failed-batch, fraud and multihop-exit PASS in about 16 min (12:44–13:00, fraud rerun after rebuilding `tools/arkdb` for linux), against about 1 h 30 on the seconds profile. Unilateral exits mature in about 100 s instead of 9 min: `basic` 1 min 32 s (9 min 23 s), `multihop-exit` 1 min 49 s (about 10 min).

### Spending an asset received on-chain (28 Oct 2026, CET)

An output that delivers an RGB asset on-chain carries only the payment's sats (2 000), so spending it again panicked on the change; the spend now adds a vanilla UTXO for the fee. `collab-exit` gained a hop: Dave spends what he received to Erin, a wallet that never saw the history (whose collaborative-exit forfeit is never mined). PASS online (Dave → Erin `810c42b5…`, Erin `settled: 1000`) and with `--offline-proof` (`eba63ad4…`; Erin registers the forfeit from the proof Dave forwards, then `settled: 1000`). In the app, a 700-unit holding received on-chain went Bob → Alice → Bob on-chain (`628772eb…`, `9be67bc6…`).

### Stock balance after Arkade spends (28 Oct 2026, CET)

rgb-lib marks a wallet UTXO spent only when it broadcasts the spending tx, so after a jump-in or a spend built outside it the stock balance kept the moved allocations (Alice showed `spendable: 1000` of an asset she had sent away). The custom coloring paths now mark their wallet inputs spent and `ark_rollback_transfer` unmarks them. `collab-exit` (online and `--offline-proof`) checks it: Alice and Dave end at `settled: 0, spendable: 0`, Erin at 1000. Quick suite on the fast profile, run alone: 11/11 PASS (15:33–15:48), failed-batch rollback included.

### arkd master with three changes (28 Oct 2026, CET)

arkd master `f863e48` (v0.9.16 + the btcd v2 migration) with only three changes: data `OP_RETURN` in the checkpoint (with the checkpoint output at vout 1), the same in the forfeit, and a 0-sat `OP_RETURN <32 bytes>` taken as data instead of a sub-dust VTXO. `forfeit-cosign` is dropped (master #1110 signs forfeits at collection time), and so are `subdust-restart-fix` and `recovery-forfeit`. Image `arkd:rgb-master` on `arkd:v0.9.16`, `arkd-wallet:v0.9.16`, fast profile: the quick suite passes, 11 of 11 (`forfeit-negatives` after making its positive control use a fresh wallet: master bans a VTXO after repeated invalid forfeits). `multihop-exit` passes, so the operator broadcasts its stored forfeit without our co-sign patch. Not run on master: `sweep-recovery` (needs `recovery-forfeit`) and the other time-scaled scenarios.

Found while rebasing, fixed in both patches: on finalizing an off-chain tx arkd watched `TxOut[0]` of each checkpoint, which is the data `OP_RETURN` when present; the wallet rejected the script, so that checkpoint went unwatched until the next restart.

### Atomic swap in one Arkade tx (29 Oct 2026, CET, Linux host, fast profile)

`swap`: Alice holds 1000 units on a VTXO (100,000 sat), Bob a bitcoin-only VTXO (100,000 sat). One Arkade tx spends both: checkpoint(Alice) carries T1, checkpoint(Bob) nothing, the ark tx carries T2 and pays [OP_RETURN, Bob 1,000 sat + 400 units, Alice 129,000 sat + 600 units, Bob 70,000 sat]. arkd accepts an off-chain tx whose inputs belong to two owners.

| Case | Outcome |
|---|---|
| Bob walks away after Alice prepared T1/T2 | Nothing submitted; Alice rolls T2 and T1 back and holds 1000 on her VTXO again |
| Alice's T2 gives Bob 300 instead of 400 | Bob's check before signing (`validate_consignment_offchain` over the unsigned txs) refuses; Alice rolls back |
| Agreed swap | Bob signs his ark tx input and his checkpoint before the submit; Alice submits, adds Bob's signature to the server-signed checkpoint and finalizes alone (arkd accepts the pre-signed checkpoint). Bob validates with arkd paused: `Fungible(400)`; Alice: active 600, new VTXO 129,000 sat |

PASS in about 30 s (15:16:21–15:16:52).

### Stock arkd lane (29 Oct 2026, CET, Linux host, `REGTEST_PROFILE=stock`: unpatched `arkd v0.9.15`, vtxoMinAmount 1, dust 330)

On stock arkd an RGB opret (`OP_RETURN <32 bytes>`, the only shape RGB accepts) is the sub-dust VTXO format: it must be worth at least vtxoMinAmount and less than dust (code read on v0.9.15 and master f863e48), and checkpoints carry nothing.

| Check | Outcome |
|---|---|
| `basic` (jump-in onto the predicted checkpoint, one hop with a 1-sat opret, unilateral exit, on-chain send) | PASS, Carol `settled: 1000` |
| `stock-lane` swap: Alice's freshly jumped-in asset for Bob's bitcoin in one Arkade tx, commitment only in the ark tx (1-sat opret) | Accepted by stock arkd; Bob checks before signing, signs up front, validates with arkd paused: `Fungible(400)`; Alice's new VTXO 128,999 sat |
| `swap` (patched lane: 0-sat opret, commitment in the checkpoint) on stock arkd | Rejected: `AMOUNT_TOO_LOW: output #0 amount is lower than min vtxo amount` |
| Second hop: Bob's checkpoint with a commitment | Rejected: `CHECKPOINT_MISMATCH` |
| Is the opret output indexed as a VTXO? | Yes: a 1-sat VTXO (marked swept) |
| arkd restart after these transfers | arkd does not come back: `failed to restore watching vtxos: ... failed to add addresses to group: HTTP 400 invalid-address`, then `server not ready` on every call. Not yet isolated to one opret (the `basic` run left one on the same stack). Report privately upstream before any public mention. |

So on a stock operator an RGB asset can enter Arkade (jump-in) and move once, including as one leg of an atomic swap, then has to leave by unilateral exit before its batch expires; and storing the opret as a VTXO leaves the operator unable to restart.

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
