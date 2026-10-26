# Design

How RGB assets live inside Arkade VTXOs: where the RGB seals sit, which transaction carries each commitment, how receivers verify without the operator, how assets survive batch expiry, and what the operator can and cannot do. Measured numbers are in [test-results.md](test-results.md); the arkd / rust-sdk / rgb-lib changes are listed in [../PATCHES.md](../PATCHES.md).

Note on arkd change names (see PATCHES.md): `checkpoint-opret` (optional 32-byte OP_RETURN in the checkpoint tx), `opret-data-output` (a 0-sat OP_RETURN is data, not a sub-dust VTXO), `forfeit-opret` (optional OP_RETURN in the forfeit tx), `forfeit-cosign` (the operator co-signs the stored forfeit), `recovery-forfeit` (a swept VTXO is recovered through a forfeit), plus `subdust-restart-fix` (arkd restarts with 32-byte non-key sub-dust payloads).

## Background

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

## Seal placement and transaction layout

### Jump-in: bringing an asset into Arkade

#### The predicted-checkpoint trick

The txid of the checkpoint tx depends only on the VTXO outpoint and on the server's checkpoint tapscript, not on the outputs of the ark tx. Alice can therefore compute in advance (dry run of `build_offchain_transactions`) the txid of the checkpoint with which she will spend her VTXO `V`, and use `cp(V):0` as the RGB seal. The ark tx, built by the user, spends `cp(V):0` and carries the opret commitment of the transition to Bob's output.

#### Two ways to bring the asset to cp(V):0

- **On-chain issuance + jump-in (recommended)**: Standard rgb-lib issuance on a mined UTXO; then an on-chain tx T1, built by the user, transfers the asset to the explicit seal `cp(V):0`. Alice gets `V` by boarding ordinary BTC, separately.
- **Issuance on a VTXO**: The genesis assigns the supply directly to `cp(V):0` (rgb-lib patch `issue_asset_nia_to_outpoint`). Simpler, but the genesis is not on a mined UTXO.

#### Flow Alice → Bob → exit → Carol

1. **On-chain genesis**: mined rgb-lib UTXO
2. **Alice's BTC boarding**: settle → VTXO V
3. **cp(V) prediction**: checkpoint dry run
4. **Jump-in T1 (mined)**: opret, asset → cp(V):0
5. **Ark tx Alice → Bob**: opret 1 sat, asset → output 1
6. **Bob accepts the consignment**: tentative off-chain witness
7. **Bob's unilateral exit**: leaf, checkpoint, ark tx mined
8. **Bob → Carol on-chain**: after CSV 512 s, opret
9. **Carol: settled 1000**

#### Technical details

- Opret only, no tapret: the taproot outputs of VTXOs, checkpoints and the tree have keys and scripts fixed by the Ark protocol (and checked by the server by rebuilding them), so the user cannot add a tapret tweak. In the ark tx the user controls the outputs and can insert an `OP_RETURN`.
- 1 sat on the opret: arkd treats `OP_RETURN <32 bytes>` as a sub-dust VTXO; it must be worth at least `vtxoMinAmount`. arkd v0.9.15 rejects 0 ("vtxo min amount must be greater than 0"), so we use 1 sat.
- `maxburnamount`: bitcoind (v31) by default rejects via RPC a tx with a non-zero-value `OP_RETURN`; during the exit the ark tx must be submitted with a raised `maxburnamount`. It is only an RPC check; the tx is standard.
- rgb-lib quirk: with P2TR outputs `color_psbt` shifts indexes by +1 (opret first); we insert an empty `OP_RETURN` at index 0 and pass index - 1.
- During the exit it was arkd itself that published the checkpoint tx; the client published the leaf and the ark tx.

### Off-chain transfer: commitment in the checkpoint and in the ark tx

#### How each hop works

| Tx | Outputs | RGB transition |
|---|---|---|
| Checkpoint (spends the VTXO) | `[OP_RETURN 32B (0 sat), checkpoint, anchor]` | T1: VTXO seal → `CP:1` |
| Ark tx (spends `CP:1`) | `[OP_RETURN 32B (0 sat), receivers, change, anchor]` | T2: `CP:1` → receiver and change |

