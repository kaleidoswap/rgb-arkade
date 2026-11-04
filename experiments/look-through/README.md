# Experiment: checkpoint look-through

An RGB-side alternative to changing arkd: stock arkd, one extra seal-closing rule in RGB. Not RGB consensus and not wired into this tree; kept for reference.

## The rule

An Arkade off-chain spend goes VTXO → checkpoint → ark tx. Stock arkd builds the checkpoint from a fixed template, so it cannot carry a commitment; the ark tx can. The patched rgb-consensus counts a seal as closed by a witness that does not spend it directly when the witness spends output 0 of a resolved tx shaped like an Arkade checkpoint: one input spending the seal, outputs `[P2TR, P2A anchor]`. Only output 0 counts, so the anyone-can-spend anchor cannot carry a competing commitment.

The commitment uses a tagged opret, `OP_RETURN <0x52 || commitment>` (one 33-byte push). Stock arkd takes it as a 0-sat data output, while the standard `OP_RETURN <32 bytes>` has the shape of its sub-dust outputs.

## Files

| File | Applies to | Change |
|---|---|---|
| `rgb-consensus.patch` | rgb-consensus 0.11.1-rc.11 | tagged opret form and the checkpoint look-through rule |
| `rgb-psbt-utils.patch` | rgb-psbt-utils 0.11.1-rc.11 | writes the tagged form (`RGB_OPRET_TAGGED=1`) |

The rgb-lib and demo wiring (checkpoint attestation, a PSBT proprietary key `rgb-ark`/1 naming the VTXO behind a checkpoint input, `stock-lane` steps 6a–6d) was written against an earlier version of this repository and is not included.

## Results (stock arkd v0.9.15, 29 October 2026)

| Check | Outcome |
|---|---|
| Swap in one Arkade tx, tagged opret at 0 sat | Accepted; the buyer checks before signing and validates offline (400 units) |
| Opret indexed as a VTXO | No |
| Renewal: one Arkade tx spends the colored VTXO through its stock checkpoint and assigns the asset to a VTXO from a batch 240 blocks later | Accepted |
| Old batch past its expiry | Swept by arkd; the asset is already on the fresher VTXO |
| Send from the fresher VTXO (second look-through hop) | Accepted |
| Offline validation of the whole history (jump-in, swap, renewal, send; 7 attested txs) | 400 units accepted |

## Limits

- Only validators with the patched rgb-consensus accept the rule; standard RGB wallets see these assets as burned.
- Until the ark tx is confirmed, the asset depends on the checkpoint output. If a checkpoint goes on-chain and its ark tx does not, the operator can claim output 0 after the timeout, and its claim would close the seal. The checkpoint commitment in [`patches/arkd-checkpoint.patch`](../../patches/arkd-checkpoint.patch) does not have this problem: the asset goes to VTXOs that already exist.
- The owner, or a delegate, must renew before expiry. A colored VTXO must never be settled in a batch: the stock forfeit carries no commitment, so the asset would be lost.
- Delegated renewal and watchtower exits were not tested with this rule.
