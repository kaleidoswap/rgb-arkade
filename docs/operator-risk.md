# Operator risk

How to reduce operator-related risk in RGB on Arkade while changing arkd as little as possible. The recommended option (O2 + O1) is now implemented and run on regtest: see [delegate-renewal.md](delegate-renewal.md) and section 7. The rest is analysis. Inputs: [design.md](design.md), [approaches.md](approaches.md), [test-results.md](test-results.md), [../PATCHES.md](../PATCHES.md), `patches/arkd.patch` (v0.9.15), `patches/arkd-checkpoint.patch` (master `f863e48`), and upstream arkd read at `f863e48` (master, 1 Sep 2026, still the head on 2 Nov 2026) and `v0.9.16`.

"Operator risk" has two readings:

- **(a) Trust in the operator.** What a user risks because arkd (Ark Labs) can refuse, lie, collude or disappear.
- **(b) Risk for the operator.** What arkd takes on by supporting RGB: liability, DoS, liquidity, fees, storage, griefing.

Hard requirements used to judge every option:

- **R1.** An asset is never lost, even after 2+ months offline. Long batch expiry is not an acceptable fix.
- **R2.** Minimal out-of-band data and validation time at USDt-like volume.
- **R3.** Minimal arkd changes.

## Summary

- **The 2-month guarantee does not need the operator.** A delegate key in a spend leaf of the VTXO can renew a colored VTXO every period without the owner. The owner presigns nothing, so the seal circularity goes away. A keyless watchtower exit is the fallback when the operator refuses. Neither needs any arkd change beyond `checkpoint-opret` (B). Stock arkd already accepts such leaves, and a probe confirmed it.
- **Presigned recovery is not viable.**
  - Since `v0.9.16` (#58) arkd's signer refuses any sighash except DEFAULT/ALL.
  - A fixed-output presign needs a seal that already exists.
  - Any presigned spend of a live VTXO is a second operator-signed spend of the same seal.
- **No leaf-based recovery after a sweep is possible.** The sweep spends the batch output above the VTXO, so no VTXO leaf (CSV, Arkade Script, delegate) ever reaches the chain.
- **The minimal-diff target** is arkd master plus B (5 files, +86/−11). A (opret-data-output) is optional. Drop `forfeit-cosign` (upstream #1110 does it) and `recovery-forfeit` (replaced by the delegate and the watchtower). Keep C (forfeit-opret) out of the minimal set.
- **Collusive double spend stays detect-only in every lane.** This holds for B as well, because checkpoints are unmined operator-co-signed txs, just like forfeits. It can be narrowed by client-side means (a first-seen co-attester), not removed without arkd or consensus work.

## 1. Risk list

### (a) Trust placed in the operator

| # | Risk | Where it arises | Lane | Status today |
|---|---|---|---|---|
| a1 | **Refused sweep recovery means the asset is lost.** The swept seal can never close again | `recovery-forfeit`: `RegisterIntent` sets `ForceForfeit` only on request, and arkd may ignore or refuse the intent (`service.go` patch 6, `domain/vtxo.go`). Sweep: `sweeper.go` | full | Cooperative case only (`sweep-recovery`) |
| a2 | **Collusive double spend.** Operator plus owner co-sign two spends of one VTXO, and two offline receivers both accept | Forfeits: `forfeit-opret` + `forfeit-cosign` (`builder.go` `VerifyForfeitTxs`, `finalizeRound`). Checkpoints: B (`SubmitOffchainTx`). Uniqueness is only arkd's DB `Spent` flag. Receiver: `ark_proof.rs` accepts any operator-co-signed witness | B and full | Detected (fraud proof, `spent_by`), not prevented |
| a3 | **Data availability.** Receivers and exits need unmined txs (checkpoints, forfeits, tree branches) that only arkd serves (`GetVirtualTxs`, indexer `walkVtxoChain`) | rgb-lib `ArkAwareResolver` online attestation (`get_forfeit_txids`); exit needs the signed branch | all | `.arkproof` sidecar removes it for validation. Exit data relies on the wallet keeping the bundle |
| a4 | **Censorship of RGB txs.** An opret checkpoint or forfeit is recognizable (32-byte `OP_RETURN` at vout 0), so arkd can refuse checkpoints, refreshes or recoveries selectively | `ExtractCheckpointOpReturn`, `ExtractForfeitOpReturn` | B and full | Fallback: unilateral exit before expiry (fees) |
| a5 | **Expiry sweep.** An owner offline past expiry with nothing acting for them loses the asset (operator sweep is legitimate) | `sweeper.go`; batch expiry | all | Watchtower (keyless, funded) or delegate pre-signed hop (one extra period) |
| a6 | **Delegation limited to about one extra period.** The owner's signature must commit to a destination that does not yet exist (seal circularity) | `approaches.md` "Not yet covered"; owner-presigned hop in `checkpoint-offline` | B | Open |
| a7 | **Stale stock balance.** Stock rgb-lib still shows a swept asset as `settled`, so the user does not react | rgb-lib upstream balance; fork has `get_asset_balance_ark` | all | Fork only |
| a8 | **Proof growth in B.** Checkpoints are never mined (other tree), so each transfer or renewal adds about 0.94 kB (binary) to every later proof, forever | design.md "Offline verification" | B | Documented only |
| a9 | **Key rotation.** Proofs are checked against pinned operator keys. A rotation without published key history breaks old proofs | README "pin them out of band" | all | Convention only |

### (b) Risk the operator takes on

| # | Risk | Where it arises | Severity |
|---|---|---|---|
| b1 | **Penalty-path change.** `forfeit-cosign` edits `broadcastForfeitTx`: it strips the operator's own sigs and tolerates "already locked" connectors. A bug there breaks the operator's reaction to an unrolled forfeited VTXO, which is direct fund loss | `fraud.go` (+37 lines) | High for a small patch. Upstream #1110 (`signForfeitTxs`, `service.go:2255`, master only, not in v0.9.16) makes it unnecessary |
| b2 | **Arbiter of asset recovery.** With `recovery-forfeit` the operator decides whether a swept balance (e.g. USDt) comes back. That is liability and pressure | `recovery-forfeit` | High (legal or reputational) |
| b3 | **Recovery griefing.** `rgb_recovery_forfeit` forces a forfeit plus connector for *any* swept or expired non-note VTXO, with no RGB check. Each one costs a dust connector output in the commitment tx | `RegisterIntent` patch 6 | Low to medium |
| b4 | **Fraud proofs name the operator.** Two operator signatures on spends of one VTXO are public evidence (`ArkFraudProof`) | rgb-lib `verify_ark_fraud_proof` | Intended. It forbids any scheme where the operator legitimately signs two spends (see O4) |
| b5 | **Unroll load.** Watchtower exits (the 2-month fallback) put branches on-chain. arkd must then sweep fragmented subtrees and react to unrolled checkpoints | sweeper; checkpoint sweep (`scheduleCheckpointSweep`) | Low to medium. It scales with how often the fallback fires |
| b6 | **Arbitrary data in ark txs.** A (`IsDataOpReturn`) accepts *any* 0-sat `OP_RETURN` as data, not only 32 bytes. It is still bounded by `maxOpReturnOutputs` | `script.go` `IsDataOpReturn`, `validateOffchainTxOutputs` | Low. Restrict to `OP_RETURN PUSH32` before upstreaming |
| b7 | **Visible RGB activity.** The operator co-signs txs it can identify as RGB, which invites regulatory pressure to censor (feeds a4) | opret layout | Policy; not fixable with opret (tapret is worse, see `tapret-vs-opret.md` on branch `tapret-comparison`) |
| b8 | **Storage and serving.** Without A, every opret is a 1-sat sub-dust VTXO row. Long checkpoint chains make `walkVtxoChain` and `GetVirtualTxs` heavier | `db/service.go`, `indexer.go` | Low |

## 2. Upstream arkd facts that change the picture

Read from the code at `f863e48` (master) unless marked otherwise.

| Fact | Where | Effect |
|---|---|---|
| Forfeit (collaborative) closures may hold **any key set that includes the signer**: `MultisigClosure`, `CLTVMultisigClosure`, `ConditionMultisigClosure`. Nothing requires the owner key | `pkg/ark-lib/script/vtxo_script.go` `Validate` (l. 97–132); the same check is in v0.9.15 | A VTXO can carry a delegate leaf `{D, S}` with no owner key |
| `SubmitOffchainTx` accepts a spend through any of those closures and enforces CLTV against the chain tip | `service.go` l. 878–930 | A delegate can make an off-chain hop (with B, carrying the RGB commitment) |
| `buildCheckpointTx` copies the spending leaf into the checkpoint output | `pkg/ark-lib/offchain/tx.go` | The ark tx spends the checkpoint through the same delegate leaf |
| Intent proofs **skip the signer key**: `intent.Verify(..., allSignerPubkeys)` | `pkg/ark-lib/intent/proof.go:59`, `service.go:4867`; also in v0.9.15 | A delegate holding a `{D, S}` leaf can register a batch intent alone |
| Upstream delegated refresh: the user presigns the intent and an `ALL\|ANYONECANPAY` forfeit, and the delegate path is `{user, delegate, operator}`. **Observed on master + arkd-wallet v0.9.16:** the batch fails with `failed to sign forfeit tx: unsupported sighash type 0x81` (the #58 signer check), so this presigned flow does not work on this stack | `internal/test/e2e/e2e_test.go` `TestDelegateRefresh` (l. 2219); Fulmine `FULMINE_DELEGATE_ENABLED`; ts-sdk PR #262; docs.arkadeos.com "intent delegation" | Fixed outputs, so for colored VTXOs it has the same seal circularity. Usable only for the *uncolored* spare |
| The operator signer accepts only `SIGHASH_DEFAULT`/`ALL` | `pkg/ark-lib/script/script.go:135` `CheckSigHashType`, used in `pkg/arkd-wallet/.../wallet/service.go:674`; #58, in v0.9.16, not in v0.9.15 | Rules out operator presigs with NONE, SINGLE or ANYONECANPAY |
| Forfeits are signed at collection time | #1110 (`13a33138`, master, not v0.9.16), `signForfeitTxs` `service.go:2255` | Makes `forfeit-cosign` redundant (confirmed: `multihop-exit` passes on master without it, test-results.md) |
| Expired but unswept VTXOs require a forfeit | #94 (`4bf83cfd`, in v0.9.16), `domain/vtxo.go:80` | Half of `recovery-forfeit` is upstream. Only the swept case still needs it |
| Unrolled VTXOs can rejoin a batch as boarding inputs | #961, `service.go:1774` (in v0.9.15) | **Hazard:** the commitment tx would close a colored seal with no commitment. The wallet must never do this for a colored output |
| Extension `OP_RETURN` (`ARK` magic, typed packets; unknown types round-trip) and Arkade Assets (`pkg/ark-lib/asset`, `Vtxo.Assets`) | `pkg/ark-lib/extension/README.md` | Not an RGB carrier (RGB needs exactly `OP_RETURN PUSH32`). It does show arkd accepting user data in ark txs. Arkade Assets trust the operator to carry asset state across a swept-VTXO recovery |
| Introspector (Arkade Script) is a separate co-signer whose key is tweaked by the script hash; the script is revealed in an extension `OP_RETURN` | github.com/ArkLabsHQ/introspector; rust-sdk PR #212 | It can constrain bitcoin outputs, not RGB transitions. It adds a second `OP_RETURN` (see O13) |

### Probe (run, not committed)

A Go test against `pkg/ark-lib` at `f863e48`, `TestDelegateLeafProbe`:

| VTXO script | `Validate(signer, 512 s, blocks=false)` |
|---|---|
| `{A,S}`, `{D,S}`, `CSV{A}` | OK |
| `{A,S}`, `CLTV(t){D1,D2,S}`, `CSV{A}` | OK |
| `{A,S}`, `CSV{A}`, `CSV{W}` (extra exit key) | OK (accepted, but it would hand W the coins; do not use) |
| `{A,S}`, `{D}` (no signer) | Rejected: `signer pubkey not found` |

`offchain.BuildTxs` spending through `{D,S}`: the ark tx spends the checkpoint through the same `{D,S}` leaf. End to end on a running arkd: **not run**.

## 3. Options

Legend for the arkd column: lines beyond the B patch, unless noted.

| ID | Option | Risks | arkd change | Client work | R1: 2 months offline | R2: proof / validation | Residual trust | Feasibility |
|---|---|---|---|---|---|---|---|---|
| **O1** | **Keyless multi-watchtower exit as a service.** N independent towers, each holding the presigned branch and paying fees through P2A CPFP | a1, a4, a5 | 0 | Exists (`exit-watchtower`). Add: N-tower registration, fee prepayment or per-exit fee, deadline policy | **Yes, with no operator involvement.** Asset parked on-chain until return | Exit publishes the branch: hops mined, proof shrinks (except B checkpoints in other trees) | 1-of-N tower liveness, fee funding, fee spikes before the deadline. Costs about 550–650 sat per level | High (measured, `long-offline`) |
| **O2** | **Delegate leaf renewal.** Receive addresses carry `CLTV(T){D,S}` (or `{D1..Dk,S}`). Each period the delegate (i) refreshes the owner's uncolored spare VTXO in a batch (it signs the intent and forfeit itself) and (ii) hops the asset onto it with a B checkpoint it signs. The ark tx returns the old sats as the next spare | a5, a6, a1 (removes the need for recovery) | 0 | RGB-aware delegate (extend the watchtower daemon or fork Fulmine's delegate). Wallet: address template with D and T, scanning, keep a copy of the stash at the delegate | **Yes, unbounded,** while the operator cooperates. If it refuses, the delegate triggers O1 | +1 checkpoint hop per period (about 0.94 kB binary, about 14.5 ms). About 9 hops for 2 months at 7-day expiry | **D and S together** can move the asset after T. arkd cosigns any valid spend mechanically, so in practice **D alone** can (N3, measured). The CLTV T, k-of-n delegates and a receiver policy check limit it (section 7) | **Run end to end on regtest** (`delegate-renewal`). The delegate joins batches live (`rgb_settle_inputs`), not with presigned ACP forfeits |
| O2c | O2 in the full lane: delegate refresh of the colored VTXO itself with C. The delegate signs the forfeit after the batch outputs are known, so there is no circularity | same | 0 beyond C | Delegate builds the forfeit transition | Yes | +1.8 kB per refresh, forever | Same as O2, plus C's forfeit uniqueness | Medium |
| O3 | Upstream delegation (`{A,D,S}`, owner-presigned intent and forfeit, Fulmine) for the **uncolored spare only**, plus the owner-presigned colored hop | a6 (partly) | 0 | Use Fulmine as is | About one extra period only | none | none extra | High, but does not meet R1 alone |
| O4 | Operator-presigned recovery made at receive or refresh time | a1 | arkd-wallet sighash plus a new API; tens of lines | Large | Would remove refusal | n/a | **Breaks uniqueness:** NONE/SINGLE/ACP lets the owner alone build many recoveries. With ALL the destination must already exist, and the presig conflicts with every later legitimate spend of the VTXO (two operator-signed spends, which is what b4 calls fraud). No APO on Bitcoin | **Reject** |
| O5 | Recovery leaf in the VTXO (CSV user-only, Arkade Script, delegate) used *after* the sweep | a1 | n/a | n/a | No: the sweep spends the batch output above the VTXO, so no VTXO leaf ever reaches the chain. *Before* expiry the CSV exit leaf is O1 | n/a | n/a | **Impossible** |
| O6 | Keep `recovery-forfeit` as an optional last resort | a1 | ~20 lines on v0.9.15. On master only the swept case is left (#94) | Exists | Only if the operator cooperates | +1 forfeit | Operator decides (b2, b3) | Works, but not in the minimal set |
| **O7** | **Public DA for proofs and exit bundles.** Encrypted `.arkproof` plus branch, posted to Nostr or Blossom by sender, receiver and towers | a3 | 0 | Publish and fetch; encrypt to the receiver or owner key | Helps: exit data survives a dead arkd and a lost device | No change to proof size; only the transport | Relay availability (any 1 of N); metadata privacy | High |
| **O8** | **First-seen co-attester.** Receivers accept an RGB spend of VTXO V only after an independent service (e.g. the KaleidoSwap hub, or k-of-n) confirms that V's spend it saw first is this one. It also mirrors arkd's `spent_by` | a2 | 0 | Small service plus a receiver check. Optional per amount (only above a cap) | Neutral | +1 small signed statement per receive (online) | A double spend needs **operator, sender and co-attester** together. Offline receives stay detect-only | Medium-high |
| O9 | Bond plus slashing: an operator attestation key with a committed nonce per VTXO, so two attestations leak the key that controls a bond UTXO | a2 | arkd-wallet nonce control or an operator-run sidecar attester; ~100+ lines | Verifier changes | Neutral | +1 signature per witness | Makes collusion costly, does not prevent it | Research; not for the MVP |
| O10 | Operator-signed history checkpoints (constant-size proof) | a8 | Sidecar or a new API | Verifier changes | Neutral | Constant | **Adds** trust in the operator's verification | Reject for now. Prefer a receiver cache or hub (test-results.md) |
| **O11** | **Patch slimming.** Master plus B; A optional and restricted to `PUSH32`; drop `forfeit-cosign` (#1110), `recovery-forfeit` (O1 + O2); C optional | b1, b2, b3, b6, b8 | Diff shrinks to +86/−11 (B). A adds about 18 lines, C about 63 | Rebase the rust-sdk hooks | Covered by O1 + O2 | B: proof grows per hop (a8) | Operator signs only standard templates plus an opaque 32-byte output | High |
| O12 | Client hygiene: ship Ark-aware balance and status (`get_asset_balance_ark`) upstream in rgb-lib; min lifetime on by default; never settle a colored VTXO in a batch (B) and never rejoin an unrolled colored output (#961); pin key history | a7, a9, #961 hazard | 0 | Small | Prevents silent loss | none | none | High |
| O13 | Swap or Tessera reuse: an LP swap to exit or renew; an introspector covenant as a policy delegate | a5, a2 | 0 | Large | Swaps need the owner online, so no help offline. An introspector can force the sats back to the owner but **cannot check the RGB transition**. Its extension `OP_RETURN` clashes with B's one-`OP_RETURN` rule if placed in the checkpoint | n/a | Adds the emulator as a co-signer | Low value for operator risk. A policy-checking RGB delegate (O2 with a validation service as D) gets the same benefit more simply |

### Minimal arkd set per guarantee

| Guarantee | Needs in arkd | Can drop if |
|---|---|---|
| Multi-hop, renewal, swaps, delegate renewal (O2) | **B** | none |
| Asset never lost after 2+ months offline | **nothing** (O1). B for the cheap path (O2) | `recovery-forfeit` (O1 + O2 replace it) |
| Receive onto a new output in one tx; plain RGB wallet accepts after exit | A | the wallet keeps a spare VTXO and the Ark proof is acceptable |
| Batch refresh or collaborative exit of a colored VTXO | C | O2 renewal by checkpoint hops covers renewal; collaborative exit becomes "hop to a fresh VTXO, then unilateral exit" or a swap |
| Offline-verifiable forfeits | upstream #1110 | `forfeit-cosign` is always droppable on master |

## 4. Effect on the 2-month case

| Owner offline | Today (full patch set) | Target (master + B + O1 + O2) |
|---|---|---|
| Less than one period | Owner renews | Same |
| Up to 2 months or more, operator cooperative | Recovery after the sweep, operator decides | Delegate renews every period. The asset never expires and the operator decides nothing about recovery |
| Up to 2 months or more, operator refuses or censors | **Asset lost** unless a watchtower exited in time | Delegate sees the refusal and the towers exit before expiry. The asset is parked on-chain with the owner's CSV leaf, safe indefinitely |
| Delegate offline | n/a | Cannot move the asset. The towers exit |
| Delegate malicious (alone) | n/a | Before T: cannot move it (N2). **After T it can,** because arkd cosigns automatically (N3). Only receivers that run the policy check reject the result. **With a 2-of-3 threshold delegate it cannot** (N3t: arkd rejects one signature at submit, honest co-signers refuse; no lock either). Two colluding delegates can (N3c) |
| Delegate and operator collude | n/a | Can redirect the asset. **This is the residual trust.** Mitigations: CLTV T near expiry, k-of-n delegates, delegate = a policy-checking service |

## 5. Recommendation

| Rank | Do | Why |
|---|---|---|
| 1 | **O2 + O1: delegate leaf renewal backed by keyless multi-watchtowers** | Meets R1 with no arkd change beyond B, and removes the operator as recovery arbiter (a1, a5, a6, b2, b3). Cost: one checkpoint hop per period. The towers make operator refusal cost fees, never the asset |
| 2 | **O11: slim the arkd diff to master + B** (A optional and `PUSH32`-only; C optional; drop `forfeit-cosign` and `recovery-forfeit`) | Smallest diff (5 files, +86/−11), nothing on the penalty path (b1), and no RGB semantics in arkd (b2, b7) |
| 3 | **O8 + O7: first-seen co-attester and public DA** | Turns the collusive double spend from "operator + sender" into "operator + sender + co-attester" for online receives (a2). Removes arkd as the only source of exit data (a3). All client side |

Then O12 (hygiene) as routine work. Reject O4 and O5. Park O9 and O10.

Target configuration:

```
arkd master f863e48 + patches/arkd-checkpoint.patch (B)    # 5 files, +86/−11
[optional] + opret-data-output (A), PUSH32 only            # ~18 lines
client: delegate leaf CLTV(T){D..,S} in receive addresses; RGB delegate (renewal hops);
        N keyless watchtowers; Nostr/Blossom DA; co-attester; Ark-aware balance.
```

Open points before implementing O2 (status after the implementation in section 7):

- An end-to-end run on regtest: delegate-signed intent through `{D,S}`, then the B hop.
- How the wallet discovers addresses with rotating T.
- Whether the delegate holding the stash is acceptable for privacy.
- Fee policy for delegate and towers.
- The interaction with `feat/bounded-offchain-tx-fees` (unmerged; may change the checkpoint template B relies on).

## 6. Verified vs reasoned

| Claim | Basis |
|---|---|
| Delegate leaf without the owner key passes `Validate`; a leaf without the signer fails; `BuildTxs` spends the checkpoint through the delegate leaf | **Verified:** probe on `f863e48` `pkg/ark-lib` |
| `SubmitOffchainTx` accepts Multisig/CLTV/Condition closures and checks CLTV; `intent.Verify` skips signer keys; `CheckSigHashType` limits operator sighash (#58); #1110, #94 and #961 contents and versions | **Verified:** code read and `git merge-base` on upstream tags |
| `recovery-forfeit` has no RGB check (b3); `forfeit-cosign` edits `fraud.go` (b1); A accepts any 0-sat `OP_RETURN` (b6) | **Verified:** `patches/arkd.patch` |
| O2 works end to end: delegate intent, batch, B hop, receiver validation; delegate theft after T accepted by arkd and stock RGB (N3) | **Verified** on regtest, scenario `delegate-renewal` (section 7) |
| Threshold delegate with no arkd change beyond B: k-of-n as C(n,k) `{Di,Dj,S}` leaves; one malicious delegate can neither spend nor lock; k colluding can | **Verified** on regtest, scenario `threshold-delegate` (section 8). arkd's `CHECKSIGADD` k-of-n leaf is refused (n-of-n only): code read |
| The `.arkproof` bundle is enough to unroll without arkd (O7) | **Reasoned:** it carries signed branch PSBTs (`ArkProof.psbts`, `vtxos`); not tested as an exit source |
| O4 breaks uniqueness; O5 is impossible; O8 and O9 trust levels | **Reasoned** from the protocol |
| Proof and fee numbers | From `test-results.md` (measured), extrapolated per period |

## 7. O2 + O1 implemented (`delegate-renewal`)

Design and numbers: `docs/delegate-renewal.md`. Built on arkd master f863e48 + patch B only (`arkd:rgb-cponly`); no other arkd change, no `recovery-forfeit`, no `forfeit-cosign`.

What the implementation showed:

- **Upstream's presigned delegate flow does not work on this stack.** rust-sdk's `prepare_delegate_psbts` / `settle_delegate` signs with `SIGHASH_ALL|ANYONECANPAY` (0x81), and arkd-wallet v0.9.16 refuses it (#58). The delegate instead joins the batch live, signing the intent and `SIGHASH_DEFAULT` forfeits with D (`Client::rgb_settle_inputs`, a client-side rust-sdk hook). Cost: the delegate must be online during each batch, which it is anyway.
- **Every collaborative leaf must include S.** arkd validates the whole taptree, so a `{D}`-only leaf makes the VTXO unspendable off-chain, even for the owner (N1). The delegate leaf is therefore `CLTV(T){D,S}`.
- **Open points from section 5:**
  - End-to-end run: done (smoke run and the 60-day run PASS: 18 renewals, proof +1825 B per renewal; numbers in `delegate-renewal.md` and `test-results.md`).
  - Discovery with rotating T: grid scan over T, 49 addresses for 14 simulated days, 160 for 62.9 days.
  - Privacy: the delegate holds the full stash and links every renewal to one owner (see the privacy section of `delegate-renewal.md`).
  - Fees: zero in regtest; the tower fee came from a presigned ladder (554 sat at 2 sat/vB). The delegate pays two batches per period.
  - `feat/bounded-offchain-tx-fees`: not checked against this flow.

### N3: after T the delegate alone can steal

The leaf says D **and** S, but S is the operator's signer, which cosigns any tx that satisfies the script. In the run, the delegate spent the colored VTXO after T to Mallory's own template: arkd accepted it, and Mallory's stock RGB validation plus the Ark proof returned `Ok(1000)`. Only the receiver-side policy check flagged it.

So the residual trust for O2 is: **in practice the delegate alone, after T**, limited only by receivers that run the policy check (a wallet rule, not consensus). The theft leaves public evidence in the consignment.

### Mitigations that need no arkd change

| Mitigation | Effect | Cost |
|---|---|---|
| Threshold delegate (k-of-n MuSig2/FROST key, or several `CLTV(T){Di,Dj,S}` leaves) | Theft needs k colluding parties; each runs the policy check before signing | Online coordination for 3 signing steps per renewal inside the batch window; more leaves = more discovery and larger exits |
| RGB-level same-owner rule | Receivers reject delegate spends that leave the templates | Wallet rule only: RGB consensus cannot see bitcoin scripts. A custom schema with an owner tag that needs another signature to change turns a theft into a frozen asset (griefing), not possible for NIA/USDt, and brings back the PFA signing problem |
| Short T window | Limits a leaked or stale key; an owner back before T makes D powerless | Does not stop an active malicious delegate; W must cover renewal + batch interval + block-time lag + tower threshold |
| Bond / slashing on proof of theft | Economic deterrent | External contract, locked capital |
| O1 only | No theft path | On-chain exit and fees every period |

## 8. Threshold delegate (`threshold-delegate`, closes N3 down to k colluding delegates)

Design, arkd code citations and numbers: [threshold-delegate.md](threshold-delegate.md). No arkd change beyond B. arkd decodes `MultisigClosure` only as n-of-n (a `CHECKSIGADD` threshold must equal the key count), so 2-of-3 is three leaves `CLTV(T){Di,Dj,S}`, each with S. arkd checks every key of the spent leaf: the ark tx at submit, checkpoints at finalize, forfeits in the batch.

| Finding | Result |
|---|---|
| N3t: one malicious delegate after T | Alone: rejected at submit (`INVALID_SIGNATURE`), the VTXO is not locked. Asking an honest co-signer: refused by its same-owner check, on the outputs and on the RGB seals |
| N3c: 2 of 3 colluding | Accepted by arkd and stock RGB (`Ok(1000)`); policy check flags it. **Residual trust: k colluding delegates** |
| Liveness | 1 of 3 offline: renewals continue; 2 of 3 offline: renewals stop, towers exit, owner recovers on-chain |
| Cost | +744 B proof per renewal (+41 %), 0.65–1.06 s waiting for co-signers per renewal (localhost) |

