# Non-interactive RGB-for-BTC sale on Arkade

Status: research note plus regtest probes (`demo/src/swap_ni.rs`: scenarios `swap-ni` and
`swap-ni-grief <exit|burn|expire>`). Written 2026-11-02, follow-up 2026-11-03. With the `cponly` profile (arkd master f863e48 plus
`patches/arkd-checkpoint.patch`) as the operator.

**Goal.** A maker (seller) publishes an offer such as "400 units of asset X for 30,000 sat" and
goes offline. Any taker (buyer) can fill it alone. Neither the operator nor any colluding parties
may be able to take the asset without paying, or take the payment without delivering. Risks that
only cancel or lock an offer are listed separately from theft risks.

## TL;DR

> **Correction (follow-up, 2026-11-02 late evening).** The first version of this note said B has
> no theft vector. That was wrong for the variant probed first, in which the maker's
> *checkpoint* signature is published in the offer ("B-public"). Arkade's checkpoint output has
> an operator-only path after `CHECKPOINT_EXIT_DELAY`. A published checkpoint signature lets the
> operator (alone), or a taker who has submitted a fill, put V and the checkpoint on-chain. That
> parks V's whole value under an output the maker cannot spend without the operator. This was
> probed and is shown below. The fix that keeps the maker offline is "B-agent": the checkpoint
> signature never leaves a maker-side fill agent, which does submit and finalize in one go.

