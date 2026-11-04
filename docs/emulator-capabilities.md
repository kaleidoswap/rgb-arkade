# What the Arkade emulator can enforce for RGB assets

Status: exploration on branch `emu-explore`. Stack: arkd cponly (master f863e48 + checkpoint
OP_RETURN patch), emulator:rgb (v0.0.9 7b830f6 + patches/emulator.patch, with the durable
finalize). Trust assumption accepted for this exploration: **the operator and the emulator are
trusted to run the covenant** (an operator that co-signs without the emulator, or an emulator that
signs anything, breaks every guarantee below; RGB client validation still protects the asset
*history*, never the *economic* condition).

Every claim is marked **[verified]** (exercised by a test in this repo, with the test named) or
**[inferred]** (read from source, not exercised).

## 1. The model

The emulator sees only Bitcoin transactions: the ark tx it co-signs, the checkpoints, the
previous ark txs (PSBT field `prevarktx`), the extension packet, and for intents the intent
proof + message. It sees RGB only as 32-byte opret commitments (checkpoint output 0 = T1, ark tx
output 0 = T2). It cannot parse an RGB transition, so it can never check *what* a commitment
moves, only *which* precommitted commitment is used, and the Bitcoin side around it.

So the building block is: **the asset owner precomputes every RGB transition the contract may
need, and the covenant only allows spends whose commitments are one of those, with Bitcoin
outputs that make the RGB result safe** (seals on pinned outputs, payment present, time window).

### 1.1 Precommitment without circularity  [verified: emu-lots]

swap-emu needed a maker signature over (C1, C2, V) because V received the asset with an explicit
seal on V's outpoint (jump-in): T1 then depends on V's outpoint, V's address on the covenant, the
covenant on C1. Funding V with a **witness-vout seal** instead (the asset moves into V in the
same ark tx that creates V: T1a in the sender's checkpoint, T2f at the ark tx's output 0, seal =
vout of V) breaks the cycle: T2f's opid does not depend on V's address, so every later
transition can be computed before V's address is known, and the Merkle root of the outcomes is a
**constant in the covenant**. No signature, the owner goes offline right after funding.

Commitments do not depend on txids, so everything is computed on placeholder txs; the real txs
carry the same OP_RETURNs and the receiver re-anchors the precomputed consignment
(`reanchor_all`) to the real txs, which it fetches from arkd (virtual txs).

rgb-lib (`color_psbt_and_consume_custom`) accepts building two alternative transitions from the
same allocation (fill and reclaim of one level), so branching precommitments need no wallet fork
[verified: emu-lots precompute].

### 1.2 The generic outcome covenant  [verified: go test TestOutcomeCovenant, emu-lots]

`tools/emucov/outcome.go`, `OutcomeCovenant(root)`, 426 bytes. Witness (bottom to top):
`stmt, authSig, proof, leaf, rawCheckpoint`. A leaf (232 bytes) is one outcome:

