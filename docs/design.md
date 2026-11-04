# Design

How RGB assets live in Arkade VTXOs: where the seals sit, which transaction carries each commitment, how receivers verify without the operator, and how assets survive batch expiry. Approaches compared: [approaches.md](approaches.md). Measurements: [test-results.md](test-results.md). Code changes: [../PATCHES.md](../PATCHES.md).

## Background

**Arkade.** A VTXO is a leaf of a tree of pre-signed transactions anchored by an on-chain commitment (batch) tx. An off-chain spend is a checkpoint tx (one input, the VTXO; outputs: checkpoint output and a P2A anchor) followed by an ark tx that spends the checkpoint output; arkd rebuilds both and compares txids. In a batch (refresh, collaborative exit) the user gives up the old VTXO with a forfeit tx, which the operator publishes only if the old VTXO is unrolled. Unilateral exit publishes the branch from the commitment tx down to the VTXO. After the tree expiry the operator sweeps the batch.

**RGB.** State is assigned to an outpoint (a single-use seal); the tx that spends the outpoint must commit to the transition that moves the state. With opret the commitment is the first `OP_RETURN` output, exactly `OP_RETURN <32 bytes>`. Receivers validate a consignment (genesis plus transition history) client-side and resolve each witness tx; rgb-consensus accepts tentative (unmined) witnesses and rejects archived ones.

Taproot outputs in Ark (VTXOs, checkpoints, tree) have keys and scripts fixed by the protocol and checked by the operator, so commitments here are opret only.

## Which transaction closes the seal

| Spend of a colored VTXO | Tx | Commitment | arkd change |
|---|---|---|---|
| Off-chain send, renewal, swap | checkpoint | `OP_RETURN` at vout 0, checkpoint output moves to vout 1 | `checkpoint-opret` (B) |
| Asset onto a new output of the same Arkade tx | ark tx | 0-sat `OP_RETURN` at vout 0 | `opret-data-output` (A) |
| Refresh, collaborative exit, sweep recovery | forfeit | 0-sat `OP_RETURN` at vout 0 | `forfeit-opret` (C) |
| Unilateral exit, then on-chain spend | the user's spend of the exited output | `OP_RETURN` at vout 0 | none |

The `OP_RETURN` must be the first output: rgb-consensus opret requires it to be the first DBC-capable output, and a P2TR output before it is rejected.

### Entering Arkade (jump-in)

The asset is issued on-chain with stock rgb-lib and moved by an ordinary on-chain RGB transfer to an explicit seal on the user's VTXO, which the user got by boarding plain bitcoin. Boarding the colored UTXO directly is not possible: the commitment tx cannot carry a user `OP_RETURN` (its on-chain outputs are accepted only as addresses), and the VTXO txid is unknown when the intent is signed.

On stock arkd the seal is instead the *predicted* checkpoint output of the user's next spend: the checkpoint txid depends only on the VTXO and the operator's checkpoint script, so it can be computed in advance. That allows exactly one hop, with the commitment in the ark tx.

### Off-chain transfer (checkpoint only)

The checkpoint of the sender's VTXO carries one transition that assigns the asset to VTXOs that already exist: the receiver's VTXO, and a spare VTXO of the sender for any change. The seals are explicit (known txids), so the commitment does not depend on the checkpoint txid and the ark tx carries nothing. Renewal is the same transfer onto the owner's own VTXO from a later batch.

### Off-chain transfer (checkpoint and ark tx)

With A as well, the checkpoint moves the asset to its own output (T1) and the ark tx moves it on to new outputs for the receiver and the change (T2). T1 uses a witness-vout seal, so it is computed before the checkpoint txid. When such a VTXO exits, its checkpoint and ark tx go on-chain with it.

### Refresh and collaborative exit (forfeit)