- The jump-in now points directly to Alice's VTXO `V`: predicting the checkpoint txid is no longer needed.
- T1 uses a witness-vout seal, so the commitment is computed before the checkpoint txid: no circularity. arkd rebuilds checkpoint and ark tx and the txids match the local ones.
- Finding: the checkpoint `OP_RETURN` must be at index 0. RGB opret1st requires it to be the first DBC-compatible output, and a P2TR output before it makes it rejected (first attempt failed). The checkpoint output therefore moves to vout 1 when there is a payload. Without a payload the upstream layout is unchanged.
- The receiver accepts chains of `Tentative` off-chain witnesses (2 for Bob, 4 for Carol) after a cross-check with arkd's `GetVirtualTxs`.

#### Implementation

- arkd (7 files, about +100 lines). `checkpoint-opret`: optional 32-byte `OP_RETURN` in the checkpoint, included in the deterministic rebuild; the checkpoint index is handled in the sweeper, indexer and watch list. `opret-data-output`: a 0-sat `OP_RETURN` is a data output, not a sub-dust VTXO (sub-dust VTXOs are worth at least `vtxoMinAmount` > 0), so no more 1 sat or `maxburnamount`. `subdust-restart-fix`: restart fix, because stock arkd does not restart if a sub-dust VTXO contains 32 bytes that are not a valid key, like the the basic flow opret.
- rust-sdk: `VtxoInput::with_checkpoint_op_return` and the same layout as arkd.
- rgb-lib: the Ark-aware resolver (`ArkAwareResolver`), `accept_transfer_consignment_offchain` and `color_psbt_and_consume_custom` (seals on exact vouts or external outpoints).

Limits still open after the off-chain transfer scenario: the rgb-lib DB does not see hand-colored transfers (the RGB stock does). Refresh, collaborative exit and negative tests were covered in the forfeit-commitment scenarios.

### Refresh and collaborative exit: commitment in the forfeit

#### How it works

| Tx | Outputs | RGB transition |
|---|---|---|
| Forfeit (spends connector + VTXO `V`) | `[OP_RETURN 32B (0 sat), operator forfeit, anchor]` | T_f: `V` → `V'` (refresh) or `V` → `C:vout` (exit) |

- The forfeit is the only tx that spends `V` on this path, so it is the only place to close the seal. The new seal is known at the `BatchFinalization` phase: the VTXO tree leaf paying the refresh address, or the commitment tx output paying the exit address. No circularity.
- arkd (`forfeit-opret` in the table below): the deterministic rebuild of the forfeit accepts a 32-byte 0-sat `OP_RETURN` at index 0, and the txid comparison covers it. The VTXO signature (SIGHASH_DEFAULT) commits to the `OP_RETURN`, so the operator cannot remove it. Sweeper, indexer and the anti-fraud path needed no changes.
- In the honest case the forfeit is never mined, so for RGB it stays a `Tentative` witness forever. Every downstream validator attests it at the Ark level: commitment tx `C` mined, forfeit present in `GetForfeitTxs(C)`, connector input present in `GetConnectors(C)`, forfeit spending `V` with exactly one correct `OP_RETURN`.
- rgb-lib: `ArkAwareResolver` in all validation paths (including `update_witnesses`, which would otherwise archive the forfeit), `add_ark_attested_witness(tx, anchor)` and a safe-height wait based on the depth of `C`.
- Finding: without the last point the transfer stayed in `WaitingSafeHeight` forever, because RGB flags the unmined forfeit as unsafe history and rgb-lib waited for a confirmation that never comes.

## Offline verification: Ark proofs

In the forfeit-commitment scenarios every receiver had to ask arkd's indexer whether the forfeit was really registered. Now the sender attaches to the consignment a separate file (`.arkproof.json`) with all the unmined Ark transactions needed: forfeits, connectors, tree branches, checkpoints and ark txs of previous hops, down to a mined transaction. The receiver checks the file only against the blockchain.