| field | check by the emulator |
|---|---|
| C1 | the spent input is `rawCheckpoint:1` (HASH256) and its output 0 is `OP_RETURN C1` |
| C2 | ark output 0 is `OP_RETURN C2` |
| payKey, payMin | output 1 is P2TR(payKey) with value >= payMin |
| rgbKey | output 2 (the RGB witness-vout seal of T2) is P2TR(rgbKey) |
| nextKey, nextMin | output 3 is P2TR(nextKey) (next covenant VTXO, precomputed address) with value >= nextMin |
| notBefore / notAfter | emulator clock (`OP_CHECKTIME`, the emulator host's `time.Now()`) |
| authKey | `CHECKSIGFROMSTACK(authSig, SHA256(leaf ‖ V outpoint), authKey)` (bound to this VTXO, no replay) |
| priceBelow (oracle mode) | `CHECKSIGFROMSTACK(authSig, SHA256(stmt), authKey)`, stmt = price(8 LE) ‖ ts(8 LE), price < priceBelow, now < ts + 3600 |
| (all) | `OP_MERKLEBRANCHROOT(leaf, proof)` == root (tags `ArkRGB/outcome`, `ArkRGB/branch`) |

A zero key / zero number disables that check. The rgbKey pin matters: a witness-vout seal goes
to whoever builds the tx, so any outcome whose RGB beneficiary is not the tx builder (reclaim,
escrow release, liquidation) must pin output 2.

### 1.3 Capability map

| capability | how | status |
|---|---|---|
| fixed precommitted transitions | C1/C2 pinned via leaf; root constant | [verified] |
| choose 1 of N transitions | MERKLEBRANCHROOT over leaves | [verified] |
| payment to a party | INSPECTOUTPUTSCRIPTPUBKEY / INSPECTOUTPUTVALUE | [verified] |
| pin the RGB beneficiary | output 2 spk = rgbKey | [verified] |
| recursive offers (change goes to a new covenant) | output 3 spk = precomputed next covenant address; finite chain unrolled at offer time | [verified] |
| true recursion (same script, computed address) | OP_TWEAKVERIFY + SHA256 streaming could compute a taproot output key in script | [inferred, not attempted] |
| absolute time (wall clock) | OP_CHECKTIME vs the emulator's clock (not the chain) | [verified] |
| block-height locks | CLTV against the tx locktime (arkd enforces the locktime against its chain view) | [verified in swap-emu cancel leaf, not in Arkade Script] |
| VTXO batch expiry | OP_PUSHEXPIRY (needs the VTXO expiry in OffchainData) | [inferred] |
| third-party signature (arbiter, co-signer) | CHECKSIGFROMSTACK over SHA256(leaf ‖ V) | [verified] |
| oracle price statement | CSFS over stmt + numeric compare + freshness | [verified: emu-lending] |
| multi-input atomicity (two covenants in one tx) | INSPECTINPUTOUTPOINT / INSPECTINPUTSCRIPTPUBKEY of the other input | [inferred] |
| Arkade assets (transparent, arkd-validated) | INSPECTOUTASSETLOOKUP on the asset packet (asset packet + emulator packet in one extension) | [verified: emu-asset-swap] |
| batch renewal of a covenant VTXO (Bitcoin level) | SubmitIntent runs Arkade Script on the intent proof; SubmitFinalization signs forfeits | [verified: upstream TestCounterContractBatchContinuation on our stack]; for RGB the forfeit path loses the asset, live two-hop migration keeps it [verified: emu-renew], see 2.5 |
| multi-party unilateral exit (k-of-n as N-of-N CSV leaves, presigned nLockTime default) | CSVMultisig exit closures | [verified on-chain: emu-exit] |
| check the *content* of an RGB transition | not possible in practice (would need RGB strict/commit encoding re-implemented in script) | impossible (practical) |

## 2. Scenarios

Results in section 3 (numbers from the runs).

### 2.1 Offer in fixed lots, with expiry (emu-lots)

Levels V_N..V_1 (1000 units, lots of 300: 300/300/300/100). Each V_k has two outcomes:
- fill_k: pay >= PRICE to the maker (output 1), lot to the taker's output 2 (taker's choice),
  the rest to V_{k-1} (output 3, pinned address, >= 2000 sat), only while now < expiry;
- reclaim_k: everything to the maker (output 2 pinned), only when now >= expiry.

T1_k (V_k -> CP:1, all units) is shared by both outcomes; T2fill_k and T2rec_k are alternatives
from the same CP:1 allocation.

### 2.2 RGB-for-RGB swap (A against B), non-interactive

**Can the covenant check the taker's B transfer? No.** [inferred, from the model]
- The emulator sees B only as a 32-byte commitment; it cannot tell which contract, how many units
  or to which seal. Checking it would mean re-implementing RGB's commitment encoding (strict
  encoding, operation ids, bundle ids, MPC tree with entropy, seal concealment) in Arkade Script
  and passing the transition in the witness. With OP_SHA256*/OP_CAT/OP_SUBSTR this is not
  impossible in principle, but it is far beyond practical (and fragile across RGB versions).