The forfeit is the only tx that spends the VTXO on these paths. When the user signs it, the commitment tx, the tree and the connectors are known, so the new seal (the new VTXO, or the on-chain exit output) is known too. arkd's deterministic rebuild of the forfeit includes the optional `OP_RETURN`, and the user's signature covers it, so the operator cannot strip it.

In the honest case a forfeit is never mined, so for RGB it stays a tentative witness forever. Validators accept it only after Ark-level attestation (commitment tx mined, forfeit and connector registered for it), and rgb-lib's safe-height wait uses the depth of that commitment tx.

## Offline verification: Ark proofs

The sender attaches an Ark proof (`<consignment>.arkproof.json`) with every unmined Ark tx the history needs (checkpoints, ark txs, forfeits, connectors, tree branches) down to a mined commitment tx. The receiver checks it against the chain only:

- every tx spends the previous output, down to a mined commitment tx;
- tree signatures verify against the output key; checkpoint, ark tx and forfeit signatures against the operator key pinned in the wallet;
- every RGB witness has an input co-signed by a pinned operator key (key-path signatures alone do not count);
- every RGB witness has exactly one `OP_RETURN <32 bytes>`, at vout 0, below dust: 0 sat on an arkd that takes it as data (`opret-data-output`, and every checkpoint or forfeit commitment), 1–329 sat on an operator that stores it as a sub-dust VTXO (stock lane).

It is a separate file because the consignment format and id are fixed. Cost: about 2.5 kB per unmined off-chain hop (0.94 kB in the binary form) and 1.8 kB per unmined forfeit. A hop drops out of the proof only once its tx is mined, which happens only when a VTXO whose branch contains it is unrolled. Txs that are never mined stay in every later proof of the asset, even after it goes on-chain: forfeits of refreshes and collaborative exits (C), and, in the checkpoint-only lane (B), the checkpoints of every transfer and renewal, because each moves the asset onto a VTXO of another tree, whose exit does not publish them. So in B the proof grows by about one hop per transfer or renewal for the whole life of the asset (for example about 2.5 kB a month for a monthly renewal, plus the transfers); no pruning is possible without trusting someone else's verification (receiver cache, operator-signed checkpoints; not implemented). Expiry: for each VTXO the proof lists, the receiver reads the sweep timelock of the batch output its branch hangs from and accepts it only if it reproduces the batch output key (MuSig2 of the tree cosigners, tweaked by the sweep leaf of the pinned operator key), so a sender cannot claim a longer life. With a minimum set (`import_ark_proof_with_min_lifetime`; in the demo `RGB_ARKADE_MIN_LIFETIME_BLOCKS` / `RGB_ARKADE_MIN_LIFETIME_SECS`, off by default because the scenarios use expiries of minutes), a VTXO with less time left before the sweep is rejected. Not verified: connectors beyond the link and the MuSig signers of tree nodes.

## Expiry

On bitcoin-only Arkade an expired VTXO is recovered in a new batch. An RGB asset is tied to its outpoint: the sweep spends the tree without a commitment, so the seal can never close and recovering the bitcoin does not bring back the asset. What keeps an asset alive:

| Mechanism | Needs | Covers |
|---|---|---|
| Renewal: off-chain move onto a VTXO of a later batch | B | Any owner who comes back before expiry |
| Pre-signed renewal hop submitted by a keyless delegate | B | About one extra period: the destination must exist when the owner signs |
| Keyless exit watchtower: broadcasts the pre-signed branch before expiry | nothing | Any absence, at on-chain fee cost (about 550–650 sat per level at 2 sat/vB) |
| Refresh in a batch | C | Owner online, no spare VTXO needed |
| Sweep recovery: a forfeit with the commitment moves the swept asset to a new VTXO | C + `recovery-forfeit` | Any absence, if the operator cooperates on return |

A delegated batch refresh for colored VTXOs is not possible with these changes: the forfeit must commit to the new VTXO, which exists only once the batch is built, and the owner's signature covers that commitment.