- Every transaction must really spend the previous output, down to a mined commitment tx (the mempool is not enough).
- Signatures are verified: tree signatures against the output key; checkpoint, ark tx and forfeit signatures against the operator key fixed in the wallet configuration.
- Every RGB witness must have exactly one 32-byte, 0-sat `OP_RETURN` at index 0.
- Why a separate file: the RGB consignment has a fixed format and identifier; adding fields would change them.
- arkd change `forfeit-cosign`: the operator also signs the forfeit it stores. Without its signature a dishonest sender could invent a forfeit with a fake connector and spend the asset twice.
- Not verified: connectors (not signed by arkd, checked only as a link), the MuSig signers of tree nodes, and whether the batch has already expired.

Cost: about 2.5 kB per off-chain hop and about 1.8 kB per unmined forfeit (6–7 kB consignment). When the hops are mined through an exit, the proof empties (39 bytes); an unmined forfeit, instead, stays forever in subsequent proofs.

## Liveness: expiry, refresh, watchtower, recovery

### Why expiry matters more than for BTC

- On BTC-only Arkade an expired VTXO is not a drama: the operator sweeps, but the balance stays recoverable in a new batch. BTC has no history.
- An RGB asset, instead, is tied to a precise output (the seal) and to its history. The sweep spends the tree without an RGB commitment: the VTXO seal can never close, and recovering the BTC does not revive the asset.
- The delegator problem: refreshing while the user is offline would need a delegator. But the forfeit must commit to the RGB transition to the new VTXO, which is known only when the batch is already built, and the user's signature covers that commitment. The user would have to sign before knowing what they sign: circular. Signing without committing to the outputs would leave anyone free to change them.

1. **1. Automatic refresh while the wallet is online**: The normal case: the wallet refreshes colored VTXOs well before expiry (tested in the forfeit-commitment scenarios).
2. **2. Delegator, later**: Requires solving the circularity with arkd-side support. To be designed, not tested.
3. **3. Exit watchtower as a safety net**: If nobody refreshed, it takes the VTXO on-chain before the sweep. It costs on-chain fees, but the asset is safe (tested in the offline-proof work).

### Exit watchtower

The watchtower receives only the VTXO's already-signed exit transactions and a key of its own to pay fees: none of the user's keys. When 7 minutes are left before expiry (batch with a 17-minute expiry for the test) it asks the public indexer whether the VTXO was refreshed; if not, it publishes the exit branch, paying fees through the anchors.

Fees paid by the watchtower at 2 sat/vB: 640 sat for the tree root and 554 sat for the leaf, 1 194 sat in total for a two-level branch. Each extra level or hop costs about 550–650 sat. After the sweep Erin's RGB wallet still shows `settled: 1000`, but the seal can never be spent. Erin recovers her 100 000 sat in a new VTXO; the asset does not, because the recovery does not go through a forfeit and so there is no transaction that can carry the RGB transition.

### Options while the user is offline

| Option | Third-party trust for the asset | Cost | Assessment |
|---|---|---|---|
| (a) Longer batches (28 days) and automatic refresh every time the wallet opens if less than 14 days plus a margin are left | None | Paid by the operator, which locks BTC for longer. For a 1 000-sat colored VTXO it is about 4 sat. But the expiry in arkd applies to everyone, BTC-only VTXOs included. | Baseline solution |
| (b) Keyless watchtower, with fees already signed by the user (ladder from 1 to 64 sat/vB) | None: it cannot steal or redirect, at most force an early on-chain exit | Paid by the user with their fee UTXOs: about 550 sat per level at 2 sat/vB | Safety net |
| (c) Delegate that refreshes on the user's behalf | With the signatures possible today the delegate could redirect the asset | Trustless, it requires large arkd changes | Later, with Ark Labs |
| (e) Vacation mode: before leaving, the asset exits on-chain via collaborative exit | None, and no expiry | About 1 000 sat at 10 sat/vB, between exit and re-entry | Manual option |