- The maker cannot precommit B either: B's transition spends the taker's allocation, which the
  maker does not know when the offer is published.
- With OpretFirst, an ark tx carries ONE commitment (output 0), an MPC over all contracts. An
  A+B swap in one tx needs `C2 = MPC{T2_A, T2_B}`, which needs both transitions, so it cannot be
  a constant in the maker's leaf.

Best achievable versions (not run, [inferred]):
1. *Taker-first counter-covenant + one maker signature (RFQ).* The taker locks B in W with
   outcomes "B -> maker's existing VTXO M (explicit seal in W's checkpoint, so the ark tx's
   output 0 only needs T2_A)" and "refund after T". The maker validates W's precommitted T1_B
   offline (client-side RGB validation: right asset, right amount, seal = M), then signs one
   message binding (W outpoint, C1_B); the maker's covenant needs one extra check, "input k is
   W and its checkpoint carries C1_B" (INSPECTINPUTOUTPOINT + the raw checkpoint of input k in
   the witness, as for the own input). After that signature, either side can complete alone and
   the swap is atomic (one ark tx). This is one message from the maker, not zero.
2. Fully offline maker: impossible with this emulator; the RGB semantics of B are checked only
   by the maker's own client validation, after the fact.

### 2.3 Escrow with arbiter (emu-escrow)

The buyer locks 1000 units (payment) in two escrow VTXOs E1 (600) and E2 (400) from one funding
tx. There are five outcomes per escrow, and every one pins the RGB beneficiary on output 2:
- release to the seller, signed by the buyer or by the arbiter (2 leaves);
- refund to the buyer, signed by the seller or by the arbiter (2 leaves);
- refund to the buyer after the timeout, with no signature.

The signatures are CSFS over SHA256(leaf ‖ escrow outpoint). This is "2-of-3" in the sense of
"the counterparty or the arbiter". The submitter can be anyone, because the outputs are pinned.

### 2.4 Vault (emu-vault)