## Batches and rollback

A batch can mix users, assets and bitcoin-only VTXOs; each owner colors their own forfeit and each receiver validates its own consignment and proof. If a round fails (a participant does not sign), the colored forfeit is never co-signed and its commitment tx never published; `ark_rollback_transfer` archives the transition and the refresh is retried. rgb-lib refuses to roll back a witness that is mined, in the mempool or attested. It keeps a small registry of tracked VTXOs and derives each one's status from the chain: active, to recover (swept), unrolled or spent.

## Atomic swaps

Asset for bitcoin in one Arkade tx with inputs from two owners (scenarios `swap`, `checkpoint-swap`, `stock-lane`). The seller builds the joint tx and the transition; the buyer validates the consignment over the still-unsigned txs, then signs its ark tx input and its checkpoint up front. The seller submits and finalizes alone. Either both legs happen or neither does; if the buyer walks away or the check fails, the seller rolls the transition back.

Not implemented: asset-for-asset in one tx (two transitions in one commitment), the same-batch variant, and HTLC swaps towards on-chain and RGB Lightning.

## Security model

- The operator alone cannot move an asset: every transition needs the owner's signature on a tx that commits to it.
- Unilateral exit is always possible before expiry; after expiry the operator can sweep the batch.
- Preconfirmed transfers need the same trust in the operator as bitcoin on Arkade.
- With unmined forfeits (C), seal uniqueness depends on the operator. If operator and owner collude they can co-sign two forfeits of one VTXO and two offline receivers both accept. This is detected (a public fraud proof with two operator signatures; an online receiver sees it through `spent_by`), not prevented.
- The operator can refuse a refresh: the owner can still renew off-chain (B) or exit unilaterally before expiry.
- The operator can refuse a sweep recovery. The swept branch can no longer be published, so the asset's seal can never be closed again: the asset is lost for good, although no valid transfer of it exists. Recovery after a sweep therefore depends on the operator cooperating.
- Surviving a long absence (for example 2 months) without the operator: only a keyless watchtower that exits the colored VTXO before expiry, which must be running and funded for the on-chain fees. In the checkpoint-only lane (B) this is the only protection beyond about one extra batch period (a pre-signed delegated renewal); in the full patch set the alternative is a sweep recovery, with the condition above.
- Only a 32-byte commitment is visible to the operator and on-chain, but opret makes RGB transactions recognizable, so selective censorship is possible.
- Exit cost grows with the number of unmined hops in the branch.

## References

- [arkd @ f863e48: commitment tx and forfeit verification (builder.go)](https://github.com/arkade-os/arkd/blob/f863e484719344edbe4a8d10cf5fe994b123f2c0/internal/infrastructure/tx-builder/covenantless/builder.go)
- [arkd @ f863e48: on-chain outputs and intent leaves (utils.go)](https://github.com/arkade-os/arkd/blob/f863e484719344edbe4a8d10cf5fe994b123f2c0/internal/infrastructure/tx-builder/covenantless/utils.go)
- [arkd @ f863e48: forfeit tx (forfeit_tx.go)](https://github.com/arkade-os/arkd/blob/f863e484719344edbe4a8d10cf5fe994b123f2c0/pkg/ark-lib/tree/forfeit_tx.go)
- [rust-sdk @ f184a5f: send.rs (checkpoint and ark tx)](https://github.com/arkade-os/rust-sdk/blob/f184a5f1aa77924b7fd40eede7cf2fd43854ae76/ark-core/src/send.rs)
- [rgb-lib (KaleidoSwap fork) @ 8852b2e](https://github.com/kaleidoswap/rgb-lib/tree/8852b2e55606ebd568f56bdfd9fa9216fbec3435)
- [rgb-consensus 0.11.1-rc.11: opret verification (spk.rs)](https://docs.rs/crate/rgb-consensus/0.11.1-rc.11/source/src/dbc/opret/spk.rs)