| | (A) emulator covenant | (B-public) checkpoint sig in the offer | **(B-agent)** checkpoint sig held by a fill agent |
|---|---|---|---|
| Works offchain today | no: the emulator rejects the opret checkpoint layout | yes (probe PASS) | **yes (probe PASS)** |
| arkd / emulator changes | emulator `validateCheckpoint` (~15 lines, unverified) plus a covenant | none | **none** |
| Client changes | covenant builder | 2 small rgb-lib changes (this branch), re-anchor helper | same, plus the agent (a thin service) |
| Operator alone can steal | **yes**: the whole offer VTXO | **yes, on-chain**: it unrolls V, adds its own signature to the published checkpoint, and takes CP:1 via its CSV path | **no** (same guarantee as any Ark VTXO) |
| Taker alone can hurt the maker | no | **yes**: submit, never finalize. V is locked until it is exited (or lost at batch expiry), or the taker puts V and the checkpoint on-chain (value then held under an operator-only path) | **no**: the taker never gets anything it can stall on |
| Who must be online | maker offline | maker offline | maker offline; the agent is online (it holds no funds and cannot move them alone) |
| Expiry | OP_CHECKTIME (operator clock) | none | **the agent stops serving** (plus V's batch expiry) |
| Fill sizes (probe) | n/a | offer 2.1 kB + consignment 6.2 kB; buyer gets consignment 6.2 kB + Ark proof 5.4 kB | same |

**Recommendation: (B-agent).** It needs no arkd or emulator change. Theft needs the agent *and*
the operator to collude, which is the same bar as the delegate in `checkpoint-offline`.
Liveness depends on the agent. (A) fails the collusion criterion outright. (B-public) fails it
too: the operator alone can take the offer VTXO on-chain, and a single taker can lock it.

Ark Labs' `banco` (arkade-os/banco) is a production-style version of (A): the condition lives in
the offer VTXO's leaf, but Bitcoin sees only an operator plus emulator 2-of-2. It has the same
verdict; see the section "Banco (Ark Labs): spending condition embedded in the offer VTXO".

## (B) Design: presigned SINGLE|ACP offer

```
V (maker VTXO, 1000 units)
 └─ checkpoint CP(V)  outputs: [OP_RETURN T1 (0 sat)] [CP:1 → ark tx] [anchor]
      T1: 400 units → WitnessVout(1)  (= CP:1)
          600 units → Explicit(C)    (C = an existing spare VTXO of the maker)
      signed by the maker (DEFAULT). This fixes CP's txid and, with it, T1.
 └─ ark tx (built by the taker)
      in0: taker VTXO                    (taker, DEFAULT)
      in1: CP:1                          (maker, SIGHASH_SINGLE|ANYONECANPAY, in the offer)
      out0: OP_RETURN T2 (1 sat)         T2: CP:1's 400 units → WitnessVout(2)
      out1: maker V+price                ← the only output the maker's signature covers
      out2: carrier (taker)              ← receives the 400 units
      out3: taker change, out4: anchor
```

The offer is `{checkpoint PSBT (maker-signed in B-public; unsigned in B-agent, where only the fill agent holds the signature), template ark PSBT (in1 with the maker's
SINGLE|ACP signature), T2 opret payload, consignment, terms}`.

Why each piece is needed. All of these were found in the probe, not assumed:

- **Witness-vout seals solve the unknown destination.** T1 assigns to "vout 1 of the closing
  tx" (the checkpoint, known to the maker). T2 assigns to "vout 2 of whatever tx spends CP:1".
  The opret commitment does not depend on the txid, so the maker can precompute T2's payload
  without knowing the taker. The taker puts any script it likes at vout 2.
- **The change must not ride in T2.** SINGLE covers only out1, not the opret. A taker could
  write its own T2' that moves all of CP:1's units. So everything the maker keeps must be
  assigned by T1. T1 is fixed by the checkpoint signature: the checkpoint txid commits to its
  OP_RETURN, and the maker's SINGLE|ACP signature commits to the prevout CP:1. The probe sends
  the 600 change to an explicit seal on a spare maker VTXO C. C must stay unspent until the
  offer is filled or cancelled. Alternative: pre-split the asset onto a dedicated offer VTXO
  that holds exactly the units for sale.
- **The opret must be output 0.** RGB `opret1st` takes the *first DBC-compatible output*
  (OP_RETURN or P2TR). With the maker's P2TR payment at out0, validation fails with "first
  DBC-compatible output of witness transaction … doesn't match the provided proof type
  (opret1st)" (first probe run). So the maker's SINGLE pair is at index 1. The template uses a
  placeholder input 0. Under ACP the signature does not commit to the input index or to the
  other inputs.
- **The opret is 1 sat.** On arkd master, an ark-tx `OP_RETURN <32B>` is a sub-dust VTXO script
  and needs a value of at least `vtxoMinOffchainTxAmount` (1) and below dust.
- **The consignment must be re-anchored.** The consignment bundles the full witness tx (the
  template). The taker swaps in the final tx (`pub_witness`). Transitions and commitments are
  unchanged. Probe: the stale consignment on the final tx gives `Invalid consignment`; after
  re-anchoring it validates 400 on final:2. `reanchor()` in `swap_ni.rs` is ~10 lines and would
  belong in rgb-lib.
- **The maker's checkpoint signature has to be re-attached** (by the taker in B-public, by the agent in B-agent). arkd does not store user
  checkpoint signatures. After `SubmitTx`, the taker copies the maker's presigned checkpoint
  signature into the operator-signed checkpoint and signs its own, then calls `FinalizeTx`.
- **arkd #58** (49e20fc6, in v0.9.16 and master): the operator refuses to sign if the PSBT
  input's `sighash_type` field is not DEFAULT/ALL. User signatures are checked with their own
  sighash byte. So the taker leaves the field unset and the maker's 0x83 signature passes. The
  probe confirms both sides of this (5c below).

### Client-side changes made in this branch (`patches/rgb-lib.patch`)

1. `ark_proof.rs`: the Ark-proof verifier rejected any non-ALL signature. Now only the
   **operator's** signature in an operator leaf must be DEFAULT/ALL. That signature is what
   makes the spend operator-acknowledged, and the double-spend/fraud-proof argument rests on
   it. A co-signing user key may use any sighash. Before the change, step 7 failed with "Ark
   proof rejected: sighash type SIGHASH_SINGLE|SIGHASH_ANYONECANPAY does not commit to the
   whole tx".
2. `validate_consignment_offchain`: the explicit-seal lookup now scans every bundle, not just
   the last one. The maker's change is in T1 (first bundle); the last bundle is T2.

There are no arkd, emulator or rust-sdk changes.

### Probe results (regtest, cponly stack in an isolated docker-in-docker, 2026-11-02 ~22:57 CET)

Run `run-1793656630-swap-ni`, `SWAP-NI RESULT: PASS`.

| step | result |
|---|---|
| offer build (maker) | 95 ms; offer.json 2,101 B (protobuf `offer.pb` now: 1,381 B vs 1,938 B JSON for the same content, see `docs/protobuf.md`); offer consignment 6,179 B; offer Ark proof 512 B (V is a batch VTXO) |
| taker pre-check on the template (offline) | 400 units on template vout 2 |
| 5a underpay maker by 1 sat | arkd: `INVALID_SIGNATURE (18): invalid signature in ark tx …` |
| 5b redirect maker output to taker | arkd: `INVALID_SIGNATURE (18) …` |
| 5c PSBT field `sighash_type = SINGLE\|ACP` on the maker input | arkd: `failed to sign ark tx: … input 1: unsupported sighash type 0x83: only SIGHASH_DEFAULT and SIGHASH_ALL are allowed` (#58) |
| stale consignment on the final tx | `Invalid consignment` |
| re-anchored consignment | 400 on final:2 (1 bundle re-anchored) |
| fill (submit + re-attach + finalize) | 290 ms; final ark tx `e4c30622…aaaa`; maker VTXO 130,000 sat (= 100,000 + 30,000) |
| fill proof sizes | consignment 6,241 B; Ark proof 5,446 B (2 witnesses, 5 PSBTs) |
| taker accepts with arkd paused | 400 accepted (arkd `/v1/info` unreachable during validation) |
| maker back online: change | 600 on C |
| taker → Carol hop, Carol validates with arkd paused | 400 received; Ark proof 6,736 B (3 witnesses, 6 PSBTs) |

The first griefing probe (run `run-1793656712-swap-ni`, a plain VTXO submitted and never
finalized) gave `VTXO_ALREADY_SPENT` for 180 s. It is superseded by `swap-ni-grief` below.

Negatives 5a and 5b show the operator rejects a fill that breaks the maker's signature. More
importantly, the signature itself makes such a fill invalid: a colluding operator could not
produce a valid tx either. It holds only its own key, and the collaborative leaf is 2-of-2 (maker, operator). The general
Ark trust assumptions (for example, the operator's timelocked paths once the tree is unrolled
on-chain) are unchanged by this construction and are out of scope here.

### Security of (B): what the follow-up found

**Code facts (arkd master f863e48 with the cponly patch):**

- The checkpoint output is `P2TR{ operator CSV(CHECKPOINT_EXIT_DELAY), V's collaborative leaf
  (maker + operator) }` (`pkg/ark-lib/offchain/tx.go` `buildCheckpointTx`). The operator-only
  CSV path is what protects the operator against a user who publishes a checkpoint and then
  abandons it.
- `SubmitOffchainTx` re-builds the checkpoints and stores them signed by the **operator only**.
  User checkpoint signatures in the submitted PSBTs are dropped (`service.go` ~1120). They reach
  arkd only through `FinalizeOffchainTx`.
- On `SubmitTx`, the inputs become spent (`VTXO_ALREADY_SPENT` for any other spend). There is no
  timeout and no cancel RPC. `GetPendingTx` lets an input owner fetch the pending tx (the
  maker's client lists it).
- `reactToFraud` (`fraud.go`): when a spent VTXO appears on-chain, arkd broadcasts the stored
  checkpoint. It fails if that checkpoint lacks the user's signature. At startup the sweeper
  schedules sweeps of unrolled-and-spent VTXOs' checkpoints (`sweeper.go` ~120).
- An ark tx's nLockTime is derived only from CLTV closures of its inputs (a lower bound). So a
  signature cannot carry an expiry.

**What this means for B-public** (the maker's checkpoint signature is in the offer):

1. *Operator alone* (needs no taker):
   - The operator holds V's fully signed tree branch and its own key. With the published maker
     signature it has a fully signed checkpoint.
   - It unrolls V, broadcasts the checkpoint (V's collaborative leaf has no CSV, so it wins
     against the maker's exit leaf), waits `CHECKPOINT_EXIT_DELAY`, and spends CP:1 alone.
   - The maker loses V's sats, and the units T1 sent to CP:1 are burnt.
   - This was not executed with a malicious operator.
2. *Taker alone*: submit a valid fill and never finalize (G1). Then:
   - V stays locked (no timeout). The maker can neither cancel (G2) nor finalize (G3, the
     taker's checkpoint is unsigned).
   - If nobody acts, V is swept with its batch (`expire` mode).
   - Or the taker puts V and the maker's checkpoint on-chain (`burn` mode). The taker has the
     operator signature from `SubmitTx` and the maker signature from the offer. CP:1 then holds
     V's value under paths only the operator can use alone.
   - The taker can still publish its own checkpoint plus the fully signed ark tx on-chain, which
     would pay the maker, but nothing forces it to.
3. The fill itself cannot be redirected (5a, 5b), and arkd #58 holds (5c).

**B-agent removes both.** The public offer carries the *unsigned* checkpoint. The maker hands
its checkpoint signature only to a fill agent. The fill is one request:

1. The taker sends the ark tx it has signed, plus its own checkpoint, presigned (possible
   because the checkpoint is deterministic).
2. The agent checks that output 1 pays the maker and that the taker's checkpoint is signed.
3. The agent calls `SubmitTx`, merges both user signatures into the operator-signed checkpoints,
   and calls `FinalizeTx`.

Results:

- There is never a pending state the taker controls.
- The operator learns the maker's checkpoint signature only when the agent already holds the
  fully signed ark tx and all checkpoints, which is the normal Arkade guarantee.
- The agent alone cannot steal: the maker's ark-tx signature is SINGLE, so every fill pays the
  maker, and the checkpoint signature is useless without the operator.
- Agent plus operator could collude: the agent leaks the signature and the operator does (1).
- If the agent crashes between submit and finalize, it holds everything needed to finalize
  later.
- The taker gives its checkpoint signature before it sees the operator-signed ark tx. So the
  taker trusts the agent and operator not to collude for the seconds until finalize. That is the
  usual multi-input Arkade exposure.
- Expiry: the agent stops serving the offer, or the maker cancels by spending V once online.

The probe's step 6 now runs B-agent: an `agent` client does `SubmitTx` + `FinalizeTx`, and the
taker presigns its checkpoint before submission.

**Other risks:**

- **Front-running**: anyone, the operator included, can fill first. The maker still gets
  exactly the price.
- **Spare VTXO C**: C must stay unspent until the offer is filled or cancelled, or the change is
  lost. A dedicated, pre-split offer VTXO avoids this. It also bounds what B-public could lose
  to the sold units plus dust.
- **Censorship**: the operator can refuse the maker's cancel, as for any Ark spend.

### Griefing measurements (scenario `swap-ni-grief <exit|burn|expire>`)

| Check | Result |
|---|---|
| G1: taker submits a valid fill, does not finalize | accepted; V `spent=true`, `spent_by` = maker checkpoint, `ark_txid` = the fill |
| Operator-signed maker checkpoint returned by SubmitTx contains the maker's signature? | **no** (arkd re-builds the checkpoints) |
| G2: maker cancels (spends V to itself) | `VTXO_ALREADY_SPENT (6)` |
| G3: maker lists pending txs | sees the stuck fill (`list_pending_offchain_txs`) |
| G3: maker finalizes it itself | `INVALID_SIGNATURE (18): invalid signature in checkpoint tx …` (the taker's checkpoint is unsigned) |
| G2 lock duration (`exit` run, SWAPNI_LOCK_MIN=30) | 30 cancel attempts over 1,827 s (heights 1803 → 1971): every one `VTXO_ALREADY_SPENT`, never released |
| exit: maker unrolls V (branch = 1 tx, CPFP 554 sat) | V confirmed at height 1974; arkd `reactToFraud`: "failed to broadcast checkpoint tx: … missing signature for pubkey <maker>"; V not spent by the checkpoint after 6 blocks |
| exit: maker spends V's exit path (after the 10-block CSV), RGB commitment in that spend | tx `427f078b…1a00`; receiver Gina settles **1000** units (the whole V, T1 never took effect) |
| burn: taker unrolls V (1 tx) and broadcasts the maker checkpoint (maker sig + operator sig) | accepted by bitcoind, confirmed; arkd `reactToFraud`: "failed to finalize checkpoint tx: missing signature for pubkey <maker>" |
| burn: CP:1 (100,000 sat; T1's 400 units) after 48 blocks (`CHECKPOINT_EXIT_DELAY` = 10) | unspent: the honest arkd does not sweep it at runtime |
| expire: nobody acts, mine past V's batch expiry | V `spent=true, swept=true`: the maker's V is lost with its batch |
| expire: taker finalizes after the expiry | `INTERNAL_ERROR: failed to finalize offchain tx: not in a valid stage to finalize offchain tx` |

**Minimal mitigations, in order of preference (none implemented in arkd):**

1. **B-agent** (client side, implemented in the probe): removes taker griefing and
   operator-alone theft.
2. **Dedicated offer VTXO**: pre-split, holding only the sold units plus dust. It bounds any
   loss and removes the C pitfall.
3. **Taker fidelity / rate limiting at the agent.** This is only useful if an agent variant
   submits before the taker's checkpoint signature arrives, so it is not needed with presigned
   taker checkpoints.
4. **Optional arkd change, not done:** an atomic "submit with user-signed checkpoints and
   finalize" RPC.
   - It would remove the pending state for every offchain tx.
   - It does not fix B-public's operator-alone path. That needs the maker's checkpoint signature
     to stay private, which is B-agent.
   - A pending-tx timeout that releases inputs is **not** a safe tiny patch: the operator has
     already signed the ark tx and checkpoints, so releasing inputs would let the taker finish
     the old fill on-chain after V was re-spent.
5. **No expiry via nLockTime or CSV.** Ark txs only take CLTV lower bounds, and a signature
   cannot be made to expire. The expiry is the agent's policy, or V's batch expiry.

### Taker UX (B-agent)

1. Fetch the offer: unsigned checkpoint, template with the maker's SINGLE|ACP signature, T2
   payload, consignment.
2. Validate the consignment and Ark proof offline against the template (400 on vout 2).
3. Build the tx with its own input and outputs.
4. Re-anchor and re-validate.
5. Sign the ark tx and presign its own checkpoint.
6. Send both to the maker's agent, which calls `SubmitTx` and `FinalizeTx` (323 ms in the probe).

That is one request and no contact with the maker. The maker is offline from publication until
it next opens its wallet. There it registers the change on C and sees the paid VTXO.

### Lifecycle after the fill (open points 1-3; run `run-1793662250-swap-ni`, B-agent fill, PASS)

| Step | Result |
|---|---|
| Maker registers its 600 change on C from the fill consignment + Ark proof | `ark_receive_vtxo_transfer` → 600 |
| Batch 2, 240 blocks later: fresh VTXOs Ga (maker), Gb (taker), R (Carol), E (Erin) | boarded |
| Renewals into batch 2 by checkpoint hops: taker final:2 → Gb (400), maker C → Ga (600) | accepted (`checkpoint_hop`, explicit seals) |
| Maker's paid bitcoin (final:1, plain BTC) | `settle_all` into a new batch: commitment `209b7a56…` |
| Batch 1 past its tree expiry | swept (`3b380033…:0`, the bitcoin left on batch 1 by the taker's renewal: `swept true`) |
| Taker Gb → Carol R (400); Carol validates with arkd paused | **400**; Ark proof 8,421 B (8 PSBTs), consignment 7,297 B |
| **Maker spends its change** Ga → Erin E (600); Erin validates with arkd paused | **600**; Ark proof 5,092 B (6 PSBTs), consignment 6,661 B |
| Frank boards (new batch), Carol R → Frank (400); Frank validates offline | **400**; Ark proof 10,106 B (10 PSBTs, 5 Ark witnesses), consignment 7,825 B |

### On-chain fallback (open point 5, analysis only, not probed)

If V was exited, the same offer works as a plain on-chain PSBT, with no operator involved:

- the maker's UTXO U is at input index 1 with SIGHASH_SINGLE|ANYONECANPAY;
- the payment is at output 1;
- the taker's opret is at output 0 (RGB `opret1st`), the carrier at output 2, plus the taker's
  inputs and change.

There is no checkpoint, so nothing fixes the transition from U. The taker writes it, because NIA
transfers need no owner signature. So **U must hold exactly the units for sale** (pre-split): any
change on U could be taken. The maker can still precompute and publish T (U → WitnessVout(2))
as a convenience.

Risks:

- No theft and no lock: U's only spend paths are the maker's key and the presigned pair.
- Front-running: the maker still gets the price.
- Free option until the maker spends U.

The maker gets there either through the exit shown in `swap-ni-grief exit`, which ends with an
on-chain RGB send of V's 1000 units, or with ordinary on-chain RGB.

## (A) Emulator covenant (not probed)

The idea: an offer VTXO whose covenant leaf lets anyone spend it if the ark tx pays X sat to the
maker and carries the right RGB commitment.

- **Trust.** A covenant leaf is a 2-of-2 of the arkd signer and the script-tweaked emulator key.
  Arkade docs: the emulator "is part of the operator, not a separate party". So the operator
  alone can sign that leaf with any tx. That means it can steal the whole offer VTXO, asset and
  sats, not just cancel it. This fails the collusion criterion. Adding the maker's key to the leaf
  removes the theft, but it also removes non-interactivity unless the maker presigns, and that is
  (B) plus an emulator.
- **Feasibility.** The emulator runs scripts on the ark tx and intent proofs, not on checkpoints.
  `validateCheckpoint` (pkg/emulator/tx.go:181-235) requires exactly 2 outputs and an ark input
  spending vout 0. So the opret checkpoint layout `[OP_RETURN, cp, anchor]` is rejected.
  - The minimal emulator patch is in `validateCheckpoint` only: allow an optional leading 0-sat
    `OP_RETURN<32B>`, cp at vout 1, anchor last, and an ark input index equal to the cp index.
    That is ~15 lines and **unverified**. `prevout.go` already uses the outpoint index
    generically.
  - Forfeits (`validateForfeitOutputs`, 2 outputs) matter only for batch renewal.
- **Pinning RGB in the covenant.** T1 lives in the checkpoint, which the script does not see
  directly. In task 1 the engine probe showed that a script on the ark tx can check it from the
  raw checkpoint passed as witness (HASH256 equals the spent txid, and bytes [56:90] are
  `6a20‖C`). Without that, a taker can rewrite T1 and take all the units. T2 is unconstrained
  unless the script also pins the ark-tx opret, but T2 only moves what T1 sent to CP:1.
- **Expiry.** The covenant could add OP_CHECKTIME (on the operator's clock) and so remove the
  free option. That is the only real advantage over (B).

## Banco (Ark Labs): spending condition embedded in the offer VTXO

Question (2026-11-03): could we use a swap whose spending condition lives in the offer VTXO's own
script, as in Ark Labs' `banco`? Read-only review. Nothing was run.

Sources:
- banco: https://github.com/arkade-os/banco at `428ae68` (2026-05-28). Read: `src/offer.ts`,
  `src/taker.ts`, `src/maker.ts`, `test/e2e/banco.test.ts`, `README.md`,
  `docker-compose.emulator.yml`, `.env.regtest`.
- emulator: https://github.com/arkade-os/emulator at `7b830f6` (tag v0.0.9, 2026-10-23, still
  HEAD on 2026-11-03). Read: `internal/application/service.go`, `internal/application/finalize.go`,
  `pkg/emulator/tx.go`, `pkg/emulator/signer.go`, `pkg/arkade/emulator_packet.go`. Its `go.mod`
  pins arkd `f863e48`, the same master we patch.
- SDK: banco depends on a personal fork branch,
  `github:louisinger/wallet-sdk#arkade-script-final`; the lockfile pins `5a24cbf2`. The arkade
  tweak and tapscript files were read at branch HEAD `5207cfe`, so the pinned commit was not
  compared line by line.
- Docs: https://docs.arkadeos.com/learn/concepts/emulator, fetched 2026-11-03.

### How banco works (verified in code unless marked otherwise)

- **The offer.** The offer is a VTXO at a "swap address" plus a TLV record. The TLV can be
  shared as hex or embedded in the funding tx as ARK extension packet type `0x03`. It carries:
  - the price: `wantAmount` / `wantAsset`, and optionally `ratioNum` / `ratioDen` for partial
    fills;
  - `makerPkScript`, `makerPublicKey` and `emulatorPubkey`;
  - optionally `cancelDelay` and `exitTimelock`.
- **The VTXO taptree** (`Offer.vtxoScript`, offer.ts:344-387) has three leaves:
  - *Fulfill*: `MultisigTapscript([serverPubkey, emulatorKey + H("ArkScriptHash", covenant)·G])`.
    On Bitcoin this is a plain 2-of-2 CHECKSIG leaf, with no covenant opcode. The covenant
    (`INSPECTOUTPUTVALUE`, `INSPECTOUTPUTSCRIPTPUBKEY`, `INSPECTOUTASSETLOOKUP`,
    `FINDASSETGROUPBYASSETID`, and for partial fills `INSPECTINPUTVALUE` /
    `INSPECTINPUTSCRIPTPUBKEY`) exists only as Arkade Script. The emulator runs it before it
    signs. The docs say so directly: after a VTXO is unrolled, "Bitcoin does not run the script.
    It checks only the signatures of the path."
  - *Cancel* (optional): `CLTV(cancelDelay)` + `[maker, server]` multisig.
  - *Exit* (optional; `Maker.createOffer` always sets it to arkd's `unilateralExitDelay`):
    `CSV` + `[maker, server]` multisig. Because this "exit" leaf includes the operator key, the
    offer VTXO **has no operator-free exit path**. This contradicts the README's "the maker can
    unilaterally exit on-chain via the CSV exit leaf".
- **Fill** (taker.ts):
  - The taker builds the ark tx with the swap VTXO at input 0. In a full fill, output 0 pays the
    maker and output 1 pays the taker; in a partial fill, output 0 is the rebound swap VTXO,
    output 1 the maker and output 2 the taker. The ARK extension (asset packet and Emulator
    Packet) is the last output.
  - The taker signs its own inputs and checkpoints. Checkpoint 0 (the swap VTXO's) is left to
    the emulator.
  - The taker then sends everything to the emulator's `SubmitTx`.
  - The emulator:
    1. checks the checkpoint shape (`validateCheckpoint`);
    2. runs the script on the **ark tx only**;
    3. signs the ark-tx input *and* checkpoint input 0 with the tweaked key, allowing only
       SIGHASH_DEFAULT or ALL (signer.go);
    4. if it is the "finalizer", calls arkd `SubmitTx` and then `FinalizeTx`, with retries.
  - The code comment says this is done "to prevent griefing attacks where a malicious signer
    retain the tx in the pending state". The retry is in memory: `// TODO: if retry fails,
    persist retry task in background queue`.
- **Who can fill:** anyone who satisfies the covenant, through the emulator.
- **Cancel:** the maker, after the CLTV, with an operator cosign. The e2e tests set
  `cancelDelay` but never call `cancelOffer`, so cancel is untested in the repo.
- **Expiry:** banco does not use `OP_CHECKTIME`. Arkade Script supports it, but on the
  emulator's clock.
- **What the operator can do alone:**
  - The docs say: "The emulator is part of the operator, not a separate party."
  - Whoever holds the emulator secret key can derive the tweaked secret key. Together with the
    operator key, that signs the fulfill leaf for **any** transaction: the whole offer VTXO,
    sats and assets, can be stolen. The tweak only stops the emulator *service* from being
    tricked into signing a different script. It does not bind the key holder.
  - The README's "cannot steal or redirect funds" assumes an honest emulator.
  - The operator can also censor: refuse to cosign, refuse the cancel, refuse the exit leaf.
- **Versions:**
  - The repo's e2e setup uses arkd `v0.9.4` plus `ghcr.io/arkade-os/emulator:v0.0.1`. Banco
    itself needs no arkd change.
  - Emulator v0.0.9 notes "arkd >= v0.9.22 finalizes inside the submit call". Banco has not been
    re-tested against that.
- **Maturity:** npm `@arkade-os/banco` 0.0.1, 16 commits (2026-04-03 to 2026-05-28), no commits
  since. It depends on an SDK fork branch. The e2e tests cover only native Arkade assets (three
  tests: asset/BTC, asset/asset, partial fill). There are no cancel, exit or griefing tests.
  Experimental.

### Fit for RGB

1. **Theft criterion: fails.** Every banco-style condition is enforced by the emulator, and the
   emulator is operator-run, so the operator alone can steal the offer. That is strictly worse
   than B-agent, where theft needs the agent *and* the operator.
   - Running our own emulator with an independent key (the offer carries `emulatorPubkey`; the
     emulator talks to arkd as an ordinary client) would make theft need emulator + operator.
     That is exactly B-agent's trust, with the agent replaced by a script-checking signer.
   - This "own emulator" setup was inferred from the code, not run.
2. **A pure-tapscript embedded condition is not possible.**
   - Tapscript cannot read the spending tx's outputs. CTV and APO are not active.
   - The "fixed signature in the leaf" trick (a script containing a precomputed sig under a
     known key) does not work either. Every taproot sighash, even ANYONECANPAY, commits to the
     spent input's own outpoint. That outpoint exists only after the VTXO carrying the leaf is
     funded, which is circular.
   - So "the output pays the maker X" can be enforced either by an emulator signature (banco)
     or by a maker signature made after funding (B: SINGLE|ACP on the ark tx). In B, that ark-tx
     signature is safe to publish. The dangerous one was the checkpoint signature (see
     B-public).
3. **Changes needed for RGB on top of our cponly patch** (emulator side, unverified):
   - The emulator rejects our checkpoint layout. `validateCheckpoint` (tx.go:181-235, unchanged
     in v0.9) requires exactly `[cp, anchor]` and an ark input spending vout 0. The task-1 engine
     probe reproduced the rejection.
   - It needs an emulator patch (about 15 lines, unverified) and an RGB-specific covenant. The
     covenant would pin:
     - the checkpoint's T1 opret, through the raw checkpoint in the witness;
     - the ark tx's output 0 = `OP_RETURN <C2>`;
     - output 1 = maker, at least X sat.
   - The engine pieces passed in task 1 (`docs/arkade-script.md`).
   - Because T1/T2 use witness-vout seals, the maker can precompute C1 and C2.
   - An ark tx would then carry two OP_RETURNs. RGB takes the first (vout 0). The emulator finds
     its packet by the `ARK` magic (`extension.IsExtension`), so it would skip the RGB opret.
     That is inferred and was not run.
4. **Griefing:** better than B-public. The taker cannot get the emulator signature without the
   emulator submitting and finalizing in one call, so a taker cannot leave a fill pending.
   - What remains: an emulator crash between submit and finalize leaves the fill pending, and
     the retry is not persisted. The VTXO then needs an exit, and banco's exit leaf needs the
     operator. This is inferred from the code, not tested.
   - CP:1 again sits under the operator CSV leaf plus the fulfill leaf (operator + emulator), as
     in our B findings.
5. **What it would add over B-agent:** on-chain-visible offer terms, discovery by txid, partial
   fills, and an expiry via `OP_CHECKTIME`. None of these change the trust model.

**Conclusion.** A banco-style embedded condition is emulator-enforced. The leaf Bitcoin sees is
a 2-of-2 of operator and emulator. It therefore does not make the RGB sale non-interactive
without a trusted signer: it only moves the trust from "holder of the maker's checkpoint
signature" (B-agent) to "emulator key holder". With the stock operator-run emulator, that is the
operator alone, which fails our criterion. It would also need an emulator patch on top of arkd
cponly. Recommendation unchanged: **B-agent**. Optionally, the fill agent could later be built
as a self-hosted emulator with an RGB covenant, to get public rules and partial fills. No probe
was run, because it needs a patched emulator image plus the SDK fork, which is not cheap.

## Regressions after the rgb-lib.patch changes

All times are CET, 2026-11-03. Every regression run uses the committed `patches/rgb-lib.patch`.

| Profile | Scenario | Result |
|---|---|---|
| cponly (arkd f863e48 + checkpoint patch) | checkpoint-renew | PASS (49 s), 00:22 |
| cponly | checkpoint-swap | PASS (44 s) |
| cponly | checkpoint-offline | PASS (61 s), 00:25 |
| cponly | swap-ni (B-agent, full lifecycle) | PASS (91 s), 00:30-00:32 |
| cponly | swap-ni-grief exit / burn / expire | PASS (findings above) |
| fast (arkd:rgb v0.9.15 + arkd.patch) | run-all-quick: basic, basic-onchain-genesis, offchain-transfer (x2), refresh, collab-exit, forfeit-negatives, multi-user, failed-batch, fraud, multihop-exit | 11/11 PASS, `failed=0`, 918 s (00:33:51-00:49:09) |


## Unverified / open

- **Operator-alone theft on B-public was shown in parts, not end to end.**
  - Shown: the checkpoint can go on-chain with the published maker signature (`burn` mode).
  - Not shown: an operator actually spending CP:1, which would need a modified operator or its
    key.
  - The claim also rests on the checkpoint script tree in `buildCheckpointTx`.
- B-agent's agent is a probe stand-in (an `ArkClient` network client in the same process). No
  agent service, auth or rate limiting exists.
- How long arkd keeps an unfinalized submission:
  - measured: >30 min (1,827 s), and gone only with the batch (`expire` mode);
  - from the code: no timeout and no cancel RPC.
  - Not measured: a tx stuck past expiry and then restarted.
- The taker's on-chain completion after `burn` (its own checkpoint + the fully signed ark tx,
  which would pay the maker) was not run.
- The on-chain fallback (point 5) is analysis only.
- (A) is still unprobed; the emulator patch is unverified.
- The `alice_change_on_c` result relies on rgb-lib's explicit-seal lookup scanning all bundles
  (this branch's change); registration on C uses the existing `ark_receive_vtxo_transfer`.