> **Superseded by sweep recovery.** Longer batches are no longer the proposal: with recovery after the sweep the asset survives even months offline.

Initial proposal (the long-offline scenario): (a) as the baseline, (b) active automatically and run by KaleidoSwap, (e) available in the app, (c) to be discussed with Ark Labs. Who runs the watchtower? KaleidoSwap by default. The registration contains no secrets, so the user can also give it to a second watchtower, for example their own node, so that if one is down there is the other.

### Sweep recovery (default for long absences)

> **New recommendation for long absences.** There is no need to lengthen the batch expiry and no need for anyone to act while the user is offline. The batch expires, the operator sweeps; when the user returns, even after months, they recover the BTC and the RGB asset in the same step into a new VTXO.

How it works: Arkade already allows recovering the BTC of a swept VTXO, which stays "recoverable" in the operator's database. We made the asset travel the same road. On return, the wallet asks the operator to treat the swept VTXO like a live VTXO: it signs a forfeit, and inside the forfeit is the RGB commitment that moves the asset to the new VTXO. It is the same mechanism as the the forfeit-commitment scenarios refresh, already verifiable offline with the the offline-proof work proof.

Why not put the RGB commitment in the recovery request (the intent)? The intent is signed before the new batch exists, so it cannot say where the asset will go. The forfeit, instead, is signed later, when the new VTXO is known. Moreover, arkd discards a free-form OP_RETURN in the intent.

| Change | Detail |
|---|---|
| arkd (`recovery-forfeit`, about 25 lines) | New signed field in the intent, `rgb_recovery_forfeit`. If the VTXO is swept or expired, arkd still requires the forfeit, which `forfeit-cosign` co-signs and stores. Normal recoveries without the field do not change. |
| Ark client (rust-sdk, about 40 lines) | Adds the field and creates the forfeit for the swept VTXO too. |

**Pros**

- No action while offline, no watchtower, no longer expiry.
- The operator cannot steal or redirect the asset: the owner's signature is required.
- Safer than a normal refresh: after the sweep the old VTXO can no longer reach the chain, so there is no race with a unilateral exit.
- Costs the same as a refresh; the offline proof is 1.8 kB.

**Cons**

- The operator can refuse the recovery: the asset stays frozen, not destroyed, as BTC is today.
- If the operator and the user collude they can co-sign two forfeits and spend twice. It is the same trust as the the forfeit-commitment scenarios refresh, and it leaves public proof of the fraud.
- The operator must keep the swept VTXO's data: arkd v0.9.15 never deletes it. The wallet keeps its own proof anyway.

Updated recommendation: recovery after the sweep (sweep recovery) as the baseline, without lengthening the expiry. The the long-offline scenario watchtower becomes optional, for those who want an on-chain exit that does not depend on the operator's cooperation. Vacation mode remains available. The 28-day batch is no longer needed.

## Batches: several users, failed rounds, rollback

A batch can mix several users, several assets and BTC-only VTXOs: each colored VTXO's owner colors their own forfeit (or checkpoint/ark tx for an off-chain send), and every receiver validates its own consignment + Ark proof. If a round fails (for example a participant does not sign its forfeit, arkd: "missing forfeit tx"), the colored forfeit that was already signed is never co-signed and its commitment tx is never published; rgb-lib shows the asset as in flight until `ark_rollback_transfer` archives the never-completed transition, then the refresh is retried. rgb-lib refuses to roll back a witness that is mined, in the mempool or already attested. rgb-lib keeps a small registry of tracked VTXOs and custom-colored transfers (`ark_vtxos.json`, `ark_transfers.json` in the wallet dir) and derives each VTXO's status from the chain only: active, to recover (swept: a batch/tree output spent by a non-branch tx), unrolled, or spent.

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
| Multi-hop | No | Yes, tested (the off-chain transfer scenario) | Yes, tested after a refresh (the forfeit-commitment scenarios) |
| VTXO refresh | No, asset lost | No | Yes, tested (the forfeit-commitment scenarios) |
| Expiry | Asset destroyed | Asset destroyed | Timely refresh or exit watchtower (the offline-proof work); without, asset destroyed (tested) |
| Collaborative exit | No, asset lost | No | Yes, tested (the forfeit-commitment scenarios) |
| Unilateral exit | Yes, tested | Yes, linear cost | Yes, tested after a refresh |

