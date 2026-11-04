# Tapret vs opret on Arkade

Would RGB **tapret** commitments, instead of the **opret** ones this project uses, need fewer arkd changes? Short answer: no. Tapret removes none of the arkd patches and needs larger ones. It also puts an `OP_SUCCESS` leaf on Ark outputs: once the tx is on-chain, anyone holding the proof can spend that output without a signature or a wait. Opret stays the choice. Current design: [design.md](design.md); patches: [../PATCHES.md](../PATCHES.md).

References: arkd `v0.9.15` (`f444de6`, unpatched) and rgb-consensus `0.11.1-rc.11`. Checks: [`tools/tapret-probe`](../tools/tapret-probe).

## What tapret requires

- **Host output.** The validator takes the *first* output that is `OP_RETURN` or P2TR. If it is P2TR, the anchor must be tapret (`validation/validator.rs` 463–484; `dbc/tapret/tx.rs` `convolve_commit`).
- **Tweak.** The host key becomes `taptweak(internal_pk, TapBranch(commitment_leaf, original_root))`, with the commitment leaf at depth 1. A 1-byte nonce is ground so that the leaf sorts first (`dbc/tapret/xonlypk.rs` `convolve_commit`, `TapretNodePartner::check_ordering`).
- **Leaf.** The leaf is `29 × OP_RESERVED ‖ OP_RETURN ‖ PUSH33 ‖ mpc ‖ nonce`, with tapscript leaf version (`dbc/tapret/tapscript.rs` `commit`). In tapscript, `0x50` is `OP_SUCCESS80`.
- **Proof.** The proof carries `internal_pk` plus the depth-1 partner: a node hash, a leaf script, or two child hashes (`dbc/tapret/mod.rs` `TapretProof`). That is about 65–100 bytes per anchor. An opret proof is empty.
- **One per tx.** Each tx holds one commitment (an MPC over contracts). This is the same for opret and tapret.

In Ark, every taproot output that arkd or a client builds uses the NUMS internal key `0250929b…` (`script.UnspendableKey`). Its tree comes from a list of closures (`TapscriptsVtxoScript.TapTree`, `AssembleTaprootScriptTree`). arkd verifies that tree:

- **Off-chain spend.** `SubmitOffchainTx` (`service.go` 443) decodes every revealed leaf as a closure with `ParseVtxoScript`, then compares the derived key with the stored VTXO key at line 757. `DecodeClosure` rejects any script that contains `OP_SUCCESS` (`script/closure.go` 36).
- **Batch inputs.** `validateVtxoInput` (4178) runs the same checks on intent inputs.
- **Boarding.** The same check runs on boarding inputs (4065).
- **Checkpoint sweep.** The sweeper rebuilds the checkpoint control block from the template (`sweeper.go` 253–290).

## Per transaction

| Tx (role) | Who builds and signs it | Where a tapret would go | What stock arkd does with it | Tapret | Opret today |
|---|---|---|---|---|---|
| **Checkpoint** (off-chain send, renewal, swap: closes the VTXO seal) | The client builds it and arkd rebuilds it from the template (`offchain.buildCheckpointTx`). The user signs, the operator co-signs | vout 0, the checkpoint output: P2TR(NUMS, {operator CSV unroll, user collaborative}) | Rebuild, then txid compare. A tweak gives `CHECKPOINT_MISMATCH` (`service.go` 996–1005). The sweeper's CSV control block would be wrong | **Patch, larger than B.** The template needs an optional commitment-leaf hash, stored with the tx. The sweeper and clients need control blocks that include it. The checkpoint output then has an `OP_SUCCESS` leaf (see below) | `checkpoint-opret` (B): `OP_RETURN` at vout 0, checkpoint output moves to vout 1 |
| **Ark tx** (new output in the same tx; on stock arkd, the one hop that closes a predicted checkpoint seal) | The client builds it and arkd rebuilds it. Outputs are chosen by the user | First P2TR output | `validateOffchainTxOutputs` (4545) accepts any P2TR at or above dust, so creating the output works. A tweaked **VTXO** can never be spent through arkd: revealing the leaf fails `DecodeClosure`, and hiding it fails the key check at 757. The only way out is a unilateral exit | **No change if vout 0 is a sacrificial P2TR of at least dust (330 sat)** that is never spent and gets swept at expiry. A **large patch** is needed if live VTXOs carry the tweak: VTXO script validation must accept an opaque depth-1 sibling in `SubmitOffchainTx`, `validateVtxoInput` and boarding, plus client exit and spend paths | A: 0-sat data `OP_RETURN` (`opret-data-output`). Stock: sub-dust `OP_RETURN`, 1–329 sat |
| **Forfeit** (refresh, collaborative exit) | The user builds and signs it. arkd rebuilds it in `VerifyForfeitTxs` (`builder.go` 281, rebuild at 480) | None. The outputs are P2WPKH to the operator's forfeit key (`getForfeitScript`, 1211) and P2A, so there is no taproot output (rgb-consensus: `NoDbcOutput`) | Rebuild and txid compare | **Patch, larger than C.** Either the template gets an extra P2TR of at least 330 sat, taken from the operator's forfeit value, or the operator's forfeit output becomes a tweaked P2TR that its wallet must be able to spend | `forfeit-opret` (C): 0-sat `OP_RETURN` at vout 0 |
| **Recovery forfeit** (after a sweep) | Same as the forfeit | Same as the forfeit | Same as the forfeit | **Patch:** the forfeit patch plus `recovery-forfeit`, unchanged | C plus `recovery-forfeit` |
| **Tree and exit txs** (unrolling) | arkd builds them; the MuSig2 cosigners sign them | Not applicable: no seal closes in these txs. Their outputs are tree nodes or VTXOs | n/a | **No change** | No change |
| **User's on-chain spend after an exit**, jump-in | The user builds and signs it | Any P2TR the user owns | Not involved | **No change.** If the exited VTXO itself is tweaked, the client needs the tapret sibling in its control blocks | No change |
| **Commitment (batch) tx** (boarding a colored UTXO) | arkd builds it. Tree cosigners sign the batch output | vout 0 is the batch output (MuSig2 plus sweep leaf); vout 1 is the connector (`createCommitmentTx`) | A user tweak would change the tree root that the cosigners sign | **Infeasible** | Infeasible too (no user `OP_RETURN`) |

