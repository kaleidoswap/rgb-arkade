# Arkade Script and the emulator with RGB assets

Question: can the Arkade emulator or Arkade Script be used with our RGB assets?

Short answer: not in a way that helps. The emulator rejects our checkpoint and forfeit layouts. It is run by the operator. It only sees bitcoin, so it cannot check an RGB transition. Keyless covenant renewal (`OP_TUNNEL`) would let the solver or the operator choose where the asset goes.

## What they are (checked on 2026-11-02)

| Fact | Source |
|---|---|
| The emulator (renamed from "introspector" on 2026-05-26) is a signing service. It runs Arkade Script and co-signs only when the script passes | github.com/arkade-os/emulator (README, PR #90) |
| "Alpha — available on testnet." Tags up to `v0.0.9` (2026-10-23); `go.mod` pins arkd master `f863e48`, the same commit our `arkd-checkpoint.patch` is based on | emulator README, `go.mod` |
| "The emulator is part of the operator, not a separate party." Mutinynet `emulator.mutinynet.arkade.sh`; regtest `localhost:7073` | docs.arkadeos.com/learn/concepts/emulator |
| Script path = 2-of-2 `(arkd signer, emulator key + tagged_hash("ArkScriptHash", script)·G)`. The user keeps a CSV exit leaf | README "Dual VM model", `test/delegate_test.go` |
| The script and its witness are revealed in an `OP_RETURN` ARK extension (`ARK` magic, packet type `0x01`) in the Arkade tx. Arkade Assets use packet type `0x00` in the same output | README "Emulator Packet" |
| About 50 opcodes: input/output/tx introspection, `OP_CAT`/`SUBSTR`, 64-bit+ BigNum arithmetic, `OP_CHECKSIGFROMSTACK`, SHA256 streaming, `OP_MERKLEBRANCHROOT`, EC ops (secp256k1, P-256, BN254 pairing), `OP_PUSHEXPIRY`/`OP_CHECKTIME` (emulator clock), intent-message and packet introspection, Arkade Asset opcodes, and `OP_TUNNEL` (native delegation) | README "Supported Opcodes", docs |
| Scripts run only on the Arkade tx and intent proofs, never on checkpoints. Forfeits are signed without running a script: the emulator checks the intent signature, the connector tree and the forfeit's shape | README `SubmitTx`, `SubmitFinalization`; `pkg/emulator/tx.go`, `finalization.go` |
| arkd has no emulator code. The emulator is a client of arkd's public API, so using it needs no arkd change | `rg emulator` in arkd master: no hits |
| Compiler `arkadec` and a browser playground exist | docs.arkadeos.com/learn/concepts/compiler |

## Probe (Go unit tests against emulator master `7b830f6`, no docker)

The files are in `docs/arkade-script-probe/`. They run inside a clone of the emulator (`pkg/emulator`, `pkg/arkade`).

| Check | Result |
|---|---|
| `validateCheckpoint`, stock `[cp, anchor]` | accepted |
| our `checkpoint-opret` layout `[opret, cp, anchor]`, ark input spends vout 1 | **rejected**: `checkpoint must have one vtxo output and one anchor output` |
| `[cp, anchor, opret]` | **rejected**: same error |
| `validateForfeitOutputs`, stock | accepted |
| our `forfeit-opret` layout (3 outputs) | **rejected**: `expected 2 outputs, got 3` |
| Script: output 0 must be `OP_RETURN <C>` with `C` pinned | passes with C, fails with D |
| Script: output 0 must be some `OP_RETURN <32B>` (stops a spend with no commitment) | passes with one, fails with none |
| Script on the Arkade tx checks the **checkpoint's** opret: the witness carries the raw checkpoint, `OP_HASH256` must equal the spent txid, the spent vout must be 1, and bytes `[56:90]` must be `6a20‖C` | passes; tampered bytes fail |

Not done: a live regtest run (arkd `rgb-cponly` with the emulator container). The rejection comes from `validateCheckpoint` in `SubmitTx`, which runs before anything is signed, so a live run would show the same error. To make it pass, the emulator itself would need a patch.

## Fit with RGB

- **Script cannot see RGB.** It sees bitcoin outputs only. It can require an opret, or pin an exact 32-byte commitment, but it cannot tell whether that commitment holds a valid transition, or where that transition sends the asset. Checking that in script would mean running RGB commitment and transition validation (MPC tree, bundle, strict encoding) inside Arkade Script. This might be possible in theory with `OP_CAT` and SHA256 streaming, but it is impractical and was not tried.
- **Keyless covenant renewal (`OP_TUNNEL`, `delegate_test.go`)** keeps the scriptPubKey, the value and the Arkade Assets on the intent output. It does not cover RGB. Renewing through a batch closes the RGB seal in the forfeit, and that transition has to point at the new VTXO, whose outpoint only exists once the batch is built. So someone online writes the commitment at batch time. The emulator signs the forfeit without running a script, and RGB transfers need no owner signature, so whoever builds the forfeit can send the RGB asset to their own seal while the sats go back to the owner. That is theft by the solver, and also by the operator. On top of that, the covenant leaf is `arkd + emulator` and the emulator is the operator, so the operator alone can spend that leaf at any time. Against the user's collusion criterion this is worse than the delegate schemes already rejected, which at least needed two parties to collude.
- **Pinned commitment instead.** If the commitment is fixed in advance (for example a transition to a witness-vout seal), an owner pre-signature already enforces it today, as `checkpoint-offline` does, with no emulator and no operator-only spend path.
- **Non-interactive RGB sell offers (not tested, needs both the A and B patches).** The pinned commitment would assign the asset to the checkpoint output (witness vout 1). The covenant would require a payment to the maker, and the taker would then move the asset on with an ark-tx opret. This is technically conceivable, but the offer sits in an operator-spendable leaf and needs an emulator patch. An interactive swap already works without the emulator (`checkpoint-swap`).
- **Accidental-burn guard.** Possible (probe rows 6–8), but RGB wallets already refuse such spends, and in a 3-of-3 with the user the guard only protects the user against their own wallet.
- **Expiry.** `OP_PUSHEXPIRY` and `OP_CHECKTIME` use the emulator's clock. That clock belongs to the operator.

## Impact on arkd changes

Adding the emulator changes nothing in arkd. It does not reduce our patch either, since it cannot carry an RGB commitment by itself. It adds work: patching the emulator's `validateCheckpoint` (plus `prevout.go` and the signing index) to allow `[opret, cp, anchor]` with the ark input on vout 1, and `validateForfeitOutputs` for C. That is a second Ark Labs component to change, and it brings a second `OP_RETURN` (the ARK extension) into the Arkade tx.

## Recommendation

Do not use the emulator or Arkade Script for RGB assets. Keep keyless watchtowers with unilateral exit plus pre-signed hops. Revisit the emulator only if Ark Labs ships it as a key independent of the operator, and an RGB-aware check becomes feasible. Arkade's own assets (Arkade Asset V1) are the use case the emulator was built for. They are a different, operator-trusted design (see `docs/operator-risk.md`, O13).