Arkade Script has no relative timelock between two offchain txs. The emulator clock is absolute,
and we did not find a way to make CSV measure the time between two preconfirmed ark txs
[inferred: not tested; the delay would have to be counted against a confirmation that never
happens offchain]. The vault therefore uses a **slot ladder**:
- the hot key may unvault X only into the pending VTXO U_i, and only during time slot i
  (notBefore/notAfter on X's leaf);
- U_i's only non-clawback outcome sends the asset to the precommitted destination D, and only at
  `end(slot i) + DELAY`;
- the cold key can claw back from X or from any U_i at any time.

The delay is therefore relative to the unvault, quantized by the slot length. The precompute cost
is linear in the number of slots: per vault, 2 + 4·slots transitions (38 for 2 vaults × 4 slots
in the run). The ladder covers a finite window; after the last slot only the clawback remains.

### 2.5 Lending (emu-lending) and batch renewal

The borrower locks the collateral in loans L1..L3. The outcomes are:
- repay: output 1 pays at least REPAY to the lender, the collateral goes to the borrower (pinned),
  before maturity;
- liquidate: an oracle statement (price, ts) signed by the oracle key, with price < threshold and
  a fresh statement; the collateral goes to the lender (pinned);
- default: after maturity, the collateral goes to the lender (pinned).

The loan principal itself is not part of the covenant (the lender pays it out of band, or in the
same funding tx); only the collateral and the repayment are enforced.

**Renewal (critical).** A covenant VTXO lives in a batch and expires with it.

*Bitcoin level [verified]:* the upstream emulator test `TestCounterContractBatchContinuation`
passes on our cponly stack (12.8 s). The emulator runs Arkade Script on the intent proof
(SubmitIntent), signs the forfeit (SubmitFinalization), and arkd carries the extension packet into
the new batch leaf. Anyone can submit; no contract party is needed. (The test hardcodes the
explorer as `http://localhost:3000`; our mempool explorer serves the esplora API under `/api`, so
we ran a copy with `localhost:3000/api`; upstream code unchanged.)

*RGB through the forfeit path (emu-renew, part A) [verified with a plain RGB VTXO]:*
- A forfeit carrying an RGB commitment (the arkd:rgb lane design) is **rejected by cponly arkd**:
  `INVALID_FORFEIT_TXS: invalid forfeit tx: expected txid …, got …`. arkd rebuilds the forfeit
  deterministically and accepts only that exact txid, so no commitment can be added.
- A plain refresh succeeds. The forfeit (inputs: connector + the old VTXO; outputs: 100330 sat to
  the operator, P2A anchor) has **no OP_RETURN**. It is the only tx that can ever spend the old
  VTXO, so the asset's seal can only be closed without a commitment: **the asset is lost**.
- Afterwards the old VTXO cannot be spent offchain (`no matching VTXO outpoints`), and after the
  batch expiry it is swept.
- Composition [inferred]: an intent-based renewal of a *covenant* VTXO uses the same forfeit
  (verified at Bitcoin level above), so it loses the RGB asset the same way. Our outcome covenant
  also never authorizes an intent: it requires a checkpoint carrying C1, which an intent proof does
  not have.

*Live migration across batches (emu-renew, part B) [verified]:*
- The loan VTXO V (batch 1) has three tapleaves:
  - fill: server + emulator;
  - collab: `server, Bob, Lena` (N-of-N, an arkd MultisigClosure);
  - exit: CSV 2-of-2 `Bob, Lena`.
- G is a `{server,Bob,Lena}` collab + `{Bob,Lena}` exit VTXO. It is funded offchain from Lena's
  batch-2 VTXO, 120 blocks after V, so it carries batch 2's expiry.
- **Hop 1, V -> G:** an ark tx through the collab leaf. Its checkpoint commits T_r, with an
  explicit seal on G, computed at renewal time as one more alternative from V's allocation. Bob
  and Lena sign; no emulator is involved. Submit + finalize took 14 ms; T_r took 103 ms; the
  consignment is 6542 B. Bob validates 1000 units on G.
- **Hop 2, G -> V':** a fresh covenant whose whole tree is recomputed (precompute 672 ms). The
  asset enters V' with a witness-vout seal (1.1), so the root is again a constant. Submit +
  finalize took 15 ms. V' carries G's expiry.
- **Batch 1 expiry:** at height 753 (past batch 1's 500-block expiry) the old chain is swept,
  while V' is neither spent nor swept.
- **The contract continues on V':** an underpaid repay is refused (`OP_VERIFY`). Bob's repay,
  paid with fresh batch-3 BTC, is co-signed in 55 ms. Bob validates **1000 units** with the full
  history (genesis -> jump-in -> fund -> V -> T_r -> G -> fund' -> V' -> repay): 8 bundles,
  9699 B, 195 ms.
- **Who must be online, and which signatures:** both parties, for both hops. That is 4 Schnorr
  signatures per hop (each party signs the checkpoint and the ark tx), plus the operator's. The
  emulator is not involved. A renewer trusted by both (one MuSig2 key in the collab leaf) could
  replace them [inferred].
- **If a party disappears:**
  - Renewal is impossible. The contract must then finish through the emulator before batch
    expiry: default and liquidation need only the lender, and repay needs only the borrower.
  - Or a party leaves through the exit path, which for lending needs the presigned default (2.8).
  - Note the vtxo indexer's `expires_at` is a timestamp estimate (V 1793715970 vs G 1793715990,
    because regtest blocks are instant). The evidence is the sweep status at a block height, not
    that field.
- An offchain hop to a witness-vout output alone does not renew anything: offchain outputs
  inherit the earliest expiry of their inputs. That is why G must be a *separate* VTXO from a newer
  batch.
- Without a renewal, **the contract must end before the batch expires** (OP_PUSHEXPIRY could
  make that a leaf condition [inferred]).

### 2.6 RGB for Arkade asset (emu-asset-swap)

Alice offers 1000 RGB units for 1500 units of an Arkade asset X. `OutcomeCovenantAsset` (468 B)
is the outcome covenant with this prefix:

`1 <X txid> <gidx> OP_INSPECTOUTASSETLOOKUP VERIFY <min> GREATERTHANOREQUAL VERIFY`

It requires output 1 to carry at least 1500 X according to the asset packet. arkd validates the
asset packet (conservation, issuance), so the emulator only reads it. One extension output carries
both packets: the asset packet (type 0x00) and the emulator packet. Carol pays X and receives the
RGB lot in one ark tx, atomically, with no signature from Alice. The asset id is passed to the
script as the txid in internal byte order plus the group index [verified].

This is the strongest result: unlike RGB-for-RGB, **the counter-asset is transparent to the
emulator**, so a fully non-interactive cross-protocol swap works.

### 2.8 Unilateral exit paths (emu-exit, on-chain) [verified]

The emulator guards only the fill leaf. Every other tapleaf of the covenant VTXO must also respect
the contract. arkd accepts only CSVMultisig / ConditionCSVMultisig exit closures (CLTV and CSV are
forbidden inside conditions), so the exits are **CSV N-of-N leaves, one per allowed key set**.
`CovVtxo::new_full` builds them. Offers keep the single-owner exit (`CovVtxo::new`).

| contract | exit leaves (each CSV + all listed keys) |
|---|---|
| offer (maker = owner) | {maker} |
| escrow | {buyer, seller}, {buyer, arbiter}, {seller, arbiter}: 2-of-3 as three 2-of-2 leaves (71 B each) |
| lending | {borrower, lender}, plus a default tx that the borrower presigns at funding time, with nLockTime = maturity, spending that leaf to the lender with the precomputed T1 |

emu-exit funds three contracts in one tx, unrolls it on-chain (3 txs, CPFP via anchors, 6 s), waits
the CSV (10 blocks), then:
- **Legacy single-key leaf, the bypass is real:** Bob alone spends X_old with the precomputed T1
  and validates **300 units** on-chain, without any covenant outcome (180 vB, consignment 6888 B).
- **Bob alone on the multi-party leaves is refused by consensus:**
  - escrow leaf {Bob, Sally} with only Bob's signature: refused ("false top stack");
  - escrow leaf {Bob, Ari}: refused, same;
  - loan leaf {Bob, Lena}: refused, same;
  - loan leaf with Bob signing both slots: refused ("Invalid Schnorr signature").
- **Recovery without the operator and the emulator:**
  - Bob + Ari (an arbiter-backed refund) take X_esc: 300 units, 213 vB, consignment 7022 B.
  - Lena alone completes Bob's presigned default. It is refused before the locktime
    (`non-final`) and accepted at height 151: 400 units to Lena, 205 vB, consignment 6990 B.
  - Bob + Ari before the CSV: `non-BIP68-final`.
- emu-escrow and emu-lending rerun with these leaf sets and PASS (arkd accepts the tapscripts on
  every covenant spend).

Liveness. These paths are needed only if the operator or the emulator fails; otherwise every
outcome stays available through the emulator.

| who disappears | escrow | lending |
|---|---|---|
| buyer / borrower | seller + arbiter exit; emulator outcomes (arbiter release) | lender: presigned default after maturity (on-chain) or emulator default/liquidation |
| seller | buyer + arbiter exit; emulator outcomes (arbiter refund, timeout) | – |
| arbiter | buyer + seller exit; emulator timeout refund | – |
| lender | – | the borrower **cannot** exit alone (by design: that would be the bypass). The borrower repays through the emulator (the payment goes to the lender's key whether or not the lender is online). On-chain alone the borrower has no path [verified: refused]. |

Every party keeps a path, except a borrower facing an absent lender *and* a failed emulator.
Covering that would need a repay tx presigned by the lender with SIGHASH_ALL|ANYONECANPAY. That
signature commits to the outputs (T1 commitment, collateral to the borrower, payment to the
lender), and the borrower adds the input that funds the payment. Possible in principle; not built
[inferred].

### 2.7 Failure modes common to all scenarios

| failure | effect | status |
|---|---|---|
| operator co-signs without the emulator, or the emulator signs anything | every covenant guarantee is gone; RGB validation still rejects invalid histories | by assumption (trusted) |
| emulator down | no covenant spend at all; funds leave via the exit leaves (2.8) | [verified: emu-exit, on-chain without operator/emulator] |
| exit leaf = CSV + one owner key | the owner unrolls and takes the asset with the precomputed T1, bypassing the covenant | [verified: emu-exit legacy leaf, 300 units]. **Fixed** for escrow/lending with multi-party exit leaves (2.8); kept for offers (maker = owner) |
| griefing a fill | an invalid fill is refused by the emulator without locking V (V `spent=false` after every negative) | [verified] |
| race between two takers | the first co-signed fill wins; the second fails as a double spend | [inferred] |
| precommitted consignment lost | the receiver needs the precomputed consignment; it is published with the offer | design |
| wall-clock skew | OP_CHECKTIME uses the emulator host's time, not the chain | [verified that it is time.Now()] |
| batch expiry | forfeit path loses the asset; live two-hop migration keeps it (2.5) | [verified: emu-renew] |

## 3. Results (regtest, cponly stack, 2026-11-03, times CET)

All scenarios PASS. Every negative is refused by the emulator with the expected script
error (not by an unrelated failure); the error is recorded in each run's `summary.json`.

| scenario | positive | negatives refused (error) | emulator co-sign | sizes |
|---|---|---|---|---|
| emu-lots (14:25–14:29) | 2 partial fills of 300; reclaim of 400 after expiry | underpay (VERIFY), rest redirected (EQUALVERIFY), sats drained (VERIFY), reclaim leaf by taker (EQUALVERIFY), reclaim before expiry (VERIFY), fill after expiry (VERIFY) | fill 80–93 ms incl. VTXO visible; reclaim 41 ms | covenant 426 B; 8 leaves × 232 B, proofs 32 B; fill ark tx 1196 vB, extension 865 B; consignments 8170 / 10248 / 12163 B (validate 162–171 ms); largest precomputed 10683 B; precompute 1.34 s |
| emu-escrow | arbiter release 600 units to the seller; timeout refund 400 to the buyer | no sig (VERIFY), seller-signed release, arbiter sig for the other escrow, buyer-signed refund (CSFS fail), redirect (EQUALVERIFY), early timeout (VERIFY) | 30 / 47 ms | proof 96 B (5 leaves); consignment 8204 B; precompute 1.33 s; exit leaves 3 × CSV 2-of-2 |
| emu-lending | L1 repay (lender paid 25000 sat), L2 oracle liquidation (price 42000), L3 default after maturity | underpaid, collateral to lender on repay, price above threshold, forged oracle, stale statement, default before maturity, repay after maturity | 38 / 32 / 36 ms | precompute 1.71 s; exit leaf CSV 2-of-2 {borrower, lender} |
| emu-vault | X1 unvault + clawback from U; X2 unvault + withdraw to D after the delay | unvault signed by cold key, next-slot leaf, unvault to another U, clawback by hot key, withdraw before delay | 34–43 ms | 38 transitions precompute 6.47 s; X proof 96 B, U proof 32 B; consignment 10259 B |
| emu-asset-swap | Carol: 1500 X -> Alice, 1000 RGB units -> Carol, one ark tx | X underpaid (VERIFY), all X to the change (VERIFY), no asset packet ("no asset packet") | 37–40 ms | covenant 468 B; fill ark tx 1218 vB, extension 930 B |
| emu-exit (on-chain) | legacy bypass (Bob alone, 300); Bob+Ari exit (300); Lena presigned default after locktime (400) | Bob alone on 2-of-2 leaves ×3 (false stack), Bob in both slots (invalid Schnorr), before CSV (non-BIP68-final), default before locktime (non-final) | – (no emulator) | unroll 3 txs in 6 s; exit txs 180 / 205 / 213 vB; consignments 6.9–7.0 KB; run 39 s |
| emu-renew A (forfeit path) | plain refresh OK | forfeit with RGB commitment rejected (INVALID_FORFEIT_TXS txid mismatch); hop from forfeited VTXO refused | – | forfeit has no OP_RETURN |
| emu-renew B (migration) | V -> G -> V' across batches; batch 1 swept; repay on V' 1000 units | underpaid repay on V' (VERIFY) | repay 55 ms; hops 14 / 15 ms (no emulator) | T_r 6542 B; recompute V' tree 672 ms; full-history consignment 9699 B, 8 bundles, 195 ms; run 90 s |
| upstream TestCounterContractBatchContinuation | covenant VTXO renewed through a batch | – | – | – |

Offer data is measured as raw bytes (leaf, proof, consignment). We defined no protobuf message for
these offers, so the protobuf framing would add only a few bytes per field.

Final regression run on a fresh cponly stack (driver 15:35:45 CET, `up-cponly` 83 s), all PASS:
checkpoint-renew 50 s, swap-ni 96 s,
checkpoint-swap 40 s, checkpoint-offline 59 s, swap-emu 78 s, emu-escrow 174 s, emu-lending 175 s,
emu-vault 406 s, emu-asset-swap 29 s, emu-exit 35 s, emu-renew 87 s, emu-lots 237 s (rerun at
16:05; the first try failed after fill 1 with an RGB resolver error "transaction can't be located"
for the jump-in tx on the new stack, an indexer delay, not a script failure). Fill/reclaim timings in this run: emu-lots 67 / 62 / 49 ms,
emu-renew repay 38 ms, hops 15 / 16 ms, V' recompute 722 ms.

## 4. What is impossible, and why

- **The emulator cannot check an RGB transfer's content** (contract, amount, seal). It sees a
  32-byte commitment. RGB-for-RGB therefore needs one maker message (2.2), or trust in the maker's
  after-the-fact validation.
- **A covenant cannot renew itself while it carries RGB.** The intent/forfeit path closes the seal
  without a commitment (cponly arkd rejects a committed forfeit), and a renewal hop needs a seal
  unknown at offer time. Renewal is possible only with live parties (two hops, 2.5) [verified]. With change C (forfeit
  data OP_RETURN, `arkd:rgb-master`) the first step can go through a batch: see
  [arkd-master-lane.md](arkd-master-lane.md).
- **There is no relative delay between offchain txs.** It can only be approximated with a slot
  ladder (2.4).
- **There are no unbounded recursive offers with constant precomputation.** Every level must be
  precomputed. A self-computing covenant address (OP_TWEAKVERIFY) is conceivable, but the RGB
  transitions still have to be precomputed per level.
- **The emulator does not protect the other tapleaves.** A single-key exit leaf is a real bypass
  [verified]. Exit leaves must be multi-party (2.8).
- **No unilateral on-chain timeout leaf.** arkd forbids CLTV in exit closures; a presigned
  nLockTime tx replaces it (2.8).

## 5. Changes needed

- arkd: **none** beyond the cponly checkpoint OP_RETURN patch already in use, for the scenarios
  themselves.
- emulator: **none** beyond the existing `patches/emulator.patch` (durable finalize).
- rgb-lib: **none** (stock `color_psbt_and_consume_custom`, `fungible_all`).
- This repo: `tools/emucov` (outcome covenant, asset variant, tree, extension packing) and
  `demo/src/emu_explore.rs`.
- Done in this repo: multi-party exit leaves + optional collab leaf (`CovVtxo::new_full`,
  `csv_multisig_script`, `nofn_script`); escrow/lending use them; scenarios `emu-exit`,
  `emu-renew`. Still no arkd / emulator / rgb-lib change.

## 6. Not verified

- Forfeit-path loss for a *covenant* VTXO renewed by intent: shown with a plain RGB VTXO + the
  upstream covenant renewal test; the combination was not run (2.5).
- Renewal through one MuSig2 renewer key instead of two signers; a lender-presigned repay path.
- RGB-for-RGB counter-covenant (2.2): not implemented.
- OP_PUSHEXPIRY deadlines, multi-input checks, race between takers: inferred.
- The emulator clock against chain time: no skew test.