Multi-hop today: Bob receives on the ark tx output; his next seal would have to be on the predicted checkpoint of his own VTXO, whose txid depends on the ark tx and therefore on the opret itself: circular. Without the patch Bob's checkpoint tx cannot carry a commitment.

Refresh and collaborative exit today: the commitment tx does not spend the VTXOs (its inputs are operator UTXOs and boarding inputs); the old VTXO is surrendered with the forfeit tx, which arkd rebuilds deterministically (outputs: operator + anchor) and compares by txid. The seal therefore stays on a VTXO that the user can no longer spend and that after the sweep can never exist on-chain: the asset is lost in practice. This corrects an earlier wording ("the commitment tx spends the VTXO without a commitment"): the result is the same, the mechanism is the forfeit.

## Recommended target model

### User experience

1. **Jump-in deposit**: On-chain asset → tx T1 to the predicted checkpoint of the user's VTXO; if the user has no VTXO, they get one via Lightning/Boltz (not tested) or with a BTC boarding.
2. **Instant multi-hop sends**: Every off-chain spend carries the commitment in the checkpoint and in the ark tx (requires `checkpoint-opret` and `opret-data-output`).
3. **Automatic refresh**: The wallet refreshes colored VTXOs before expiry with forfeits carrying the commitment (requires `forfeit-opret`).
4. **Collaborative exit**: The asset moves to the batch's on-chain output through the forfeit with commitment (requires `forfeit-opret`); the unilateral exit remains the fallback.

### arkd changes in order of cost

| Change | Cost | What it enables | Status |
|---|---|---|---|
| `checkpoint-opret`: checkpoint tx with an optional 32-byte user `OP_RETURN`, included in the deterministic rebuild | Small | Multi-hop: the checkpoint closes the VTXO seal and moves the asset to the checkpoint output (vout 1, because the `OP_RETURN` must be the first output); the ark tx carries it to the receiver | Tested |
| `opret-data-output`: distinguish the RGB opret from sub-dust VTXOs | Small | No burned sats or indexed pseudo-VTXOs (0-sat opret) | Tested |
| `forfeit-opret`: forfeit tx with a 32-byte `OP_RETURN` (0 sat, index 0), included in the deterministic rebuild | Small (2 files, +62/−1 lines) | Refresh and collaborative exit without losing the asset | Tested |
| `forfeit-cosign`: the operator also signs the forfeit it stores (`SignTransactionTapscript` in `finalizeRound`) | Minimal (8 lines) | Offline proof of the forfeit: no forfeit invented with a fake connector | Tested |
| (not implemented) direct onboarding: `OP_RETURN` in the commitment tx + a new batch phase | Large | Boarding of the colored UTXO; to be avoided | Not tested |

`forfeit-opret`: when the user signs the forfeit (`BatchFinalization` phase), commitment tx, tree and connectors are already known, so the new outpoint (new VTXO in a refresh, on-chain output in a collaborative exit) is known and does not depend on the forfeit. The forfeit can close the old VTXO's seal and move the asset without circularity. Stock arkd rejects any forfeit different from the rebuilt one (`VerifyForfeitTxs` compares txids); `forfeit-opret` extends the rebuild with the optional `OP_RETURN`, and that was enough (the forfeit-commitment scenarios).

### RGB side