## Effect on the current arkd patches

| Patch | With tapret |
|---|---|
| `checkpoint-opret` (B) | Replaced by a larger patch: the template plus the control blocks for the sweeper and clients. The checkpoint output stays at vout 0, so B's index changes in the sweeper and indexer go away |
| `opret-data-output` (A) | Avoidable only with a sacrificial P2TR of at least 330 sat at vout 0 on every hop, instead of 0 sat. Tweaking a live VTXO needs a new, more invasive patch to arkd's VTXO script validation |
| `forfeit-opret` (C) | Replaced by a larger patch: the forfeit tx has no taproot output to tweak |
| `forfeit-cosign`, `recovery-forfeit` | Not about the commitment type, so still needed |

## Opret vs tapret

| | Opret (current) | Tapret |
|---|---|---|
| arkd changes | B: 1 opt-in. A+B+C: 3, plus 3 that are not about commitments | None removed. B and C become larger, and live tweaked VTXOs need a new patch to script validation |
| Stock arkd | One hop: sub-dust `OP_RETURN` (1–329 sat), stored as a sub-dust VTXO | One hop: sacrificial P2TR (≥ 330 sat), stored as a VTXO that cannot be spent and is swept at expiry |
| Proof size | 0 bytes per anchor | About 65–100 bytes per anchor (internal key, partner, nonce), small next to the 2.5 kB of Ark proof per hop |
| Our validation code | `ark_proof.rs` `opret_layout_ok`: one `OP_RETURN<32>` at vout 0 | Needs a rewrite: vout 0 must be P2TR and match the anchor's `TapretProof`. rgb-lib colors only with `OpretFirst` (`online.rs` 2738, `rust_only.rs` 262/575/675) |
| On-chain cost when published (unroll, fraud) | +43 vB `OP_RETURN` output, 0 sat | 0 extra vB when tweaking an existing output (the checkpoint). An extra output (forfeit, sacrificial) costs +43 vB and at least 330 sat |
| Privacy | The commitment is visible once the tx is on-chain. The operator sees it | Hidden on-chain. The operator must be given the leaf hash wherever it rebuilds or sweeps |
| Expiry, sweep, watchtower | Not affected by the commitment type | Same, except that a tweaked VTXO cannot be renewed or refreshed on stock arkd and can only exit |
| Spend safety | `OP_RETURN` outputs cannot be spent | **The commitment leaf is `OP_SUCCESS80`.** Anyone holding the proof can spend the host by script path with no signature and no CSV wait, once the tx is on-chain. This is consensus-valid and blocked only by relay policy, so it needs a cooperating miner. In Ark this races the user's CSV exit and the operator's sweep or penalty. Every later receiver of the asset gets the proof |

## Verified vs reasoned

Verified in [`tools/tapret-probe`](../tools/tapret-probe) with `go test -v` against `vendor/arkd/pkg/ark-lib`. The runs used unpatched arkd v0.9.15 and the patched vendor tree; both pass:

1. A default VTXO (CSV exit plus user and operator multisig, NUMS internal key) with a tapret tweak:
   - with the leaf revealed, `ParseVtxoScript` fails (`OP_SUCCESS`);
   - with it hidden, the key arkd derives (`505fcc…`) does not match the VTXO key (`0cde40…`).
2. A tapret on the checkpoint output changes the txid that arkd rebuilds.
3. The forfeit built by `tree.BuildForfeitTx` with arkd's P2WPKH forfeit script has no P2TR output.
4. A script-path spend through the tapret leaf with no signature passes btcd's script engine under consensus flags and fails under `StandardVerifyFlags` (`OP_SUCCESS`).
5. arkd's `AssembleTaprootScriptTree` puts a third leaf at depth 1 for a 2-closure VTXO, which is the tapret shape. With 3 or more closures it does not, so a patch needs an explicit sibling.
6. Cross-check: rgb-consensus `TapretProof::verify` accepts the Go-built VTXO key and rejects it for a different commitment. The proof restores arkd's untweaked key (`rgb-check/`).

Reasoned from code, not run end to end:

- the sweeper control-block failure;
- the ark tx output acceptance, beyond the P2TR class check;
- the batch (commitment) tx;
- the size of each patch;
- the fee and privacy figures.

No docker scenario was run.
