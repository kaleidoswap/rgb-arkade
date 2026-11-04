# arkd master lane: changes B, C and A on arkd master

`arkd:rgb-master` = arkd master f863e48 + [`patches/arkd-checkpoint.patch`](../patches/arkd-checkpoint.patch)
(B) + [`patches/arkd-master-ac.patch`](../patches/arkd-master-ac.patch) (C, A). Build: `make setup-master`,
stack: `make regtest-up-master` (profile `patches/arkade-regtest.master.env`, delays in blocks).

| change | what | files | size |
|---|---|---|---|
| B `checkpoint-opret` | optional 0-sat `OP_RETURN <32B>` at index 0 of the checkpoint tx (same as `arkd-checkpoint.patch`) | indexer.go, service.go, sweeper.go, offchain/tx.go, script/script.go | +86/−11 |
| C `forfeit-opret` | optional 0-sat `OP_RETURN <32B>` at index 0 of the forfeit tx: `VerifyForfeitTxs` rebuilds the forfeit with the same payload, so any other change still fails the txid check | tx-builder/covenantless/builder.go | +17 |
| A `opret-data-output` | at most one 0-sat `OP_RETURN <32B>` in the ark tx, accepted as a data output: not checked against the VTXO minimum, not decoded or indexed as a VTXO (still counts towards `maxOpReturnOutputs`) | script/script.go, service.go, utils.go, db/service.go | +25/−3 |

Total on master: 8 files, +128/−14. Not ported from the full patch set (`patches/arkd.patch`, v0.9.15):
`forfeit-cosign` (master #1110 already signs forfeits at collection time; `fraud` passes without it)
and `recovery-forfeit` (needed by `sweep-recovery`). Client side: `settle_inputs_with_forfeit_hook`
in the rust-sdk patch (batch session over caller-built inputs, e.g. a covenant VTXO spent through
its collab leaf).

## Results (isolated regtest stack, 3 Nov 2026, times CET)

Measured on a development build of this lane that also carried one further small arkd change not
included in these patches. A rerun on exactly `arkd-checkpoint.patch` + `arkd-master-ac.patch` is
pending.

Quick profile (driver 17:50–18:51):

| scenario | result | time |
|---|---|---|
| checkpoint-renew, swap-ni, checkpoint-swap, checkpoint-offline | PASS | 49 / 96 / 40 / 58 s |
| swap-emu:crash, swap-emu | PASS | 57 / 78 s |
| emu-escrow, emu-lending, emu-vault, emu-asset-swap, emu-exit, emu-renew | PASS | 174 / 175 / 483 / 23 / 40 / 56 s |
| emu-renew-batch | PASS | 116 s |
| basic | PASS | 79 s |
| offchain-transfer, offchain-transfer --offline-proof, refresh | PASS | 98 / 114 / 83 s |
| collab-exit, forfeit-negatives, multi-user, failed-batch, fraud | PASS | 62 / 143 / 54 / 61 / 36 s |
| multihop-exit, swap | PASS | 211 / 382 s |
| stock-lane, opret-probe | PASS | 26 / 21 s |
| emu-lots, basic-onchain-genesis | FAIL, not rerun | 249 / 438 s: a fresh boarding output refused by arkd as `INVALID_PSBT_INPUT: failed to validate boarding input: tx … expired` on every retry. Boarding validation is not touched by A, B or C; cause not investigated. Both passed on the previous build of the lane (385 / 90 s) |

On the quick profile a 0-sat ark-tx `OP_RETURN` is not returned by the indexer as a VTXO (checked
on the stored output), while the 1-sat form still is.

Seconds profile (time-scaled scenarios, 19:10–21:19):

| scenario | result | notes |
|---|---|---|
| sweep-recovery | FAIL (expected) | setup and sweep PASS; recovery refused (`INVALID_INTENT_PROOF: wrong tx hash in message input`): needs `recovery-forfeit` |
| long-offline | FAIL | Alice and Erin PASS; the control user's batch was not yet swept when he returned (the scenario expects it to be). Not investigated |
| exit-watchtower | FAIL | `no exit branch` for Alice's VTXO at setup. Not investigated |

## Covenant renewal through the batch (emu-renew-batch) [verified]

Loan V (batch 1; leaves fill {server, emulator}, collab {server, Bob, Lena}, exit CSV {Bob, Lena}).

1. 120 blocks later Bob boards fresh sats (batch 2).
2. Negative: a session holding only Bob's key registers the renewal intent: refused,
   `INVALID_INTENT_PROOF: missing signature for <Lena>`.
3. One session holding Bob's and Lena's keys joins the next batch with V (collab leaf) + Bob's
   batch-2 VTXO, output G' = {server, Bob, Lena} collab + CSV {Bob, Lena}. In the forfeit hook
   T_f (V -> G', explicit seal on the new tree leaf) is computed in 88 ms and its commitment
   inserted at forfeit index 0. arkd accepts: forfeit bbe130b8… outputs [0 sat OP_RETURN,
   2330 sat operator, anchor], listed in GetForfeitTxs; V spent_by the forfeit, settled_by the
   commitment. Session 14.3 s. G' expires later than V.
4. Bob validates 1000 units on G' (consignment 6656 B, 154 ms; the forfeit is an unmined,
   operator-attested witness).
5. Hop G' -> V' (fresh covenant, tree recomputed 593 ms; submit+finalize 15 ms).
6. Batch 1 expires: V (forfeited) swept; V' untouched.
7. On V': underpaid repay refused (OP_VERIFY); repay through the emulator 79 ms, 1000 units back
   to Bob; full-history consignment 9735 B, 8 bundles, validated in 192 ms.

Who must be online: Bob and Lena (intent proof + forfeit signatures on the collab leaf, 2 + 2
Schnorr signatures, plus the operator's), during one batch; then both again for the G' -> V' hop.

What changed vs the cponly findings:
- The forfeit path no longer loses the asset: the batch renewal carries the RGB transition
  (before: INVALID_FORFEIT_TXS, the asset was lost on any intent renewal).
- The covenant's own sats are renewed by the batch (no external newer-batch VTXO G needed as in
  the two offchain hops of emu-renew B).
- Still two steps for a covenant: V -> V' directly in one batch is circular [inferred, by
  construction]: V''s address commits to the contract transitions, which commit to T_f's opid,
  which commits to the explicit seal on V''s tree outpoint, known only after the tree is built.
- Renewal without live parties: still not possible [inferred + code]. The emulator signs batch
  forfeits only with exactly 2 outputs (`validateForfeitOutputs` in the emulator), so an
  emulator-signed forfeit with the RGB commitment is refused; a presigned (delegate) forfeit
  cannot carry T_f because the new seal is unknown when presigning.