- rgb-lib resolver for chains of off-chain witnesses: tested in the off-chain transfer scenario (the Ark-aware resolver (`ArkAwareResolver`), witnesses from the consignment as `Tentative` and cross-check with `GetVirtualTxs`).
- Ark-aware validation: tested in the forfeit-commitment scenarios. An unmined forfeit is accepted as a witness only after attestation at the Ark level (`GetForfeitTxs` and `GetConnectors` of the mined commitment); the safe height follows the commitment's depth.
- Offline proof: tested in the offline-proof work. `import_ark_proof` verifies the attached file with the blockchain alone and records the proven witnesses; after that, normal validation no longer needs arkd.
- Wallet: track colored VTXOs, exclude them from blind refreshes and from BTC coin selection, schedule refreshes and exits before expiry.

## Security model

**What stays as in pure RGB**

- Asset data stays private: on-chain and towards the operator only a 32-byte commitment appears.
- Unilateral exit always possible: the asset reaches the chain with mined witnesses (demonstrated).
- Preconfirmed txs require trust in the operator as for BTC on Arkade, no more.

**What gets weaker or must be managed**

- With unmined forfeits (`forfeit-opret`) seal uniqueness depends permanently on Ark: arkd must not accept a second spend of the same VTXO, and must publish the forfeit if the user unrolls the old VTXO. An operator that is offline or colluding before the sweep is the critical scenario; the anti-fraud path with a colored forfeit has not been tested.
- Every future receiver must receive the proof of the unmined forfeit. After the offline-proof work arkd's indexer is no longer needed, but the proof file must be kept and passed along together with the consignment (about 1.8 kB per forfeit).
- Liveness: if a colored VTXO expires and is swept, the operator recovers the BTC but the asset is destroyed (demonstrated in the offline-proof work). Timely refreshes or an exit watchtower are needed, which costs on-chain fees.
- The exit cost grows linearly with the off-chain chain (every checkpoint and ark tx must be published).
- Selective censorship is possible: a 32-byte opret in the ark tx is recognizable.
- Opret only: less on-chain privacy on exit compared to tapret.
- Data availability: the receiver must keep the consignment; without it the asset cannot be spent.

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

## Atomic swap on Arkade with RGB (theory only)

> **Untested proposal.** This section describes how an atomic swap could be done. No part has been tried on regtest; it does, however, build on already-tested pieces: commitment in the checkpoint and in the ark tx (the off-chain transfer scenario), commitment in the forfeit (the forfeit-commitment scenarios), offline proof (the offline-proof work) and rollback of a failed batch (the batch scenarios).

### Basic rule

In RGB the receiver must have the consignment, otherwise they cannot spend. So in every variant the two parties first exchange consignments and proofs (built on the not-yet-signed txs, whose txids are already fixed) and verify them. Only then do they sign. This way nobody signs without having already checked what they receive.

### (a) Asset for BTC in a single ark tx

1. **Construction**: A single ark tx with two inputs: Alice's colored VTXO (through her checkpoint) and Bob's BTC VTXO (through his checkpoint). Outputs: 0 = OP_RETURN with the commitment; then Bob's new VTXO with the asset, Alice's new VTXO with the BTC, and any change.
2. **Commitment**: Alice's checkpoint carries transition T1 (asset from Alice's VTXO to the checkpoint output), as in the off-chain transfer scenario. The ark tx carries T2 (from the checkpoint output to Bob's VTXO, plus any asset change to Alice). Bob's checkpoint has no commitment.
3. **Verification**: Alice gives Bob the consignment of T1+T2 and the proof with the unsigned PSBTs. Bob checks that the asset arrives at his VTXO and that the BTC for Alice are the agreed amount.
4. **Signatures**: Each party signs their own checkpoint and their own ark tx input (user leaf + operator). The PSBT passes from one to the other; then the client submits it to arkd, which co-signs.
5. **Outcome**: A single transaction: either both legs hold or neither does. Neither party can exit on-chain with their own leg without also the other's checkpoint.

### (b) Asset A for asset B

Same structure, but both inputs are colored. Each checkpoint carries its owner's T1. The ark tx OP_RETURN carries a single commitment containing two transitions (RGB allows several contracts in the same commitment): asset A to Bob and asset B to Alice. The PSBT is passed between the two parties and each adds its own transition before the final commitment is computed. A small rgb-lib function is needed to add to the PSBT the transition prepared by the other party.

### Same-batch variant

The two parties register a single intent with both parties' inputs (the BIP322 proof has one signature per input, from different owners) and crossed outputs. At finalization each signs the forfeit of their own VTXO with the RGB commitment to the other party's new VTXO (as in the forfeit-commitment scenarios). The batch is atomic: if a forfeit is missing, arkd cancels the round and nothing happens (measured in the `failed-batch` scenario). Whoever had already colored their forfeit rolls it back. Here atomicity depends on the operator as for any Ark batch: it must not publish the commitment without all the forfeits.

### HTLC variants (beyond Arkade's boundaries)

| Swap | How | Timing |
|---|---|---|
| Arkade ↔ on-chain | Alice locks the asset in a VHTLC (a VTXO with a "preimage + Bob + operator" path and a refund to Alice after a timeout); the RGB seal is the VHTLC outpoint. Bob locks BTC or another asset in an on-chain HTLC with the same hash. Bob claims the VHTLC by revealing the preimage (the spending tx carries the RGB commitment as in the off-chain transfer scenario); Alice reads the preimage and claims on-chain. | On-chain timeout longer than the VHTLC's, with a margin; the VHTLC must not expire (batch) before the refund: refresh or recovery after the sweep (sweep recovery). |
| Arkade ↔ RGB Lightning (KaleidoSwap channels) | From Arkade to the channel: the user locks the asset in a VHTLC to the LSP; the LSP pays the user's RGB-LN invoice with the same hash; the user, claiming on LN, reveals the preimage and the LSP claims the VHTLC. The other way: the LSP locks the asset in a VHTLC to the user and the user pays an RGB-LN hold invoice from the LSP. | The VHTLC timeout must exceed the LN HTLC's, plus the time of a unilateral exit from Arkade (blocks to unroll the branch). |

### What can go wrong

- One party does not sign: the ark tx or the batch does not exist; the prepared transitions must be cancelled with the rollback (the batch scenarios).
- One party signs the ark tx but does not complete their checkpoint: the tx stays pending for both, because the ark tx spends both checkpoints; the operator can complete it later.
- Consignment not delivered after the swap: avoided by exchanging consignments before signing.
- HTLC: the preimage is revealed but the claim does not get in on time; or the operator censors the VHTLC spend and a unilateral exit is needed, which takes time and fees.
- Batch expiry during the swap: the locked VTXO must be refreshed or recovered after the sweep.

### Changes needed

- arkd: the arkd patch set already cover the commitment in checkpoint, ark tx and forfeit, and recovery after the sweep. It must be verified that the stock code accepts an ark tx with inputs from different owners and an intent with inputs from several owners (it should, but it is not proven).
- Ark client: multi-party signing of the same PSBT (passing the PSBT between the parties) and a VHTLC with an OP_RETURN in the claim tx.
- rgb-lib: adding to the same commitment a transition prepared by another party; automatic rollback if the swap fails.

### Trust and security

In the same ark tx, atomicity is that of a Bitcoin tx: no more trust than Arkade already requires for preconfirmed txs. In the batch variant, atomicity depends on the operator as for every batch. With HTLCs the usual timeout rules apply, plus the Arkade exit times. In all variants the the batch scenarios fraud case remains: an operator colluding with one of the parties can co-sign a second spend of the same VTXO; it is detected, not prevented.

## Open questions for Ark Labs

- Would you accept an optional 32-byte user `OP_RETURN` in the deterministic rebuild of checkpoints and forfeits?
- How stable is the checkpoint tapscript (`checkpointTapscript`)? A rotation breaks the prediction.
- Can an opret be excluded from sub-dust VTXOs, or a value of 0 be allowed?
- Can `GetVirtualTxs` or an equivalent API serve off-chain chains to an RGB resolver?
- Would you accept the operator also signing the stored forfeit, so that it is verifiable without arkd (`forfeit-cosign`)?
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
